// SPDX-License-Identifier: MIT OR Apache-2.0

//! The chat data model: one topic is one chat server, and one Loro document holds all of it.
//!
//! A topic ([`ServerId`]) is the unit of everything: it has exactly one membership
//! ([`crate::data::auth::AuthGroup`]), one set of channels, and all the messages inside them. Joining
//! a server is subscribing to its topic; there is no other scope.
//!
//! ## The document layout
//!
//! Six root containers, and nothing else. An update that touches any other root is rejected outright
//! (see [`ChatDoc::import_checked`]) — new top-level state cannot be smuggled in by a peer running a
//! newer or hostile build.
//!
//! ```text
//! profiles      LoroMap   author-id  -> { name, status }              (see profile.rs)
//! channels      LoroMap   channel-id -> { name, topic, archived, .. } (see channel.rs)
//! channel_order LoroMovableList of channel-id                         (see channel.rs)
//! messages      LoroMap   message-id -> { author, channel, text, .. } (see message.rs)
//! reactions     LoroMap   reaction-id-> { author, message, emoji }    (see reaction.rs)
//! threads       LoroMap   thread-id  -> { channel, anchor, .. }       (see thread.rs)
//! ```
//!
//! Everything is flat and keyed by id. Nothing is nested by containment — a thread does not *hold*
//! its messages, a message names its thread. That is deliberate: a flat keyspace means two peers who
//! concurrently create a message and a thread can never produce a structural conflict, only a
//! dangling reference that resolves itself when the other half arrives. See "Dangling references are
//! normal" below.
//!
//! ## Two layers of authorization, and why one is not enough
//!
//! The `todo-auth` example this grew from had one rule: *may this author write at all?* The group
//! answers it, the write check is [`crate::data::auth::AuthGroup::may_write_at`] against the group
//! heads the edit named, and that is the whole story — every writer may edit every item.
//!
//! Chat needs more. "Alice may write here" must not mean "Alice may edit Bob's message" or "Alice
//! may rename Bob's profile". So there are two layers:
//!
//! 1. **Coarse (the group).** May this *device* write to this server at the group operations it
//!    depends on? Enforced by the caller, before we are ever reached, exactly as in the example.
//! 2. **Fine (this module).** Given that they may write *something*, may they write *this*? Enforced
//!    by [`ChatDoc::import_checked`], per touched item, against per-type ownership rules.
//!
//! ### Identity is the person, not the device
//!
//! Layer 2 works in terms of [`AuthorId`] — a **person subgroup id**, not a device key. Alice's
//! laptop and Alice's phone resolve to the same `AuthorId`, so "the author's subgroup may edit the
//! message" (the rule CHAT.md asks for) falls out for free, and adding a device does not orphan the
//! messages you wrote from the old one. The resolution is
//! [`crate::data::auth::AuthGroup::identity_at`], and like every other check it is evaluated *at the
//! group heads the update named*, so every peer computes the same author for the same update.
//!
//! ## Validate before merge, because merging is forever
//!
//! Loro merges whatever it is given, and there is no un-merging: once bad state is in the oplog it
//! is in every snapshot and every peer that syncs from you. But a Loro update is an opaque blob —
//! one update may touch a dozen items, and we cannot tell from the bytes whether they are all the
//! author's own.
//!
//! So [`ChatDoc::import_checked`] merges it into a **throwaway fork first**:
//!
//! ```text
//!   fork the doc  ->  import into the fork  ->  collect which items changed (via the fork's
//!   change events)  ->  authorize each one against pre-state and post-state  ->  all pass?
//!   import into the real doc.  any fail?  drop the fork, and the whole update with it.
//! ```
//!
//! Two properties worth naming:
//!
//! * **An update is atomic.** One bad item rejects the entire update, not just that item. Anything
//!   else would let an author publish a valid edit and an invalid one together and have the pair
//!   half-applied differently depending on how a peer chose to split it — and peers must agree.
//! * **Both states matter.** An authorization question is almost always "who owned this *before*",
//!   which is why the check gets the pre-import item (from the real doc) and the post-import item
//!   (from the fork). Checking only the post-state would let Mallory edit Bob's message *and*
//!   rewrite its `author` field to Mallory in one update, and pass.
//!
//! The cost is a document fork per remote update, which loro documents as O(n) in document size.
//! That is fine at the scale of "a chat server that fits in memory" and is not fine forever; see the
//! TODO on [`ChatDoc::preview`].
//!
//! ### Rejecting an update cuts that peer off, and that is not a bug we can fix here
//!
//! One consequence is sharp enough to state on its own. A peer's Loro operations form a **chain**:
//! their next update builds on the one before it. So refusing an update does not just discard that
//! update — every *later* update from the same peer arrives with a hole in its history, and Loro
//! parks it as pending rather than applying it. The peer is, from our point of view, silenced from
//! that moment on.
//!
//! That is harsh, and it is also the honest behaviour for V1. The local write API cannot produce an
//! update that fails validation, so a rejection means the peer is running modified or broken code —
//! and continuing to accept their data by silently skipping the parts we refused would mean two
//! peers holding different documents while both believing they are in sync, which is strictly worse
//! than one peer being visibly stuck.
//!
//! TODO: the real fix is to make rejection recoverable rather than terminal — track that we are out
//! of step with a peer and re-request their history from a known-good point, or validate at a finer
//! granularity than "the whole update". Both are V2 work, and both need a story for what the UI
//! shows while a peer is in that state. Until then, [`ChatDoc::import_checked`] returning `Err` for
//! a peer should be treated as "we have stopped listening to them", not "one message was dropped".
//!
//! ## Immutable fields
//!
//! Each item type declares fields that may never change after creation — a message's `author`,
//! `channel` and `created_at`, a channel's `created_by`. These are checked by comparing the
//! pre-import item to the post-import item field by field. Without this, "the owner may edit their
//! own item" is not enough of a rule: an owner could move their message into another channel long
//! after the fact, or an item could be laundered from one author to another.
//!
//! ## Dangling references are normal, and are not errors
//!
//! A reaction names a message; a message names a channel and maybe a thread. We do **not** check
//! that the target exists. Loro updates carry no cross-item causality, so a peer can legitimately
//! receive a reaction before the message it reacts to — rejecting it would mean rejecting valid data
//! for arriving in an order we did not choose, and (worse) two peers would end up with different
//! documents depending on network timing.
//!
//! Instead, references resolve lazily at read time: a reaction to an unknown message simply does not
//! show up in any message's reaction list until the message arrives, and then it does. Materializing
//! is a filter over a flat map, never a join that can fail.
//!
//! ## Ordering: timestamps are a hint, ids are the tie-break
//!
//! Messages sort by `(created_at, id)`. `created_at` is wall-clock milliseconds **supplied by the
//! author**, so it is a hint and nothing more — a peer with a wrong clock, or a malicious one, can
//! place a message anywhere in the list. What the id tie-break buys is the property that actually
//! matters for a CRDT: *every peer produces the same order for the same set of messages*, with no
//! dependence on arrival order or merge order.
//!
//! CHAT.md raises causal ordering ("show after" edges, so a reply always sorts below what it
//! replies to) as the more principled alternative. It is deliberately **not** implemented in V1:
//! [`message::Message::reply_to`] records the edge, so the data needed to switch is being collected
//! from day one, but the sort stays timestamp-based until we have a real answer to what an
//! interleaved conversation should look like when two people's clocks disagree.

pub mod channel;
pub mod message;
pub mod profile;
pub mod reaction;
pub mod thread;

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use loro::event::Diff;
use loro::{
    ContainerID, ExportMode, Index, LoroDoc, LoroMap, LoroText, LoroValue, ValueOrContainer,
    VersionVector,
};
use p2panda_core::{Hash, Topic, VerifyingKey};

use crate::Result;

/// A chat server. One topic, one membership, one Loro document.
pub type ServerId = Topic;

/// Who wrote something, as far as the data model is concerned: a **person subgroup id**, so all of
/// one person's devices share it. Resolved from the signing device by
/// [`crate::data::auth::AuthGroup::identity_at`].
pub type AuthorId = VerifyingKey;

/// The chat state of one server.
pub struct ChatDoc {
    id: ServerId,
    doc: LoroDoc,
    /// Everything already broadcast by us or imported from someone else. Anything beyond this
    /// version is a local change nobody else has seen yet.
    shared: VersionVector,
    /// Our own identity, used to stamp `author` on what we write locally and to refuse local edits
    /// of other people's items before they are ever published.
    me: AuthorId,
}

impl ChatDoc {
    pub fn new(id: ServerId, me: AuthorId) -> Self {
        Self {
            id,
            doc: LoroDoc::new(),
            shared: VersionVector::default(),
            me,
        }
    }

    pub fn id(&self) -> ServerId {
        self.id
    }

    /// Our own author id — the person subgroup we write as.
    pub fn me(&self) -> AuthorId {
        self.me
    }

    pub(crate) fn doc(&self) -> &LoroDoc {
        &self.doc
    }

    // --- Replication ------------------------------------------------------------------------

    /// Commit local changes and export the update bytes nobody else has seen yet, or `None` when
    /// there is nothing new to send.
    ///
    /// The caller pairs this with the group heads it was written against — see
    /// [`crate::data::message::ChatUpdate`].
    pub fn export(&mut self) -> Result<Option<Vec<u8>>> {
        self.doc.commit();

        let version = self.doc.oplog_vv();
        if version == self.shared {
            return Ok(None);
        }

        let bytes = self.doc.export(ExportMode::updates(&self.shared))?;
        self.shared = version;

        Ok(Some(bytes))
    }

    /// Merge a remote update, but only if every item it touches is one `author` may touch.
    ///
    /// The caller must *already* have established, against the group heads the update named, that
    /// the signing device may write at all and that it resolves to `author`. This is layer 2: given
    /// a writer, which items are theirs. See the module docs.
    ///
    /// On rejection nothing is merged — not even the parts that would have passed.
    pub fn import_checked(&mut self, update: &[u8], author: AuthorId) -> Result<()> {
        let (staging, touched) = self.preview(update)?;

        for touch in &touched {
            self.authorize(&staging, touch, author)?;
        }

        self.import(update)
    }

    /// Merge an update with **no** ownership checks.
    ///
    /// Only for updates we produced ourselves (the local write API already constrains those) and for
    /// replaying our own log at startup. Everything from the network goes through
    /// [`ChatDoc::import_checked`].
    ///
    /// Idempotent, so replayed and duplicated deliveries are harmless.
    ///
    /// `import` implicitly commits our own pending changes, so `oplog_vv()` afterwards can include a
    /// local edit we have not published yet; taking it wholesale as `shared` would mean that edit is
    /// never sent. Keeping our own peer's counter where it was avoids that.
    pub fn import(&mut self, update: &[u8]) -> Result<()> {
        let peer = self.doc.peer_id();
        let published = self.shared.get(&peer).copied().unwrap_or(0);

        self.doc.import(update)?;

        let mut version = self.doc.oplog_vv();
        version.insert(peer, published);
        self.shared = version;

        Ok(())
    }

    // --- Validation -------------------------------------------------------------------------

    /// Merge `update` into a throwaway fork and report which `(root, item)` pairs it changed.
    ///
    /// The fork's change events give us a resolved `path` for every changed container, which is
    /// exactly the "which item was this" question we need answered — no manual walk of the update
    /// bytes, and no dependence on how Loro chose to batch the ops.
    ///
    /// TODO: `LoroDoc::fork` is documented as O(n) in document size, so this is O(document) per
    /// remote update. Fine while a server fits in memory, wrong at scale. The shape of the fix is a
    /// long-lived staging doc that stays one update ahead of the real one and is only rebuilt on
    /// rejection — the same trick [`crate::data::auth::AuthGroup`] uses for the non-`Clone` group
    /// state.
    fn preview(&self, update: &[u8]) -> Result<(LoroDoc, BTreeSet<Touch>)> {
        // Fork at *committed* state, so a local edit in flight does not look like part of the
        // incoming update.
        self.doc.commit();
        let staging = self.doc.fork();

        let touched = Arc::new(Mutex::new(BTreeSet::new()));
        let sink = Arc::clone(&touched);
        let subscription = staging.subscribe_root(Arc::new(move |event| {
            let mut sink = sink.lock().expect("touch set is never poisoned");
            for container in &event.events {
                collect_touches(container.path, &container.diff, &mut sink);
            }
        }));

        staging.import(update)?;
        staging.commit();
        drop(subscription);

        let touched = touched.lock().expect("touch set is never poisoned").clone();
        Ok((staging, touched))
    }

    /// May `author` make this change to this item?
    ///
    /// Dispatches to the owning module, handing it the item as it was *before* the update (from our
    /// real document) and as it would be *after* (from the fork). Either may be `None`: a creation
    /// has no before, a deletion has no after.
    fn authorize(&self, staging: &LoroDoc, touch: &Touch, author: AuthorId) -> Result<()> {
        let Some(item) = touch.item.as_deref() else {
            // A change to a root container that is not keyed by item — the channel order list. Any
            // writer may reorder their view of the channel list; there is nothing to own.
            return match touch.root.as_str() {
                channel::ORDER_ROOT => Ok(()),
                other => Err(format!("update changes root container {other:?} wholesale").into()),
            };
        };

        match touch.root.as_str() {
            profile::ROOT => profile::authorize(item, author),
            channel::ROOT => channel::authorize(
                channel::read(&self.doc, item).as_ref(),
                channel::read(staging, item).as_ref(),
                author,
            ),
            message::ROOT => message::authorize(
                message::read(&self.doc, item).as_ref(),
                message::read(staging, item).as_ref(),
                author,
            ),
            reaction::ROOT => reaction::authorize(
                reaction::read(&self.doc, item).as_ref(),
                reaction::read(staging, item).as_ref(),
                author,
            ),
            thread::ROOT => thread::authorize(
                thread::read(&self.doc, item).as_ref(),
                thread::read(staging, item).as_ref(),
                author,
            ),
            other => Err(format!("update touches unknown root container {other:?}").into()),
        }
    }

    /// Refuse a local edit of somebody else's item before we ever publish it.
    ///
    /// Purely a courtesy to the local user: the enforcement that matters is every *other* peer
    /// running [`ChatDoc::import_checked`] on what we send.
    pub(crate) fn require_mine(&self, owner: AuthorId, what: &str) -> Result<()> {
        if owner != self.me {
            return Err(format!("{what} belongs to someone else").into());
        }
        Ok(())
    }
}

/// One `(root container, item id)` pair that an update changed.
///
/// `item` is `None` for a change to a root container that is not a map of items — in practice only
/// the channel order list.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Touch {
    root: String,
    item: Option<String>,
}

/// Turn one changed container into the item(s) it belongs to.
///
/// Loro hands us a `path` from the document down to the changed container, where each element is
/// `(that container, the key it sits under in its parent)`. So `path[0]` is the *root* container
/// paired with its own root name, and `path[1]`, when present, is the item under it. Since every
/// root here is a flat map of items or a flat list, that gives three cases:
///
/// * `path` has two or more elements — the change is inside an item (`messages/<id>` itself, or
///   `messages/<id>/text` below it). `path[1]`'s key is the item.
/// * `path` has one element and the diff is a map delta — keys were inserted into or deleted from a
///   root map, and each key in the delta is one item. Deletions only ever show up here, since a
///   deleted item has no path of its own any more.
/// * `path` has one element and the diff is anything else — a root changed as a whole, which in this
///   layout means the channel order list.
///
/// Anything else is recorded as `<unrecognised>` so that [`ChatDoc::authorize`] refuses it, rather
/// than being silently skipped. A shape we do not understand is a shape we cannot authorize.
fn collect_touches(path: &[(ContainerID, Index)], diff: &Diff, out: &mut BTreeSet<Touch>) {
    let mut unrecognised = || {
        out.insert(Touch {
            root: "<unrecognised>".to_string(),
            item: None,
        });
    };

    let Some(root) = path.first().and_then(|(id, _)| root_name(id)) else {
        unrecognised();
        return;
    };

    match path.get(1) {
        Some((_, Index::Key(item))) => {
            out.insert(Touch {
                root,
                item: Some(item.to_string()),
            });
        }
        Some(_) => unrecognised(),
        None => match diff {
            // Keys added to or removed from a root map: each key is one item.
            Diff::Map(delta) => {
                for key in delta.updated.keys() {
                    out.insert(Touch {
                        root: root.clone(),
                        item: Some(key.to_string()),
                    });
                }
            }
            // The channel order list, or anything else that changes a root as a whole.
            _ => {
                out.insert(Touch { root, item: None });
            }
        },
    }
}

/// The name of a root container, or `None` if it is not a root at all.
///
/// Deliberately does *not* filter to the roots we know: an unknown root name is passed through so
/// that [`ChatDoc::authorize`] can refuse it by name and say which one. The set of legal roots is
/// defined in exactly one place — the `match` in `authorize` — rather than in a list here that could
/// drift out of step with it.
fn root_name(id: &ContainerID) -> Option<String> {
    match id {
        ContainerID::Root { name, .. } => Some(name.to_string()),
        ContainerID::Normal { .. } => None,
    }
}

// --- Shared field accessors -------------------------------------------------------------------
//
// Every item is a `LoroMap` under a root map, holding plain (last-writer-wins) values plus, where
// concurrent editing has to merge character by character, a `LoroText` child. These turn that back
// into ordinary Rust values, treating "missing" and "wrong type" alike as `None` — a peer can put
// anything in a map, so reads must never panic on a malformed item.

pub(crate) fn item_map(doc: &LoroDoc, root: &str, id: &str) -> Option<LoroMap> {
    match doc.get_map(root).get(id) {
        Some(ValueOrContainer::Container(container)) => container.into_map().ok(),
        _ => None,
    }
}

pub(crate) fn str_field(map: &LoroMap, key: &str) -> Option<String> {
    match map.get(key) {
        Some(ValueOrContainer::Value(LoroValue::String(value))) => Some(value.to_string()),
        _ => None,
    }
}

pub(crate) fn i64_field(map: &LoroMap, key: &str) -> Option<i64> {
    match map.get(key) {
        Some(ValueOrContainer::Value(LoroValue::I64(value))) => Some(value),
        _ => None,
    }
}

pub(crate) fn bool_field(map: &LoroMap, key: &str) -> Option<bool> {
    match map.get(key) {
        Some(ValueOrContainer::Value(LoroValue::Bool(value))) => Some(value),
        _ => None,
    }
}

pub(crate) fn text_field(map: &LoroMap, key: &str) -> Option<LoroText> {
    match map.get(key) {
        Some(ValueOrContainer::Container(container)) => container.into_text().ok(),
        _ => None,
    }
}

/// Parse an author id out of a map key or a reference field.
///
/// `None` for anything that is not a valid key — a peer can write arbitrary strings, and an item we
/// cannot parse is one we simply do not show.
pub(crate) fn parse_author(hex: &str) -> Option<AuthorId> {
    AuthorId::from_str(hex).ok()
}

/// Parse an item id (channel, message, reaction, thread) out of a map key or a reference field.
pub(crate) fn parse_id(hex: &str) -> Option<Hash> {
    Hash::from_str(hex).ok()
}

/// The ids of every item in a root map, sorted, so listings are stable across peers.
pub(crate) fn item_ids(doc: &LoroDoc, root: &str) -> Vec<String> {
    let mut ids: Vec<String> = doc.get_map(root).keys().map(|key| key.to_string()).collect();
    ids.sort();
    ids
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A two-peer harness. Every test in this module tree is "Alice and Bobby each hold a document;
    //! what happens when they both write and then sync?", so the harness is that and nothing else.

    use super::*;
    use p2panda_core::SigningKey;

    /// A fresh author id. Real ones are person subgroup ids; for the data model any distinct key
    /// will do, since it only ever compares them.
    pub fn author() -> AuthorId {
        SigningKey::generate().verifying_key()
    }

    /// Seed a second device for the same person from `source`'s **whole** history.
    ///
    /// It has to be the whole history, not the latest update: one peer's Loro operations form a
    /// chain, so a document that starts from update N alone can never apply it — the ops it builds
    /// on are missing and Loro parks them as pending. This is the same property that makes rejecting
    /// an update terminal for a peer (see the module docs), met here in its benign form.
    pub fn second_device(source: &ChatDoc, author: AuthorId) -> Result<ChatDoc> {
        let mut device = ChatDoc::new(source.id(), author);
        source.doc().commit();
        device.import(&source.doc().export(ExportMode::all_updates())?)?;
        Ok(device)
    }

    /// Two documents on the same server, one per person.
    pub struct Pair {
        pub alice: ChatDoc,
        pub bobby: ChatDoc,
    }

    impl Pair {
        pub fn new() -> (Self, AuthorId, AuthorId) {
            let (alice_id, bobby_id) = (author(), author());
            let server = ServerId::random();
            (
                Self {
                    alice: ChatDoc::new(server, alice_id),
                    bobby: ChatDoc::new(server, bobby_id),
                },
                alice_id,
                bobby_id,
            )
        }

        /// Push Alice's pending changes to Bobby, with the ownership check that a real peer applies.
        pub fn alice_to_bobby(&mut self) -> Result<()> {
            let author = self.alice.me();
            match self.alice.export()? {
                Some(update) => self.bobby.import_checked(&update, author),
                None => Ok(()),
            }
        }

        pub fn bobby_to_alice(&mut self) -> Result<()> {
            let author = self.bobby.me();
            match self.bobby.export()? {
                Some(update) => self.alice.import_checked(&update, author),
                None => Ok(()),
            }
        }

        /// Exchange in both directions, so both documents end up at the same version.
        pub fn sync(&mut self) -> Result<()> {
            let from_alice = self.alice.export()?;
            let from_bobby = self.bobby.export()?;
            let (alice_id, bobby_id) = (self.alice.me(), self.bobby.me());
            if let Some(update) = from_alice {
                self.bobby.import_checked(&update, alice_id)?;
            }
            if let Some(update) = from_bobby {
                self.alice.import_checked(&update, bobby_id)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    //! Cross-cutting properties of [`ChatDoc`] itself. Per-type rules live in each type's module.

    use super::test_support::*;
    use super::*;

    /// The property the whole validate-before-merge design exists for: a rejected update leaves the
    /// document exactly as it was, including the parts of the update that were perfectly valid.
    #[test]
    fn a_rejected_update_is_rejected_whole() -> Result<()> {
        let (mut pair, alice_id, _) = Pair::new();

        let channel = pair.alice.create_channel("general", 1_000)?;
        pair.alice_to_bobby()?;

        // Bobby writes one legitimate message and then tampers with Alice's profile, in one update.
        pair.bobby.post_message(channel, "hello", 2_000)?;
        pair.bobby
            .doc()
            .get_map(profile::ROOT)
            .ensure_mergeable_map(&alice_id.to_hex())?
            .insert("name", "not alice")?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&update, author).is_err());

        // Neither half landed. The legitimate message is collateral damage, and that is the point:
        // an update is all-or-nothing, so every peer that rejects it rejects the same bytes.
        assert!(pair.alice.channel_messages(channel).is_empty());
        assert!(pair.alice.profile(alice_id).is_none());
        Ok(())
    }

    /// The other side of atomic rejection, and the sharp edge of it: a peer whose update we refuse
    /// is not merely missing that update, they are **cut off**. Their next update builds on the ops
    /// we rejected, so Loro parks it as pending and it never materialises.
    ///
    /// This test exists to pin that behaviour down rather than to endorse it — see the module docs
    /// for why V1 accepts it and what fixing it would take.
    #[test]
    fn a_rejected_update_silences_that_peer_from_then_on() -> Result<()> {
        let (mut pair, alice_id, _) = Pair::new();

        pair.bobby
            .doc()
            .get_map(profile::ROOT)
            .ensure_mergeable_map(&alice_id.to_hex())?
            .insert("name", "not alice")?;
        let author = pair.bobby.me();
        let bad = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&bad, author).is_err());

        // Bobby now behaves perfectly, and it makes no difference.
        let channel = pair.alice.create_channel("general", 1_000)?;
        pair.alice_to_bobby()?;
        pair.bobby.post_message(channel, "hello", 2_000)?;
        pair.bobby_to_alice()?;

        assert!(
            pair.alice.channel_messages(channel).is_empty(),
            "a peer whose update was rejected stays cut off; see the module docs"
        );

        // Alice's own document is otherwise unharmed — the rejection left no fork state behind.
        pair.alice.post_message(channel, "still working", 3_000)?;
        assert_eq!(pair.alice.channel_messages(channel).len(), 1);
        Ok(())
    }

    /// p2panda promises at-least-once delivery, echoes our own operations back to us, and replays
    /// the whole log on restart — so duplicates are the norm, not an edge case.
    #[test]
    fn reimporting_the_same_update_is_idempotent() -> Result<()> {
        let (mut pair, _, _) = Pair::new();

        let channel = pair.alice.create_channel("general", 1_000)?;
        pair.alice.post_message(channel, "hello", 2_000)?;

        let author = pair.alice.me();
        let update = pair.alice.export()?.expect("alice has changes");
        pair.bobby.import_checked(&update, author)?;
        pair.bobby.import_checked(&update, author)?;

        assert_eq!(pair.bobby.channels().len(), 1);
        assert_eq!(pair.bobby.channel_messages(channel).len(), 1);
        Ok(())
    }

    /// New root containers cannot be introduced by a peer. This is what keeps the "six roots and
    /// nothing else" layout an actual invariant rather than a convention — an update from a newer
    /// (or hostile) build carrying unknown state is refused instead of silently accumulating.
    #[test]
    fn an_update_touching_an_unknown_root_is_refused() -> Result<()> {
        let (mut pair, _, _) = Pair::new();

        pair.bobby.doc().get_map("something_else").insert("k", 1)?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("unknown root must be refused");
        assert!(err.to_string().contains("unknown root"), "{err}");
        Ok(())
    }

    /// Export is relative to what we have already shared, so nothing is sent twice and — the part
    /// that is easy to get wrong — a local edit made while a remote update was being merged is not
    /// swallowed.
    #[test]
    fn a_local_edit_during_a_merge_is_still_published() -> Result<()> {
        let (mut pair, _, _) = Pair::new();

        let channel = pair.alice.create_channel("general", 1_000)?;
        pair.alice_to_bobby()?;

        // Bobby writes locally, then merges Alice's next update before publishing his own.
        pair.bobby.post_message(channel, "from bobby", 2_000)?;
        pair.alice.post_message(channel, "from alice", 2_001)?;
        pair.alice_to_bobby()?;

        // Bobby's own message must still be in what he exports.
        pair.bobby_to_alice()?;
        assert_eq!(pair.alice.channel_messages(channel).len(), 2);
        Ok(())
    }
}
