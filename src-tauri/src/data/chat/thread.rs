// SPDX-License-Identifier: MIT OR Apache-2.0

//! Threads: a side conversation hanging off one message.
//!
//! CHAT.md describes a thread as anchored to the message where it starts, and attached "like a
//! reaction". This module takes that literally, including the derived id:
//!
//! ## One message, at most one thread
//!
//! A thread id is `H(anchor message id)`. That makes "start a thread on this message" idempotent and
//! conflict-free: two people clicking it at the same moment produce the *same* thread rather than
//! two competing threads on one message, and neither has to have seen the other's write. It also
//! means the thread for a message can be named without a lookup — [`ChatDoc::thread_messages`] and
//! the UI can both compute it from the anchor alone.
//!
//! The constraint it imposes is in the name: one thread per message. That is a real limit, and it is
//! the right one for V1 — "which of this message's four threads did you mean" is a worse problem to
//! have than "you cannot have four".
//!
//! ## Threads are immutable
//!
//! A thread has no editable state at all. It is created once, anchored to a message, and after that
//! only its *messages* change. There is no rename and no delete: renaming is a feature nobody has
//! asked for yet, and deleting would strand the messages whose `thread` field points here. If a
//! thread needs a title later, that is an added mutable field and a rule in [`authorize`] to go with
//! it, not a change to anything else.
//!
//! The derived id costs one thing, and it is worth naming: `created_by` and `created_at` are
//! **advisory**. Two people starting the same thread at the same moment write the same key with
//! different values for both, so a later change to either is indistinguishable from an honest
//! concurrent creation and [`authorize`] cannot refuse it. `channel` and `anchor` are the fields
//! that are genuinely pinned — and `anchor` is pinned twice over, since the id hashes from it.
//!
//! ## Anyone may start one
//!
//! Like channels, and unlike messages and reactions, a thread is not owned. Any writer may start a
//! thread on any message, including someone else's — that is the entire point of threading. The
//! `created_by` field is provenance only, and immutable like everything else here.
//!
//! ## The anchor may not exist yet
//!
//! A thread naming a message we have not synced is stored and resolves later, like every other
//! reference in this model. See the module docs on [`super`].

use loro::LoroDoc;
use p2panda_core::Hash;

use super::channel::ChannelId;
use super::message::MessageId;
use super::{AuthorId, ChatDoc, i64_field, parse_author, parse_id, str_field};
use crate::Result;

/// Root `LoroMap`: thread id (hex) -> thread map.
pub const ROOT: &str = "threads";

const CHANNEL: &str = "channel";
const ANCHOR: &str = "anchor";
const CREATED_BY: &str = "created_by";
const CREATED_AT: &str = "created_at";

pub type ThreadId = Hash;

/// A side conversation hanging off one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub id: ThreadId,
    /// The channel the anchor message is in. Threads do not cross channels.
    pub channel: ChannelId,
    /// The message this thread hangs off.
    pub anchor: MessageId,
    pub created_by: AuthorId,
    pub created_at: i64,
}

/// The deterministic id of the thread anchored to `anchor`. One message, one thread.
pub fn thread_id(anchor: MessageId) -> ThreadId {
    let mut input = Vec::from(b"p2panda-chat:thread:".as_slice());
    input.extend_from_slice(anchor.as_bytes());
    Hash::digest(&input)
}

/// Read one thread, or `None` if it does not exist or is malformed.
pub fn read(doc: &LoroDoc, id: &str) -> Option<Thread> {
    let map = super::item_map(doc, ROOT, id)?;
    Some(Thread {
        id: parse_id(id)?,
        channel: parse_id(&str_field(&map, CHANNEL)?)?,
        anchor: parse_id(&str_field(&map, ANCHOR)?)?,
        created_by: parse_author(&str_field(&map, CREATED_BY)?)?,
        created_at: i64_field(&map, CREATED_AT).unwrap_or(0),
    })
}

/// May `author` make this change to this thread?
///
/// Anyone may create one; nobody may change or remove one.
pub fn authorize(before: Option<&Thread>, after: Option<&Thread>, author: AuthorId) -> Result<()> {
    match (before, after) {
        (None, Some(after)) => {
            if after.created_by != author {
                return Err("a thread must be created in the creator's own name".into());
            }
            // The derived id is what guarantees one thread per message. A thread stored under any
            // other key would be a second thread on the same anchor, or a thread shadowing an
            // unrelated message's.
            if thread_id(after.anchor) != after.id {
                return Err("a thread's id must be derived from its anchor message".into());
            }
            Ok(())
        }
        (Some(before), Some(after)) => {
            // What the thread *is* — where it lives and what it hangs off — may never change.
            if before.channel != after.channel || before.anchor != after.anchor {
                return Err("a thread's channel and anchor are immutable".into());
            }
            // `created_by` and `created_at` deliberately are *not* checked here, and that is a
            // consequence of the derived id rather than an oversight. Two people starting the same
            // thread concurrently write the same key with different creators and different clocks;
            // Loro resolves that to one of them, so a change to either field is indistinguishable
            // from an honest concurrent creation. They are therefore advisory — fine for showing
            // "started by", not something to rely on.
            Ok(())
        }
        (Some(_), None) => Err("threads cannot be deleted".into()),
        (None, None) => Err("malformed thread".into()),
    }
}

impl ChatDoc {
    /// Start (or re-affirm) the thread on a message. Idempotent: the id is derived from the anchor,
    /// so two people starting the same thread produce one thread.
    pub fn start_thread(&self, channel: ChannelId, anchor: MessageId, now: i64) -> Result<ThreadId> {
        let id = thread_id(anchor);
        if super::item_map(self.doc(), ROOT, &id.to_hex()).is_some() {
            return Ok(id);
        }

        let map = self
            .doc()
            .get_map(ROOT)
            .ensure_mergeable_map(&id.to_hex())?;
        map.insert(CHANNEL, channel.to_hex())?;
        map.insert(ANCHOR, anchor.to_hex())?;
        map.insert(CREATED_BY, self.me().to_hex())?;
        map.insert(CREATED_AT, now)?;

        Ok(id)
    }

    pub fn thread(&self, id: ThreadId) -> Option<Thread> {
        read(self.doc(), &id.to_hex())
    }

    /// The thread on a message, if one has been started.
    pub fn thread_on(&self, anchor: MessageId) -> Option<Thread> {
        self.thread(thread_id(anchor))
    }

    /// Every thread in a channel, ordered by id so peers agree.
    pub fn channel_threads(&self, channel: ChannelId) -> Vec<Thread> {
        super::item_ids(self.doc(), ROOT)
            .iter()
            .filter_map(|id| read(self.doc(), id))
            .filter(|thread| thread.channel == channel)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use loro::LoroMap;

    use super::super::test_support::*;
    use super::*;

    fn with_message() -> Result<(Pair, AuthorId, AuthorId, ChannelId, MessageId)> {
        let (mut pair, alice_id, bobby_id) = Pair::new();
        let channel = pair.alice.create_channel("general", 1_000)?;
        let message = pair.alice.post_message(channel, "let's discuss", 2_000)?;
        pair.alice_to_bobby()?;
        Ok((pair, alice_id, bobby_id, channel, message))
    }

    /// A thread's messages leave the channel's main flow and appear in the thread instead — the
    /// behaviour CHAT.md asks for.
    #[test]
    fn thread_messages_leave_the_main_flow() -> Result<()> {
        let (mut pair, _, _, channel, anchor) = with_message()?;

        let thread = pair.bobby.start_thread(channel, anchor, 3_000)?;
        pair.bobby.post_to_thread(channel, thread, "in thread", 3_001)?;
        pair.bobby.post_message(channel, "in channel", 3_002)?;
        pair.bobby_to_alice()?;

        let main: Vec<String> = pair
            .alice
            .channel_messages(channel)
            .iter()
            .map(|m| m.text.clone())
            .collect();
        let in_thread: Vec<String> = pair
            .alice
            .thread_messages(thread)
            .iter()
            .map(|m| m.text.clone())
            .collect();

        assert_eq!(main, ["let's discuss", "in channel"]);
        assert_eq!(in_thread, ["in thread"]);
        Ok(())
    }

    /// Anyone may thread anyone's message. This is the intended permissiveness, not an oversight.
    #[test]
    fn a_writer_may_start_a_thread_on_someone_elses_message() -> Result<()> {
        let (mut pair, _, bobby_id, channel, anchor) = with_message()?;

        let thread = pair.bobby.start_thread(channel, anchor, 3_000)?;
        pair.bobby_to_alice()?;

        let thread = pair.alice.thread(thread).expect("thread");
        assert_eq!(thread.anchor, anchor);
        assert_eq!(thread.created_by, bobby_id);
        assert_eq!(pair.alice.thread_on(anchor).expect("thread").id, thread.id);
        Ok(())
    }

    /// The payoff of the derived id: two people starting a thread on the same message at the same
    /// moment get one thread, not two competing ones.
    #[test]
    fn two_people_starting_the_same_thread_get_one_thread() -> Result<()> {
        let (mut pair, _, _, channel, anchor) = with_message()?;

        let from_alice = pair.alice.start_thread(channel, anchor, 3_000)?;
        let from_bobby = pair.bobby.start_thread(channel, anchor, 3_001)?;
        assert_eq!(from_alice, from_bobby, "the id derives from the anchor");

        pair.sync()?;

        assert_eq!(pair.alice.channel_threads(channel).len(), 1);
        assert_eq!(pair.bobby.channel_threads(channel).len(), 1);
        assert_eq!(
            pair.alice.thread(from_alice),
            pair.bobby.thread(from_alice),
            "peers must converge on one thread"
        );
        Ok(())
    }

    /// A thread stored under an id that does not derive from its anchor is refused — that is how the
    /// one-thread-per-message invariant is enforced against a peer that ignores it.
    #[test]
    fn a_thread_under_the_wrong_id_is_refused() -> Result<()> {
        let (mut pair, _, bobby_id, channel, anchor) = with_message()?;

        let map = pair
            .bobby
            .doc()
            .get_map(ROOT)
            .insert_container(&Hash::digest(b"second thread").to_hex(), LoroMap::new())?;
        map.insert(CHANNEL, channel.to_hex())?;
        map.insert(ANCHOR, anchor.to_hex())?;
        map.insert(CREATED_BY, bobby_id.to_hex())?;
        map.insert(CREATED_AT, 3_000)?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("a mis-keyed thread must be refused");
        assert!(err.to_string().contains("derived from its anchor"), "{err}");
        Ok(())
    }

    #[test]
    fn a_thread_cannot_be_created_in_someone_elses_name() -> Result<()> {
        let (mut pair, alice_id, _, channel, anchor) = with_message()?;

        let map = pair
            .bobby
            .doc()
            .get_map(ROOT)
            .insert_container(&thread_id(anchor).to_hex(), LoroMap::new())?;
        map.insert(CHANNEL, channel.to_hex())?;
        map.insert(ANCHOR, anchor.to_hex())?;
        map.insert(CREATED_BY, alice_id.to_hex())?;
        map.insert(CREATED_AT, 3_000)?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&update, author).is_err());
        assert!(pair.alice.thread_on(anchor).is_none());
        Ok(())
    }

    #[test]
    fn a_thread_cannot_be_re_anchored() -> Result<()> {
        let (mut pair, _, _, channel, anchor) = with_message()?;

        let thread = pair.bobby.start_thread(channel, anchor, 3_000)?;
        pair.bobby_to_alice()?;

        super::super::item_map(pair.bobby.doc(), ROOT, &thread.to_hex())
            .expect("thread")
            .insert(ANCHOR, Hash::digest(b"somewhere else").to_hex())?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("re-anchoring a thread must be refused");
        assert!(err.to_string().contains("immutable"), "{err}");
        assert_eq!(pair.alice.thread(thread).expect("thread").anchor, anchor);
        Ok(())
    }

    #[test]
    fn a_thread_cannot_be_deleted() -> Result<()> {
        let (mut pair, _, _, channel, anchor) = with_message()?;

        let thread = pair.bobby.start_thread(channel, anchor, 3_000)?;
        pair.bobby_to_alice()?;

        pair.bobby.doc().get_map(ROOT).delete(&thread.to_hex())?;
        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("deleting a thread must be refused");
        assert!(err.to_string().contains("cannot be deleted"), "{err}");
        assert!(pair.alice.thread(thread).is_some());
        Ok(())
    }

    /// A thread whose anchor has not arrived is accepted and resolves when the message does.
    #[test]
    fn a_thread_on_an_unknown_message_is_accepted() -> Result<()> {
        let (mut pair, _, _, channel, _) = with_message()?;

        let unknown = Hash::digest(b"not yet synced");
        let thread = pair.bobby.start_thread(channel, unknown, 3_000)?;
        pair.bobby_to_alice()?;

        assert_eq!(pair.alice.thread(thread).expect("thread").anchor, unknown);
        assert!(pair.alice.message(unknown).is_none());
        Ok(())
    }
}
