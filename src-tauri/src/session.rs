// SPDX-License-Identifier: MIT OR Apache-2.0

//! One node's view of one chat server: the data, the group governing it, and the operations that
//! cannot be judged yet.
//!
//! This is the event loop from the `ref/` CLI example, restated for a GUI. The pipeline is the same
//! four steps — **receive → order → authorize → apply** — and the rules are the ones the example
//! established; what changed is where the output goes. The CLI printed to a terminal; this returns
//! [`Applied`], which [`crate::backend`] turns into Tauri events.
//!
//! ## Why this type knows nothing about p2panda or Tauri
//!
//! [`Session`] never publishes anything and never emits anything. It takes messages in and hands
//! back what changed. Publishing is [`crate::backend`]'s job, and it feeds the result straight back
//! through [`Session::receive`].
//!
//! That split is not tidiness for its own sake — it is what makes the interesting behaviour
//! testable. The tests at the bottom of this file drive a whole server through founding, a member
//! joining, an authorized write and an unauthorized one, and messages arriving in the worst possible
//! order, with no node, no network and no `AppHandle` anywhere. None of that could be exercised if
//! the loop published as a side effect.
//!
//! ## The two-layer check, in one place
//!
//! [`Session::apply`] is where the layers described in [`crate::data::chat`] meet:
//!
//! 1. `may_write_at(author, depends_on)` — the group's coarse verdict, evaluated at the operations
//!    the update named, so every peer decides identically regardless of sync order.
//! 2. `identity_at(author, depends_on)` — which *person* that device is, resolved at the same
//!    point, because the data model authorizes people rather than devices.
//! 3. `import_checked(update, person)` — the per-item rules: your own message, your own profile.
//!
//! An update that fails any of them is never handed to Loro. That ordering matters: step 3 assumes
//! step 1 has already passed, and there is no un-merging once bytes are in the document.
//!
//! ## What is deliberately missing
//!
//! Removal and demotion, exactly as in the example — the group CRDT has `Remove` and `Demote` and
//! nothing here ever issues them. See `ref/README.md` for why that is its own piece of work: a
//! removed member can still write against a group head at which they had access, and fixing it
//! properly needs a freshness rule nobody has designed yet.

use p2panda_auth::group::{GroupAction, GroupMember};
use p2panda_auth::{Access, GroupsOperation};
use p2panda_core::{Hash, VerifyingKey};

use crate::Result;
use crate::data::auth::{self, AuthGroup, Conditions, GroupOperation};
use crate::data::chat::{Change, ChatDoc, ServerId};
use crate::data::message::{ChatUpdate, GroupMessage, Message};
use crate::data::ordering::Orderer;

/// An operation the orderer has released: its dependencies are applied, so it can now be judged.
///
/// Both variants are boxed because an ed25519 `VerifyingKey` and a `GroupAction::Create` are each
/// far bigger than the enum's discriminant; boxing keeps the buffered queue small.
enum Ready {
    Group(Box<GroupOperation>),
    Chat(Box<ChatOperation>),
}

/// A chat update, with the author p2panda verified it against.
struct ChatOperation {
    author: VerifyingKey,
    update: ChatUpdate,
}

/// What came of feeding messages in: what changed, and what we refused.
#[derive(Debug, Default)]
pub struct Applied {
    /// Items the UI should re-query.
    pub changes: Vec<Change>,
    /// Human-readable reasons we turned something away. Surfaced as log lines rather than as UI
    /// state — a peer sending us operations we reject is a diagnostic, not something the person
    /// using the app can act on.
    pub rejected: Vec<String>,
    /// Set when our own access level changed, so the caller can push a fresh status to the UI.
    pub access_changed: bool,
    /// Membership changes we accepted, described for the log. Not UI state — the UI reads the
    /// membership itself from [`crate::backend::status_of`]; this is here so an operator can see
    /// *when* the group moved and why, which is otherwise invisible.
    pub notes: Vec<String>,
}

impl Applied {
    fn merge(&mut self, other: Applied) {
        self.changes.extend(other.changes);
        self.rejected.extend(other.rejected);
        self.notes.extend(other.notes);
        self.access_changed |= other.access_changed;
    }
}

/// Everything one node knows about one chat server.
pub struct Session {
    /// This device's key: our node id, the author of everything we sign, and our member id in the
    /// group. Not the same as our *authorship* in the chat document — see [`Session::my_author`].
    my_id: VerifyingKey,
    chat: ChatDoc,
    group: AuthGroup,
    orderer: Orderer<Ready>,
    /// Whether this run should create our own device subgroup, and whether we have done it. Only a
    /// person joining as a *new user* needs one; a second device belongs to someone else's.
    wants_subgroup: bool,
    subgroup_ensured: bool,
}

impl Session {
    pub fn new(my_id: VerifyingKey, server: ServerId, wants_subgroup: bool) -> Self {
        Self {
            my_id,
            // Until the group is synced, the best guess at who we are is the subgroup we would
            // manage. `refresh_identity` corrects this the moment the group says otherwise.
            chat: ChatDoc::new(server, auth::person_group_id(&my_id)),
            group: AuthGroup::new(my_id, server),
            orderer: Orderer::new(),
            wants_subgroup,
            subgroup_ensured: false,
        }
    }

    pub fn server_id(&self) -> ServerId {
        self.chat.id()
    }

    pub fn my_id(&self) -> VerifyingKey {
        self.my_id
    }

    /// Who we are *in the chat document* — a person, not a device.
    pub fn my_author(&self) -> VerifyingKey {
        self.chat.me()
    }

    pub fn chat(&self) -> &ChatDoc {
        &self.chat
    }

    pub fn group(&self) -> &AuthGroup {
        &self.group
    }

    /// How many operations are waiting on dependencies we have not seen. Non-zero is normal
    /// mid-sync; persistently non-zero means someone referenced a group operation we never got.
    pub fn pending(&self) -> usize {
        self.orderer.pending()
    }

    // --- Receiving ----------------------------------------------------------------------------

    /// Take in one operation — from the network, from the replayed log, or one we just published —
    /// and apply whatever that makes applicable.
    ///
    /// `id` and `author` do **not** come from the payload. They are p2panda's operation hash and
    /// the key whose signature it verified, so an operation cannot lie about who wrote it. If the
    /// author were self-declared the group would mean nothing: anyone could claim to be the admin.
    pub fn receive(&mut self, id: Hash, author: VerifyingKey, message: Message) -> Applied {
        let mut applied = Applied::default();

        let ready = match message {
            Message::Group(group_message) => {
                let operation = GroupsOperation {
                    id,
                    author,
                    dependencies: group_message.dependencies.clone(),
                    group_id: group_message.group_id,
                    action: group_message.action,
                };
                self.orderer.process(
                    id,
                    group_message.dependencies,
                    Ready::Group(Box::new(operation)),
                )
            }
            Message::Chat(update) => {
                // An update naming no group operation can never be authorized — there is no
                // membership snapshot to judge it against. Refuse it now rather than let it sit in
                // the buffer forever waiting for a dependency that does not exist.
                if update.depends_on.is_empty() {
                    applied.rejected.push(format!(
                        "rejected an update from {}: it depends on no group operation",
                        short(author)
                    ));
                    return applied;
                }

                self.orderer.process(
                    id,
                    update.depends_on.clone(),
                    Ready::Chat(Box::new(ChatOperation { author, update })),
                )
            }
        };

        for operation in ready {
            let outcome = self.apply(operation);
            applied.merge(outcome);
        }

        applied
    }

    fn apply(&mut self, operation: Ready) -> Applied {
        let mut applied = Applied::default();

        match operation {
            Ready::Group(operation) => {
                let before = self.group.my_access();
                // Built before `apply` consumes the operation.
                let description = describe(&operation);
                match self.group.apply(*operation) {
                    Ok(()) => {
                        applied.notes.push(description);
                        // A membership change can change who *we* are: a second device learns its
                        // person only once the group naming it has synced.
                        self.refresh_identity();
                        applied.access_changed = self.group.my_access() != before;
                    }
                    Err(err) => applied.rejected.push(err.to_string()),
                }
            }

            Ready::Chat(operation) => {
                let ChatOperation { author, update } = *operation;

                // Layer 1: may this device write here at all, as of the operations it named?
                if !self.group.may_write_at(author, &update.depends_on) {
                    applied.rejected.push(format!(
                        "rejected an update from {}: not a writer as of the group operation it \
                         depends on",
                        short(author)
                    ));
                    return applied;
                }

                // Which person is that device? The chat model authorizes people, not devices.
                let Some(person) = self.group.identity_at(author, &update.depends_on) else {
                    applied.rejected.push(format!(
                        "rejected an update from {}: cannot resolve which person they are",
                        short(author)
                    ));
                    return applied;
                };

                // Layer 2: are all the items it touches that person's to touch?
                if let Err(err) = self.chat.import_checked(&update.update, person) {
                    applied.rejected.push(format!(
                        "rejected an update from {}: {err}",
                        short(author)
                    ));
                    return applied;
                }

                applied.changes = self.chat.drain_changes();
            }
        }

        applied
    }

    /// Re-resolve which person this device belongs to, against the current group.
    ///
    /// Cheap and idempotent, so it runs after every accepted group operation rather than trying to
    /// detect the one that matters.
    fn refresh_identity(&mut self) {
        let heads = self.group.heads();
        if let Some(person) = self.group.identity_at(self.my_id, &heads) {
            self.chat.set_me(person);
        }
    }

    // --- Producing messages to publish --------------------------------------------------------
    //
    // These build messages; they do not send them. The caller publishes and then feeds the result
    // back through `receive` with the hash p2panda assigned.

    /// The two `Create`s that found a server: our own subgroup, then the top group naming it.
    pub fn genesis_messages(&self) -> Vec<GroupMessage> {
        self.group
            .genesis_actions()
            .into_iter()
            .map(|(group_id, action)| GroupMessage {
                group_id,
                action,
                // A genesis has no predecessor. Deliberately not `heads_for`, which would be empty
                // anyway but reads as though it might not be.
                dependencies: Vec::new(),
            })
            .collect()
    }

    /// Wrap a group action with the tips of *its own group's* history.
    ///
    /// Per-group tips, not every group's, so one person's device changes stay causally independent
    /// of another's — while concurrent changes within a group remain resolvable rather than
    /// last-one-wins.
    pub fn group_message(
        &self,
        group_id: VerifyingKey,
        action: GroupAction<VerifyingKey, Conditions>,
    ) -> GroupMessage {
        GroupMessage {
            dependencies: self.group.heads_for(group_id),
            group_id,
            action,
        }
    }

    /// Everything we have changed locally and not yet published, tagged with the group operations
    /// it was written against. `None` when there is nothing new.
    ///
    /// The tag is *all* group tips, not one group's: judging a chat update needs to see the top
    /// group and the author's subgroup at once.
    pub fn take_chat_update(&mut self) -> Result<Option<Message>> {
        let Some(update) = self.chat.export()? else {
            return Ok(None);
        };

        let depends_on = self.group.heads();
        if depends_on.is_empty() {
            return Err("cannot publish before the server's group exists".into());
        }

        Ok(Some(Message::Chat(ChatUpdate { depends_on, update })))
    }

    /// Drain what changed locally, for the UI. Call after [`Session::take_chat_update`], which is
    /// what commits the Loro document and so what makes the changes visible.
    pub fn drain_changes(&self) -> Vec<Change> {
        self.chat.drain_changes()
    }

    /// A new user creates their own subgroup once, as soon as they know the server exists.
    ///
    /// The subgroup is what lets them add their own further devices, and what an admin's "add this
    /// person" points at. Returns `None` when there is nothing to do — a second device, a resume, or
    /// a server we have not synced yet. Waiting for the top group means we do not mint a subgroup
    /// for a mistyped server id.
    pub fn subgroup_message(&mut self) -> Option<GroupMessage> {
        if !self.wants_subgroup || self.subgroup_ensured {
            return None;
        }
        // Wait until a sync tells us the server exists, so we do not mint a subgroup for a
        // mistyped server id.
        self.group.top_group_id()?;
        self.subgroup_ensured = true;
        if self.group.i_manage_a_subgroup() {
            return None;
        }
        let (group_id, action) = self.group.create_my_subgroup_action();
        Some(self.group_message(group_id, action))
    }

    // --- Membership actions -------------------------------------------------------------------

    /// Admin action: add a *person* to the server as a reader or writer.
    pub fn add_user_message(
        &self,
        device: VerifyingKey,
        access: Access<Conditions>,
    ) -> Result<GroupMessage> {
        // p2panda-auth enforces "only a manager may change membership" itself, but refusing here
        // gives a clearer error than an operation every peer silently drops.
        if !self.group.i_may_manage_list() {
            return Err(format!(
                "only an admin may add people (your access: {})",
                self.my_standing()
            )
            .into());
        }
        let (group_id, action) = self.group.add_user_action(device, access)?;
        Ok(self.group_message(group_id, action))
    }

    /// Subgroup-manager action: add one of *our own* devices.
    pub fn add_device_message(
        &self,
        device: VerifyingKey,
        access: Access<Conditions>,
    ) -> Result<GroupMessage> {
        let (group_id, action) = self.group.add_device_action(device, access)?;
        Ok(self.group_message(group_id, action))
    }

    // --- Queries ------------------------------------------------------------------------------

    /// What we may do, in a form fit for display.
    pub fn my_standing(&self) -> String {
        match self.group.my_access() {
            Some(access) => access.to_string(),
            None => "none".to_string(),
        }
    }

    /// May we write? A courtesy check, not the enforcement.
    ///
    /// It saves publishing an operation every peer would throw away, and it lets the error say what
    /// to do about it. The check that matters runs on the **receiving** side, in [`Session::apply`],
    /// on every peer including this one.
    pub fn require_write(&self) -> Result<()> {
        if self.group.i_may_write() {
            return Ok(());
        }
        Err(format!(
            "you may not write to this server (your access: {}). Ask an admin to add {}",
            self.my_standing(),
            self.my_id.to_hex()
        )
        .into())
    }
}

/// A one-line rendering of a group operation, aware of the two-level structure. For logs.
pub fn describe(operation: &GroupOperation) -> String {
    let author = short(operation.author);
    let is_top = operation.group_id == auth::top_group_id(&operation.author);

    match &operation.action {
        GroupAction::Create { .. } if is_top => format!("{author} created the server"),
        GroupAction::Create { .. } => format!("{author} created their device subgroup"),
        GroupAction::Add {
            member: GroupMember::Group(subgroup),
            access,
        } => format!("{author} added a {access} user (subgroup {})", short(*subgroup)),
        GroupAction::Add {
            member: GroupMember::Individual(device),
            access,
        } => format!("{author} added device {} as {access}", short(*device)),
        GroupAction::Remove { member } => format!("{author} removed {}", short(member.id())),
        GroupAction::Promote { member, access } => {
            format!("{author} promoted {} to {access}", short(member.id()))
        }
        GroupAction::Demote { member, access } => {
            format!("{author} demoted {} to {access}", short(member.id()))
        }
    }
}

/// Node ids are 64 hex characters. Nobody wants to read that in a log line.
pub fn short(id: VerifyingKey) -> String {
    id.to_hex()[..8].to_string()
}

#[cfg(test)]
mod tests {
    //! A whole server driven through its lifecycle with no node and no network: two peers, a hand
    //! -rolled "wire" that just moves `Message`s between them, and full control over delivery order.
    //!
    //! What these cover that the data-layer tests cannot is the *seam* — that the group's verdict
    //! and the chat document's per-item rules compose correctly, and that the orderer holds things
    //! back until they can be judged.

    use p2panda_core::SigningKey;

    use super::*;
    use crate::data::chat::Collection;

    /// Stands in for p2panda: assigns operation hashes and carries messages between sessions.
    struct Wire {
        next: u64,
    }

    impl Wire {
        fn new() -> Self {
            Self { next: 0 }
        }

        fn next_hash(&mut self) -> Hash {
            self.next += 1;
            Hash::digest(self.next.to_le_bytes())
        }

        /// Publish from `from` and deliver to everyone, `from` included — exactly what p2panda does
        /// by echoing our own operations back to us.
        fn broadcast(
            &mut self,
            author: VerifyingKey,
            message: Message,
            to: &mut [&mut Session],
        ) -> Applied {
            let id = self.next_hash();
            let mut applied = Applied::default();
            for session in to {
                applied.merge(session.receive(id, author, message.clone()));
            }
            applied
        }
    }

    fn device() -> SigningKey {
        SigningKey::generate()
    }

    /// Found a server: publish the two genesis operations to everyone listening.
    fn found(wire: &mut Wire, creator: VerifyingKey, sessions: &mut [&mut Session]) {
        let genesis = sessions[0].genesis_messages();
        for message in genesis {
            wire.broadcast(creator, Message::Group(Box::new(message)), sessions);
        }
    }

    /// Publish whatever `index` changed locally to everybody.
    fn publish_chat(wire: &mut Wire, author: VerifyingKey, sessions: &mut [&mut Session], index: usize) -> Applied {
        let message = sessions[index]
            .take_chat_update()
            .expect("export")
            .expect("there are local changes");
        wire.broadcast(author, message, sessions)
    }

    #[test]
    fn founding_a_server_makes_the_creator_an_admin_who_may_write() {
        let mut wire = Wire::new();
        let alice = device();
        let alice_id = alice.verifying_key();
        let mut session = Session::new(alice_id, auth::list_topic(&alice_id), false);

        found(&mut wire, alice_id, &mut [&mut session]);

        assert!(session.group().i_may_manage_list());
        assert!(session.require_write().is_ok());
        assert_eq!(session.my_standing(), "manage");
        // Our authorship resolved to our person subgroup, not our raw device key.
        assert_eq!(session.my_author(), auth::person_group_id(&alice_id));
    }

    /// The happy path end to end: found, write, and see the change reported for the UI.
    #[test]
    fn a_local_write_reports_what_changed() {
        let mut wire = Wire::new();
        let alice = device();
        let alice_id = alice.verifying_key();
        let mut session = Session::new(alice_id, auth::list_topic(&alice_id), false);
        found(&mut wire, alice_id, &mut [&mut session]);

        let channel = session.chat().create_channel("general", 1_000).expect("channel");
        session.chat().post_message(channel, "hello", 2_000).expect("message");

        // Publishing is what commits the document, so changes are drained after it.
        session.take_chat_update().expect("export").expect("changes");
        let changes = session.drain_changes();

        assert!(changes.iter().any(|c| c.collection == Collection::Channel));
        assert!(changes.iter().any(|c| c.collection == Collection::Message));
        assert!(changes.iter().any(|c| c.collection == Collection::ChannelOrder));
        assert_eq!(session.chat().channel_messages(channel).len(), 1);
    }

    /// Two people, the full join flow, and a write that arrives authorized.
    #[test]
    fn an_added_writer_can_write_and_their_messages_arrive() {
        let mut wire = Wire::new();
        let (alice, bobby) = (device(), device());
        let (alice_id, bobby_id) = (alice.verifying_key(), bobby.verifying_key());
        let server = auth::list_topic(&alice_id);

        let mut a = Session::new(alice_id, server, false);
        let mut b = Session::new(bobby_id, server, true);
        found(&mut wire, alice_id, &mut [&mut a, &mut b]);

        // Alice adds Bobby as a writer.
        let add = a.add_user_message(bobby_id, Access::write()).expect("add user");
        wire.broadcast(alice_id, Message::Group(Box::new(add)), &mut [&mut a, &mut b]);

        // Bobby creates his own subgroup, which is what makes him a writer in practice.
        let subgroup = b.subgroup_message().expect("bobby needs a subgroup");
        wire.broadcast(bobby_id, Message::Group(Box::new(subgroup)), &mut [&mut a, &mut b]);

        assert!(b.require_write().is_ok(), "bobby should be a writer now");
        assert_eq!(b.my_author(), auth::person_group_id(&bobby_id));

        // Alice makes a channel, Bobby posts in it.
        let channel = a.chat().create_channel("general", 1_000).expect("channel");
        publish_chat(&mut wire, alice_id, &mut [&mut a, &mut b], 0);

        b.chat().post_message(channel, "hello from bobby", 2_000).expect("message");
        let applied = publish_chat(&mut wire, bobby_id, &mut [&mut b, &mut a], 0);

        assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
        assert_eq!(a.chat().channel_messages(channel).len(), 1);
        assert_eq!(a.chat().channel_messages(channel)[0].text, "hello from bobby");
    }

    /// The coarse check, over the wire: a device nobody added is not a writer, however well-formed
    /// its update is.
    #[test]
    fn an_update_from_a_non_member_is_rejected() {
        let mut wire = Wire::new();
        let (alice, mallory) = (device(), device());
        let (alice_id, mallory_id) = (alice.verifying_key(), mallory.verifying_key());
        let server = auth::list_topic(&alice_id);

        let mut a = Session::new(alice_id, server, false);
        let mut m = Session::new(mallory_id, server, false);
        found(&mut wire, alice_id, &mut [&mut a, &mut m]);

        let channel = a.chat().create_channel("general", 1_000).expect("channel");
        publish_chat(&mut wire, alice_id, &mut [&mut a, &mut m], 0);

        // Mallory writes locally — nothing stops him doing that — and pushes it out.
        m.chat().post_message(channel, "i am not a member", 2_000).expect("message");
        assert!(m.require_write().is_err(), "the local courtesy check refuses too");
        let applied = publish_chat(&mut wire, mallory_id, &mut [&mut m, &mut a], 0);

        assert!(
            applied.rejected.iter().any(|r| r.contains("not a writer")),
            "{:?}",
            applied.rejected
        );
        assert!(a.chat().channel_messages(channel).is_empty());
    }

    /// The fine-grained check, over the wire: Bobby is a legitimate writer and still may not edit
    /// Alice's message. This is the seam the data-layer tests cannot reach on their own — it needs
    /// the group to resolve Bobby to a person first.
    #[test]
    fn a_writer_may_not_edit_another_persons_message_over_the_wire() {
        let mut wire = Wire::new();
        let (alice, bobby) = (device(), device());
        let (alice_id, bobby_id) = (alice.verifying_key(), bobby.verifying_key());
        let server = auth::list_topic(&alice_id);

        let mut a = Session::new(alice_id, server, false);
        let mut b = Session::new(bobby_id, server, true);
        found(&mut wire, alice_id, &mut [&mut a, &mut b]);

        let add = a.add_user_message(bobby_id, Access::write()).expect("add user");
        wire.broadcast(alice_id, Message::Group(Box::new(add)), &mut [&mut a, &mut b]);
        let subgroup = b.subgroup_message().expect("subgroup");
        wire.broadcast(bobby_id, Message::Group(Box::new(subgroup)), &mut [&mut a, &mut b]);

        let channel = a.chat().create_channel("general", 1_000).expect("channel");
        let message = a.chat().post_message(channel, "alice wrote this", 2_000).expect("message");
        publish_chat(&mut wire, alice_id, &mut [&mut a, &mut b], 0);

        // Bobby edits it. The local API refuses, so he has to go round it — which is exactly what a
        // modified client would do.
        assert!(b.chat().edit_message(message, "bobby wrote this", 3_000).is_err());
        let map = crate::data::chat::message::ROOT;
        let item = crate::data::chat::item_map(b.chat().doc(), map, &message.to_hex()).expect("message");
        crate::data::chat::text_field(&item, "text")
            .expect("body")
            .update("bobby wrote this", loro::UpdateOptions::default())
            .expect("local write");

        let applied = publish_chat(&mut wire, bobby_id, &mut [&mut b, &mut a], 0);

        assert!(
            applied.rejected.iter().any(|r| r.contains("only the author")),
            "{:?}",
            applied.rejected
        );
        assert_eq!(a.chat().message(message).expect("message").text, "alice wrote this");
    }

    /// The orderer's whole reason for existing, at the session level: a chat update that arrives
    /// before the group operation authorizing it must not be rejected as unauthorized. It waits, and
    /// applies once the group catches up.
    ///
    /// Without this, whether a legitimate message was accepted would depend on network timing, and
    /// two peers would disagree about the contents of a channel.
    #[test]
    fn an_update_arriving_before_its_authorizing_group_operation_waits() {
        let mut wire = Wire::new();
        let (alice, bobby) = (device(), device());
        let (alice_id, bobby_id) = (alice.verifying_key(), bobby.verifying_key());
        let server = auth::list_topic(&alice_id);

        // Bobby is fully set up and writes a message; Alice has not yet heard any of it.
        let mut a = Session::new(alice_id, server, false);
        let mut b = Session::new(bobby_id, server, true);
        found(&mut wire, alice_id, &mut [&mut a, &mut b]);

        let add = a.add_user_message(bobby_id, Access::write()).expect("add user");
        let add_id = wire.next_hash();
        let add_message = Message::Group(Box::new(add));
        a.receive(add_id, alice_id, add_message.clone());
        b.receive(add_id, alice_id, add_message.clone());

        let subgroup = b.subgroup_message().expect("subgroup");
        let subgroup_id = wire.next_hash();
        let subgroup_message = Message::Group(Box::new(subgroup));
        b.receive(subgroup_id, bobby_id, subgroup_message.clone());

        let channel = a.chat().create_channel("general", 1_000).expect("channel");
        let channel_update = a.take_chat_update().expect("export").expect("changes");
        let channel_id = wire.next_hash();
        a.receive(channel_id, alice_id, channel_update.clone());
        b.receive(channel_id, alice_id, channel_update);

        b.chat().post_message(channel, "arrives early", 2_000).expect("message");
        let bobby_update = b.take_chat_update().expect("export").expect("changes");

        // Alice gets Bobby's message *before* she has seen his subgroup being created.
        let update_id = wire.next_hash();
        let applied = a.receive(update_id, bobby_id, bobby_update);
        assert!(applied.rejected.is_empty(), "it must wait, not be refused: {:?}", applied.rejected);
        assert_eq!(a.pending(), 1, "the update is buffered");
        assert!(a.chat().channel_messages(channel).is_empty());

        // The missing group operation turns up and releases it.
        let applied = a.receive(subgroup_id, bobby_id, subgroup_message);
        assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
        assert_eq!(a.pending(), 0);
        assert_eq!(a.chat().channel_messages(channel).len(), 1);
        assert_eq!(a.chat().channel_messages(channel)[0].text, "arrives early");
    }

    /// p2panda echoes our own operations back and replays the whole log on restart, so every
    /// message is delivered more than once. Applying one twice must be a no-op.
    #[test]
    fn redelivery_is_harmless() {
        let mut wire = Wire::new();
        let alice = device();
        let alice_id = alice.verifying_key();
        let mut session = Session::new(alice_id, auth::list_topic(&alice_id), false);
        found(&mut wire, alice_id, &mut [&mut session]);

        let channel = session.chat().create_channel("general", 1_000).expect("channel");
        session.chat().post_message(channel, "hello", 2_000).expect("message");
        let update = session.take_chat_update().expect("export").expect("changes");

        let id = wire.next_hash();
        session.receive(id, alice_id, update.clone());
        session.receive(id, alice_id, update);

        assert_eq!(session.chat().channels().len(), 1);
        assert_eq!(session.chat().channel_messages(channel).len(), 1);
    }

    /// An update with no `depends_on` is refused immediately rather than buffered. There is no
    /// membership snapshot that could ever authorize it, so waiting would mean waiting forever.
    #[test]
    fn an_update_depending_on_nothing_is_refused_not_buffered() {
        let mut wire = Wire::new();
        let alice = device();
        let alice_id = alice.verifying_key();
        let mut session = Session::new(alice_id, auth::list_topic(&alice_id), false);
        found(&mut wire, alice_id, &mut [&mut session]);

        let applied = session.receive(
            wire.next_hash(),
            alice_id,
            Message::Chat(ChatUpdate {
                depends_on: Vec::new(),
                update: Vec::new(),
            }),
        );

        assert!(applied.rejected.iter().any(|r| r.contains("no group operation")));
        assert_eq!(session.pending(), 0, "it must not sit in the buffer");
    }

    /// A writer is not an admin. p2panda-auth enforces this itself; we refuse earlier so the error
    /// can say something useful.
    #[test]
    fn a_writer_may_not_add_people() {
        let mut wire = Wire::new();
        let (alice, bobby) = (device(), device());
        let (alice_id, bobby_id) = (alice.verifying_key(), bobby.verifying_key());
        let server = auth::list_topic(&alice_id);

        let mut a = Session::new(alice_id, server, false);
        let mut b = Session::new(bobby_id, server, true);
        found(&mut wire, alice_id, &mut [&mut a, &mut b]);

        let add = a.add_user_message(bobby_id, Access::write()).expect("add user");
        wire.broadcast(alice_id, Message::Group(Box::new(add)), &mut [&mut a, &mut b]);
        let subgroup = b.subgroup_message().expect("subgroup");
        wire.broadcast(bobby_id, Message::Group(Box::new(subgroup)), &mut [&mut a, &mut b]);

        assert!(b.require_write().is_ok());
        let err = b
            .add_user_message(device().verifying_key(), Access::write())
            .expect_err("a writer may not add people");
        assert!(err.to_string().contains("only an admin"), "{err}");
    }
}
