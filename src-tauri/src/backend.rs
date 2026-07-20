// SPDX-License-Identifier: MIT OR Apache-2.0

//! The live backend: the p2panda node, the stream task, and the bridge to the UI.
//!
//! This is the only module that knows about both p2panda and Tauri. [`Session`] holds the rules,
//! [`crate::commands`] holds the API surface, and this holds the machinery that keeps them running.
//!
//! ## Lifecycle
//!
//! [`Backend::start`] runs once, from Tauri's `setup` hook, on the async runtime:
//!
//! ```text
//!   load or mint the identity (OS keyring)
//!   -> decide which server this is (stored, or founded from our own key)
//!   -> spawn the p2panda node
//!   -> open the topic stream from the very start of the log
//!   -> found the server, if this is first run
//!   -> spawn the stream task, which runs for the life of the app
//! ```
//!
//! The `Node` is **owned by [`Backend`], which is owned by Tauri's managed state**, so it lives
//! exactly as long as the app does. p2panda 0.7 has no `shutdown` — dropping the node *is* the
//! shutdown — so this ownership is not a detail, it is the whole lifecycle contract. Storing it
//! anywhere shorter-lived (a local in `setup`, say) would silently tear the network down the moment
//! that scope ended.
//!
//! Startup is deliberately **not** blocking: `setup` returns immediately and the work happens on a
//! task, so the window paints while the keyring, the database and the network come up. Commands
//! issued before it finishes get a clear "still starting" error rather than a deadlock, and the UI
//! is told it can proceed by a `chat:ready` event.
//!
//! ## Locking
//!
//! [`Session`] sits behind a `std::sync::Mutex`, not an async one, and the rule that makes that safe
//! is: **never hold the lock across an `await`.** Every publish path is written as three separate
//! steps — build the message under the lock, publish without it, apply the result under it again.
//! An async mutex would let the lock be held across a publish and quietly serialise the whole app
//! behind the network.
//!
//! ## Talking to the UI
//!
//! Four events, all `chat:`-prefixed:
//!
//! | event | payload | meaning |
//! |---|---|---|
//! | `chat:ready` | [`StatusView`] | the backend is up; query initial state now |
//! | `chat:changed` | `Vec<Change>` | these items changed — re-query them |
//! | `chat:status` | [`StatusView`] | our access or the membership changed |
//! | `chat:sync` | [`SyncView`] | a sync session started or ended, for a progress indicator |
//!
//! `chat:changed` says *what* changed, never *how*. The UI re-queries the affected items. That keeps
//! payloads small and bounded, and — more importantly — means a coalesced, reordered or dropped
//! event can never leave the UI displaying something the document does not actually say. The cost is
//! a round trip per change; the benefit is that the UI cannot drift out of sync with the backend,
//! which is the failure mode that is miserable to debug.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use p2panda::streams::{StreamEvent, StreamFrom, StreamPublisher, StreamSubscription};
use p2panda_core::VerifyingKey;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::Result;
use crate::data::auth;
use crate::data::chat::Change;
use crate::data::message::{GroupMessage, Message};
use crate::data::persistence::{KeyStorage, Profile};
use crate::session::{Applied, Session};
use crate::views::{MemberView, StatusView};

pub const EVENT_READY: &str = "chat:ready";
pub const EVENT_CHANGED: &str = "chat:changed";
pub const EVENT_STATUS: &str = "chat:status";
pub const EVENT_SYNC: &str = "chat:sync";

/// Which profile directory to use. Overridable so two instances can run side by side on one machine
/// — the only way to exercise peer-to-peer behaviour during development.
const PROFILE_ENV: &str = "P2PANDA_CHAT_PROFILE";
const DEFAULT_PROFILE: &str = "default";

/// Set to use a key file instead of the OS credential store.
///
/// The keyring is the right default — the private key is the whole identity — but it needs a running
/// Secret Service (Linux), which a headless box, a container or a fresh CI runner will not have. In
/// that case the app cannot start at all, so there has to be a way out that is not "patch the code".
const KEY_FILE_ENV: &str = "P2PANDA_CHAT_KEY_FILE";

/// Tauri's managed state. Holds nothing until the backend has finished starting.
#[derive(Default)]
pub struct AppState {
    backend: Mutex<Option<Arc<Backend>>>,
}

impl AppState {
    /// The running backend, or an error a command can hand straight back to the UI.
    pub fn backend(&self) -> Result<Arc<Backend>> {
        self.backend
            .lock()
            .expect("backend slot is never poisoned")
            .clone()
            .ok_or_else(|| "the backend is still starting up".into())
    }

    fn set(&self, backend: Arc<Backend>) {
        *self.backend.lock().expect("backend slot is never poisoned") = Some(backend);
    }
}

/// A running node, its stream, and the session it feeds.
pub struct Backend {
    /// Owned purely to keep the node alive; p2panda has no explicit shutdown. Never dropped before
    /// the app exits.
    _node: p2panda::Node,
    publisher: StreamPublisher<Message>,
    session: Mutex<Session>,
    app: AppHandle,
}

impl Backend {
    /// Bring everything up and register it in the app's state.
    pub async fn start(app: AppHandle) -> Result<()> {
        let profile_name =
            std::env::var(PROFILE_ENV).unwrap_or_else(|_| DEFAULT_PROFILE.to_string());
        let profile = Profile::open(&profile_name)?;

        // The OS credential store talks over D-Bus on Linux, which blocks. Keep it off the runtime.
        let storage = match std::env::var(KEY_FILE_ENV) {
            Ok(value) if !value.is_empty() && value != "0" => KeyStorage::File,
            _ => KeyStorage::Keyring,
        };
        let (signing_key, key_origin) = {
            let profile = profile.clone();
            tauri::async_runtime::spawn_blocking(move || profile.load_or_create_key(storage))
                .await??
        };
        let my_id = signing_key.verifying_key();

        if let Some(warning) = profile.check_key_without_log(key_origin, storage) {
            log::warn!("{warning}");
        }

        // Which server, and are we founding it or resuming? A founder's server id is *derived from
        // their key*, so the id commits to the genesis and no two peers can disagree about who
        // created it.
        //
        // TODO: joining someone else's server. That needs a UI to paste a server id into and a
        // restart of the stream on a different topic, so it waits for the frontend. Everything under
        // it — `Session::subgroup_message`, the `wants_subgroup` flag — is already in place.
        let (server_id, founding) = match profile.load_list_id()? {
            Some(id) => (id, false),
            None => (auth::list_topic(&my_id), true),
        };
        profile.store_list_id(server_id)?;

        // TODO: no network configuration yet. `NodeBuilder` takes `relay_url`, `bootstrap`,
        // `mdns_mode`, `network_id` and bind addresses; with none of them set this is a local-area
        // node with default mDNS discovery and no relay, which is enough to run the app on one
        // machine. Reaching peers across a NAT needs at least a relay and a bootstrap node.
        let node = p2panda::builder()
            .signing_key(signing_key)
            .database_url(&profile.database_url())
            .spawn()
            .await?;

        // From the very start of the log: the Loro document *and* the group are rebuilt by replaying
        // it. Neither is stored as materialised state — the log is the storage, for both.
        let (publisher, subscription) = node
            .stream_from::<Message>(server_id, StreamFrom::Start)
            .await?;

        let backend = Arc::new(Backend {
            _node: node,
            publisher,
            session: Mutex::new(Session::new(my_id, server_id, false)),
            app: app.clone(),
        });

        if founding {
            // Two operations, two levels, from the very first commit: our own subgroup, then the top
            // group referencing it and naming this device as admin.
            for message in backend.with_session(|session| session.genesis_messages()) {
                backend.publish_group(message).await?;
            }
        }

        app.state::<AppState>().set(Arc::clone(&backend));
        tauri::async_runtime::spawn(run_stream(Arc::clone(&backend), subscription));

        log::info!(
            "chat backend ready: profile={profile_name} ({}) server={server_id} device={}",
            profile.dir().display(),
            my_id.to_hex(),
        );
        backend.emit_status(EVENT_READY);
        Ok(())
    }

    /// Run something against the session. Sync only — see the locking note in the module docs.
    pub fn with_session<T>(&self, f: impl FnOnce(&mut Session) -> T) -> T {
        let mut session = self.session.lock().expect("session is never poisoned");
        f(&mut session)
    }

    /// Publish a group operation and apply it to ourselves straight away.
    ///
    /// Applying immediately, rather than waiting for p2panda to echo it back, means the UI reflects
    /// what just happened. It costs nothing and stays consistent: `PublishFuture` hands us the very
    /// operation hash every other peer will see, so when the echo arrives the orderer drops it as a
    /// duplicate.
    pub async fn publish_group(&self, message: GroupMessage) -> Result<()> {
        let wire = Message::Group(Box::new(message));
        let published = self.publisher.publish(wire.clone()).await?;

        let my_id = self.with_session(|session| session.my_id());
        let applied = self.with_session(|session| session.receive(published.hash(), my_id, wire));
        self.report(applied);
        Ok(())
    }

    /// Publish whatever the last command changed in the chat document.
    ///
    /// A no-op when nothing changed, which is why every mutating command can call it unconditionally.
    /// We do *not* feed our own update back through `Session::receive` — the document already has it,
    /// and the echo p2panda sends us later is dropped by the orderer as a duplicate.
    pub async fn publish_chat(&self) -> Result<()> {
        let Some(message) = self.with_session(|session| session.take_chat_update())? else {
            return Ok(());
        };
        self.publisher.publish(message).await?;

        let changes = self.with_session(|session| session.drain_changes());
        self.emit_changes(changes);
        Ok(())
    }

    /// A new user's one-time subgroup creation, if this is the moment for it.
    async fn ensure_subgroup(&self) -> Result<()> {
        let Some(message) = self.with_session(|session| session.subgroup_message()) else {
            return Ok(());
        };
        self.publish_group(message).await
    }

    // --- Events ---------------------------------------------------------------------------------

    /// Push everything an [`Applied`] produced out to the UI and the log.
    fn report(&self, applied: Applied) {
        for note in &applied.notes {
            log::info!("{note}");
        }
        for reason in &applied.rejected {
            // A peer sending us operations we refuse is a diagnostic, not something the person using
            // the app can act on — so it goes to the log, not the window.
            log::warn!("{reason}");
        }
        self.emit_changes(applied.changes);
        if applied.access_changed {
            self.emit_status(EVENT_STATUS);
        }
    }

    fn emit_changes(&self, changes: Vec<Change>) {
        if changes.is_empty() {
            return;
        }
        if let Err(err) = self.app.emit(EVENT_CHANGED, changes) {
            log::error!("could not emit {EVENT_CHANGED}: {err}");
        }
    }

    fn emit_status(&self, event: &str) {
        let status = self.with_session(status_of);
        if let Err(err) = self.app.emit(event, status) {
            log::error!("could not emit {event}: {err}");
        }
    }

    fn emit_sync(&self, phase: &str, peer: VerifyingKey) {
        let payload = SyncView {
            phase: phase.to_string(),
            peer: peer.to_hex(),
        };
        if let Err(err) = self.app.emit(EVENT_SYNC, payload) {
            log::error!("could not emit {EVENT_SYNC}: {err}");
        }
    }
}

/// Sync progress, for a "connecting…" indicator.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncView {
    /// `"started"` or `"ended"`.
    pub phase: String,
    pub peer: String,
}

/// Our standing, membership and identity, as the UI wants it.
pub fn status_of(session: &mut Session) -> StatusView {
    let group = session.group();
    let members = group
        .describe_membership()
        .into_iter()
        .map(|line| MemberView {
            is_me: line.id == session.my_id(),
            id: line.id.to_hex(),
            access: line.access.to_string(),
            is_subgroup: line.is_subgroup,
            depth: line.depth,
        })
        .collect();

    StatusView {
        server_id: session.server_id().to_hex(),
        device_id: session.my_id().to_hex(),
        author_id: session.my_author().to_hex(),
        access: session.my_standing(),
        may_write: session.group().i_may_write(),
        may_manage: session.group().i_may_manage_list(),
        pending: session.pending(),
        members,
    }
}

/// The stream task. Runs for the life of the app, turning p2panda events into session state and UI
/// events.
async fn run_stream(backend: Arc<Backend>, mut subscription: StreamSubscription<Message>) {
    while let Some(event) = subscription.next().await {
        match event {
            StreamEvent::Processed { operation, .. } => {
                let applied = backend.with_session(|session| {
                    session.receive(
                        operation.id(),
                        operation.author(),
                        operation.message().clone(),
                    )
                });
                backend.report(applied);
            }

            StreamEvent::SyncStarted { remote_node_id, .. } => {
                backend.emit_sync("started", remote_node_id);
            }

            StreamEvent::SyncEnded { remote_node_id, .. } => {
                backend.emit_sync("ended", remote_node_id);

                // A peer that has just joined learns the group from this sync, not from a replay —
                // it has no log of its own yet — so this is where a new user creates their subgroup
                // and finds out whether they may write.
                if let Err(err) = backend.ensure_subgroup().await {
                    log::error!("could not create our device subgroup: {err}");
                }
                backend.emit_status(EVENT_STATUS);
            }

            StreamEvent::ReplayEnded => {
                let pending = backend.with_session(|session| session.pending());
                if pending > 0 {
                    // Not lost: each applies the moment the group operation it names turns up.
                    log::info!("{pending} operation(s) waiting on group operations we don't have");
                }
                if let Err(err) = backend.ensure_subgroup().await {
                    log::error!("could not create our device subgroup: {err}");
                }
                backend.emit_status(EVENT_STATUS);
            }

            StreamEvent::ReplayStarted { .. }
            | StreamEvent::ImportStarted { .. }
            | StreamEvent::ImportEnded { .. } => {}

            other => log::warn!("unhandled stream event: {other:?}"),
        }
    }

    // The stream only ends if the node is going away, which for us means the app is shutting down.
    log::info!("chat stream ended");
}

#[cfg(test)]
mod tests {
    //! One test, against a **real p2panda node**, covering the seam nothing else can.
    //!
    //! The unit tests in [`crate::session`] use a hand-rolled wire, which proves the rules but
    //! assumes p2panda behaves as documented. This proves the assumptions themselves:
    //!
    //! * `Message` survives the CBOR round trip p2panda does on it;
    //! * the author on a received operation is the key p2panda verified, and matches the one the
    //!   group was built with — if this were wrong, every write check would fail closed and the app
    //!   would look broken with no clue why;
    //! * `PublishFuture::hash()` is the same hash the operation arrives with, which is what lets
    //!   [`Backend::publish_group`] apply immediately and still have the echo deduplicate;
    //! * a peer holding nothing can rebuild the whole server from the stream alone.
    //!
    //! It uses no `AppHandle`, so it exercises everything in this module except the Tauri events.
    //! The node gets p2panda's default in-memory database, so nothing touches the real profile
    //! directory or the OS keyring.

    use std::time::Duration;

    use p2panda_core::SigningKey;

    use super::*;
    use crate::session::Session;

    #[tokio::test]
    async fn a_real_node_round_trips_founding_and_a_message() {
        let key = SigningKey::generate();
        let my_id = key.verifying_key();
        let server = auth::list_topic(&my_id);

        // Default builder: in-memory SQLite, local-area networking, no relay. Same configuration
        // `Backend::start` uses, minus the on-disk database.
        let node = p2panda::builder()
            .signing_key(key)
            .spawn()
            .await
            .expect("node spawns");
        let (publisher, mut subscription) = node
            .stream_from::<Message>(server, StreamFrom::Start)
            .await
            .expect("stream opens");

        // Found the server, exactly as `Backend::start` does.
        let mut author = Session::new(my_id, server, false);
        let mut published_hashes = Vec::new();
        for message in author.genesis_messages() {
            let wire = Message::Group(Box::new(message));
            let published = publisher.publish(wire.clone()).await.expect("publish");
            published_hashes.push(published.hash());
            author.receive(published.hash(), my_id, wire);
        }
        assert!(author.require_write().is_ok(), "the founder may write");

        // Write something and publish it.
        let channel = author
            .chat()
            .create_channel("general", 1_000)
            .expect("channel");
        author
            .chat()
            .post_message(channel, "hello from a real node", 2_000)
            .expect("message");
        let update = author
            .take_chat_update()
            .expect("export")
            .expect("there are changes");
        let published = publisher.publish(update).await.expect("publish");
        published_hashes.push(published.hash());

        // A peer that has seen nothing, rebuilt purely from what comes off the stream.
        let mut replica = Session::new(my_id, server, false);
        let mut seen = Vec::new();

        let collect = async {
            while seen.len() < published_hashes.len() {
                let Some(event) = subscription.next().await else {
                    panic!("the stream ended early");
                };
                if let StreamEvent::Processed { operation, .. } = event {
                    seen.push(operation.id());
                    let applied = replica.receive(
                        operation.id(),
                        operation.author(),
                        operation.message().clone(),
                    );
                    assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), collect)
            .await
            .expect("all operations come back off the stream");

        // The hashes we were handed at publish time are the ones the operations arrived with. This
        // is what makes `publish_group`'s apply-immediately safe.
        for hash in &published_hashes {
            assert!(seen.contains(hash), "published hash {hash} never arrived");
        }

        // And the replica rebuilt the server from the stream alone.
        assert!(replica.group().i_may_manage_list());
        assert_eq!(replica.chat().channels().len(), 1);
        assert_eq!(replica.chat().channels()[0].name, "general");
        let messages = replica.chat().channel_messages(channel);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "hello from a real node");
        assert_eq!(messages[0].author, auth::person_group_id(&my_id));
    }
}
