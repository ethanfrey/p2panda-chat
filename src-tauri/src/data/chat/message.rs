// SPDX-License-Identifier: MIT OR Apache-2.0

//! Messages: the actual chat. One flat map, keyed by message id.
//!
//! This is the type where every design decision in this module tree gets exercised at once, so it is
//! worth reading the four of them together.
//!
//! ## 1. The body is a `LoroText`, everything else is a value
//!
//! `text` is the one field where character-level merging earns its keep: CHAT.md wants message
//! editing, and a person editing the same message from their laptop and their phone should not have
//! one edit silently clobber the other. `LoroText::update` computes a minimal diff and applies it as
//! inserts and deletes, so the two edits interleave the way a text CRDT is supposed to make them.
//!
//! Every other field — `channel`, `author`, `deleted`, the timestamps — is a plain last-writer-wins
//! value. Merging a boolean character-wise is nonsense; merging a channel id character-wise is
//! actively dangerous.
//!
//! ## 2. Only the author's subgroup may edit
//!
//! [`authorize`] is where CHAT.md's "author subgroup can modify" becomes an enforced rule rather than
//! a UI convention. Because [`super::AuthorId`] is a *person* subgroup and not a device, this works
//! across your devices for free — you can edit from your phone a message you posted from your
//! laptop, and no device-level bookkeeping is needed to make that true.
//!
//! The check is stated over the message *before* the update, not after. That closes the obvious
//! attack: Mallory rewriting both the body and the `author` field of Bob's message in one update
//! would pass a check that only looked at the result.
//!
//! ## 3. Deleting is a tombstone, not a removal
//!
//! A "deleted" message keeps its key, its author and its position; only `deleted` flips. Hard
//! removal is refused outright by [`authorize`], for the same reason channels cannot be deleted:
//! replies and reactions point at message ids, and a peer that has not yet synced them cannot
//! distinguish "removed" from "not arrived yet".
//!
//! The body is *not* cleared on delete. Whether to keep it is a policy question — keeping it means
//! "delete" is really "retract", and any peer that synced before the deletion still has the text
//! anyway, which is the honest thing for a p2p system to admit. The UI shows a tombstone;
//! [`Message::text`] still carries what was written.
//!
//! ## 4. Ordering is `(created_at, id)`, and `created_at` is not trustworthy
//!
//! See the module docs on [`super`]. The short version: the timestamp is a hint supplied by the
//! author, the id is what makes the sort *total and identical on every peer*, and CHAT.md's causal
//! ordering idea is deferred with [`Message::reply_to`] recording the edge in the meantime.
//!
//! ## Where a message shows up
//!
//! A message names its `channel` always, and a `thread` sometimes. A message with `thread: None` is
//! in the channel's main flow; a message with `thread: Some(..)` is in that thread and *not* in the
//! main flow. Both fields are immutable, so a message cannot be moved between channels or dragged
//! into a thread after the fact — that would reorder other people's conversations retroactively.

use loro::{LoroDoc, LoroMap, LoroText, UpdateOptions};
use p2panda_core::Hash;

use super::channel::ChannelId;
use super::thread::ThreadId;
use super::{
    AuthorId, ChatDoc, ServerId, bool_field, i64_field, parse_author, parse_id, str_field,
    text_field,
};
use crate::Result;

/// Root `LoroMap`: message id (hex) -> message map.
pub const ROOT: &str = "messages";

const AUTHOR: &str = "author";
const CHANNEL: &str = "channel";
const THREAD: &str = "thread";
const REPLY_TO: &str = "reply_to";
const TEXT: &str = "text";
const CREATED_AT: &str = "created_at";
const EDITED_AT: &str = "edited_at";
const DELETED: &str = "deleted";

pub type MessageId = Hash;

/// One message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    /// The person subgroup that wrote it. Immutable.
    pub author: AuthorId,
    /// The channel it belongs to. Immutable.
    pub channel: ChannelId,
    /// The thread it belongs to, if any. Immutable. `None` means the channel's main flow.
    pub thread: Option<ThreadId>,
    /// The message being replied to, if any. Immutable, and may name a message we have not synced
    /// yet — see the module docs on dangling references.
    pub reply_to: Option<MessageId>,
    /// The body. Merges character by character across concurrent edits.
    pub text: String,
    /// Author-supplied wall-clock milliseconds. Untrusted; used for ordering as a hint only.
    pub created_at: i64,
    /// When the body was last edited, if it ever was. The UI uses this to show "(edited)".
    pub edited_at: Option<i64>,
    /// Retracted by its author. The message keeps its place; the body is still present.
    pub deleted: bool,
}

/// Read one message, or `None` if it does not exist or is malformed.
pub fn read(doc: &LoroDoc, id: &str) -> Option<Message> {
    let map = super::item_map(doc, ROOT, id)?;
    Some(Message {
        id: parse_id(id)?,
        author: parse_author(&str_field(&map, AUTHOR)?)?,
        channel: parse_id(&str_field(&map, CHANNEL)?)?,
        thread: str_field(&map, THREAD).and_then(|id| parse_id(&id)),
        reply_to: str_field(&map, REPLY_TO).and_then(|id| parse_id(&id)),
        text: text_field(&map, TEXT)
            .map(|text| text.to_string())
            .unwrap_or_default(),
        created_at: i64_field(&map, CREATED_AT).unwrap_or(0),
        edited_at: i64_field(&map, EDITED_AT),
        deleted: bool_field(&map, DELETED).unwrap_or(false),
    })
}

/// May `author` make this change to this message?
///
/// Create your own; edit your own; never delete outright.
pub fn authorize(before: Option<&Message>, after: Option<&Message>, author: AuthorId) -> Result<()> {
    match (before, after) {
        (None, Some(after)) => {
            if after.author != author {
                return Err("a message must be posted in the author's own name".into());
            }
            Ok(())
        }
        (Some(before), Some(after)) => {
            // Judged on who owned it *before* the update, so rewriting `author` in the same update
            // does not launder the message.
            if before.author != author {
                return Err("only the author's subgroup may edit a message".into());
            }
            if before.author != after.author
                || before.channel != after.channel
                || before.thread != after.thread
                || before.reply_to != after.reply_to
                || before.created_at != after.created_at
            {
                return Err(
                    "a message's author, channel, thread, reply and creation time are immutable"
                        .into(),
                );
            }
            Ok(())
        }
        (Some(_), None) => {
            Err("messages cannot be removed, only marked deleted".into())
        }
        (None, None) => Err("malformed message".into()),
    }
}

impl ChatDoc {
    /// Post a message to a channel's main flow.
    pub fn post_message(&self, channel: ChannelId, body: &str, now: i64) -> Result<MessageId> {
        self.write_message(channel, None, None, body, now)
    }

    /// Post a message as a reply to another. The reply still lives in the channel's main flow;
    /// `reply_to` is a reference for display, not a container.
    pub fn reply_to_message(
        &self,
        channel: ChannelId,
        reply_to: MessageId,
        body: &str,
        now: i64,
    ) -> Result<MessageId> {
        self.write_message(channel, None, Some(reply_to), body, now)
    }

    /// Post a message into a thread. It will not appear in the channel's main flow.
    pub fn post_to_thread(
        &self,
        channel: ChannelId,
        thread: ThreadId,
        body: &str,
        now: i64,
    ) -> Result<MessageId> {
        self.write_message(channel, Some(thread), None, body, now)
    }

    fn write_message(
        &self,
        channel: ChannelId,
        thread: Option<ThreadId>,
        reply_to: Option<MessageId>,
        body: &str,
        now: i64,
    ) -> Result<MessageId> {
        let id: MessageId = ServerId::random().into();

        let map = self
            .doc()
            .get_map(ROOT)
            .insert_container(&id.to_hex(), LoroMap::new())?;
        map.insert(AUTHOR, self.me().to_hex())?;
        map.insert(CHANNEL, channel.to_hex())?;
        if let Some(thread) = thread {
            map.insert(THREAD, thread.to_hex())?;
        }
        if let Some(reply_to) = reply_to {
            map.insert(REPLY_TO, reply_to.to_hex())?;
        }
        map.insert(CREATED_AT, now)?;
        map.insert(DELETED, false)?;
        map.insert_container(TEXT, LoroText::new())?
            .insert(0, body)?;

        Ok(id)
    }

    /// Edit our own message.
    ///
    /// `LoroText::update` applies the minimal set of inserts and deletes, which is what lets two of
    /// our devices edit the same message concurrently and have both edits survive.
    pub fn edit_message(&self, id: MessageId, body: &str, now: i64) -> Result<()> {
        let message = self
            .message(id)
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                format!("unknown message {id}").into()
            })?;
        self.require_mine(message.author, "this message")?;

        let map = self.message_map(id)?;
        let text = text_field(&map, TEXT)
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                format!("message {id} has no body").into()
            })?;
        text.update(body, UpdateOptions::default())
            .map_err(|err| format!("could not edit message: {err}"))?;
        map.insert(EDITED_AT, now)?;
        Ok(())
    }

    /// Retract our own message. The entry stays; `deleted` flips. See the module docs.
    pub fn delete_message(&self, id: MessageId) -> Result<()> {
        let message = self
            .message(id)
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                format!("unknown message {id}").into()
            })?;
        self.require_mine(message.author, "this message")?;
        self.message_map(id)?.insert(DELETED, true)?;
        Ok(())
    }

    pub fn message(&self, id: MessageId) -> Option<Message> {
        read(self.doc(), &id.to_hex())
    }

    /// A channel's main flow: everything in it that is not in a thread, oldest first.
    pub fn channel_messages(&self, channel: ChannelId) -> Vec<Message> {
        self.messages_where(|message| message.channel == channel && message.thread.is_none())
    }

    /// A thread's messages, oldest first.
    pub fn thread_messages(&self, thread: ThreadId) -> Vec<Message> {
        self.messages_where(|message| message.thread == Some(thread))
    }

    /// Every message matching a predicate, in the agreed order.
    ///
    /// Sorted by `(created_at, id)`: the timestamp is the intent, the id is what makes the result
    /// identical on every peer no matter what order things merged in.
    fn messages_where(&self, keep: impl Fn(&Message) -> bool) -> Vec<Message> {
        let mut messages: Vec<Message> = super::item_ids(self.doc(), ROOT)
            .iter()
            .filter_map(|id| read(self.doc(), id))
            .filter(|message| keep(message))
            .collect();
        messages.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.to_hex().cmp(&b.id.to_hex()))
        });
        messages
    }

    fn message_map(&self, id: MessageId) -> Result<LoroMap> {
        super::item_map(self.doc(), ROOT, &id.to_hex())
            .ok_or_else(|| format!("unknown message {id}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    /// Set up a synced pair with one channel, since almost every test needs exactly that.
    fn with_channel() -> Result<(Pair, AuthorId, AuthorId, ChannelId)> {
        let (mut pair, alice_id, bobby_id) = Pair::new();
        let channel = pair.alice.create_channel("general", 1_000)?;
        pair.alice_to_bobby()?;
        Ok((pair, alice_id, bobby_id, channel))
    }

    #[test]
    fn a_message_is_posted_and_replicates() -> Result<()> {
        let (mut pair, alice_id, _, channel) = with_channel()?;

        let id = pair.alice.post_message(channel, "hello world", 2_000)?;
        pair.alice_to_bobby()?;

        let message = pair.bobby.message(id).expect("message");
        assert_eq!(message.text, "hello world");
        assert_eq!(message.author, alice_id);
        assert_eq!(message.channel, channel);
        assert_eq!(message.created_at, 2_000);
        assert_eq!(message.edited_at, None);
        assert!(!message.deleted);
        assert!(message.thread.is_none());
        Ok(())
    }

    /// Messages come back oldest-first regardless of the order they were merged in.
    #[test]
    fn a_channel_reads_back_in_timestamp_order() -> Result<()> {
        let (mut pair, _, _, channel) = with_channel()?;

        pair.alice.post_message(channel, "third", 3_000)?;
        pair.alice.post_message(channel, "first", 1_000)?;
        pair.alice.post_message(channel, "second", 2_000)?;
        pair.alice_to_bobby()?;

        let bodies: Vec<String> = pair
            .bobby
            .channel_messages(channel)
            .iter()
            .map(|m| m.text.clone())
            .collect();
        assert_eq!(bodies, ["first", "second", "third"]);
        Ok(())
    }

    /// The property the id tie-break exists for: identical timestamps must not leave the order up to
    /// merge order, or two people would see the same conversation differently.
    #[test]
    fn messages_with_identical_timestamps_are_ordered_identically_on_both_peers() -> Result<()> {
        let (mut pair, _, _, channel) = with_channel()?;

        // Same millisecond, different authors, and each peer learns of its own first.
        pair.alice.post_message(channel, "from alice", 2_000)?;
        pair.bobby.post_message(channel, "from bobby", 2_000)?;
        pair.sync()?;

        let alice_view: Vec<String> = pair
            .alice
            .channel_messages(channel)
            .iter()
            .map(|m| m.text.clone())
            .collect();
        let bobby_view: Vec<String> = pair
            .bobby
            .channel_messages(channel)
            .iter()
            .map(|m| m.text.clone())
            .collect();
        assert_eq!(alice_view.len(), 2);
        assert_eq!(alice_view, bobby_view, "peers must agree on message order");
        Ok(())
    }

    /// The reason `text` is a `LoroText`: one person editing from two devices at once keeps both
    /// edits instead of one overwriting the other.
    #[test]
    fn concurrent_edits_by_the_authors_own_devices_merge() -> Result<()> {
        let (mut pair, alice_id, _, channel) = with_channel()?;

        // Alice's second device, seeded from her whole history.
        let id = pair.alice.post_message(channel, "Buy milk", 2_000)?;
        let mut phone = second_device(&pair.alice, alice_id)?;

        pair.alice.edit_message(id, "Buy oat milk", 3_000)?;
        phone.edit_message(id, "Buy milk today", 3_001)?;

        let from_laptop = pair.alice.export()?.expect("laptop has changes");
        let from_phone = phone.export()?.expect("phone has changes");
        pair.alice.import_checked(&from_phone, alice_id)?;
        phone.import_checked(&from_laptop, alice_id)?;

        let merged = pair.alice.message(id).expect("message").text;
        assert_eq!(merged, phone.message(id).expect("message").text);
        assert!(
            merged.contains("oat") && merged.contains("today"),
            "an edit was dropped: {merged}"
        );
        Ok(())
    }

    /// The central access rule. Bobby may write in this channel and still may not touch Alice's
    /// message.
    #[test]
    fn a_writer_may_not_edit_someone_elses_message() -> Result<()> {
        let (mut pair, _, _, channel) = with_channel()?;

        let id = pair.alice.post_message(channel, "hello", 2_000)?;
        pair.alice_to_bobby()?;

        // Local API refuses...
        assert!(pair.bobby.edit_message(id, "tampered", 3_000).is_err());

        // ...and so does a peer, when Bobby writes to the container directly.
        text_field(
            &super::super::item_map(pair.bobby.doc(), ROOT, &id.to_hex()).expect("message"),
            TEXT,
        )
        .expect("body")
        .update("tampered", UpdateOptions::default())
        .expect("local write");

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("editing another author's message must be refused");
        assert!(err.to_string().contains("only the author"), "{err}");
        assert_eq!(pair.alice.message(id).expect("message").text, "hello");
        Ok(())
    }

    /// Rewriting `author` in the same update as the edit must not launder the message. This is why
    /// [`authorize`] judges on the before-state.
    #[test]
    fn a_writer_may_not_edit_a_message_by_claiming_it_first() -> Result<()> {
        let (mut pair, _, bobby_id, channel) = with_channel()?;

        let id = pair.alice.post_message(channel, "hello", 2_000)?;
        pair.alice_to_bobby()?;

        let map = super::super::item_map(pair.bobby.doc(), ROOT, &id.to_hex()).expect("message");
        map.insert(AUTHOR, bobby_id.to_hex())?;
        text_field(&map, TEXT)
            .expect("body")
            .update("tampered", UpdateOptions::default())
            .expect("local write");

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&update, author).is_err());
        assert_eq!(pair.alice.message(id).expect("message").author, pair.alice.me());
        assert_eq!(pair.alice.message(id).expect("message").text, "hello");
        Ok(())
    }

    /// Posting in someone else's name is caught at creation, where there is no before-state.
    #[test]
    fn a_message_cannot_be_posted_in_someone_elses_name() -> Result<()> {
        let (mut pair, alice_id, _, channel) = with_channel()?;

        let map = pair
            .bobby
            .doc()
            .get_map(ROOT)
            .insert_container(&Hash::digest(b"forged").to_hex(), LoroMap::new())?;
        map.insert(AUTHOR, alice_id.to_hex())?;
        map.insert(CHANNEL, channel.to_hex())?;
        map.insert(CREATED_AT, 2_000)?;
        map.insert_container(TEXT, LoroText::new())?
            .insert(0, "alice did not write this")?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&update, author).is_err());
        assert!(pair.alice.channel_messages(channel).is_empty());
        Ok(())
    }

    /// Even the author may not move their message to another channel after the fact — that would
    /// retroactively reorder a conversation other people have already read.
    #[test]
    fn a_message_cannot_be_moved_between_channels() -> Result<()> {
        let (mut pair, _, _, channel) = with_channel()?;
        let other = pair.alice.create_channel("other", 1_001)?;

        let id = pair.bobby.post_message(channel, "hello", 2_000)?;
        pair.bobby_to_alice()?;

        super::super::item_map(pair.bobby.doc(), ROOT, &id.to_hex())
            .expect("message")
            .insert(CHANNEL, other.to_hex())?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("moving a message must be refused");
        assert!(err.to_string().contains("immutable"), "{err}");
        Ok(())
    }

    /// Deletion is a tombstone: the message keeps its place in the flow, and replies to it still
    /// resolve.
    #[test]
    fn deleting_a_message_tombstones_it() -> Result<()> {
        let (mut pair, _, _, channel) = with_channel()?;

        let id = pair.alice.post_message(channel, "oops", 2_000)?;
        pair.alice.delete_message(id)?;
        pair.alice_to_bobby()?;

        let message = pair.bobby.message(id).expect("message still exists");
        assert!(message.deleted);
        assert_eq!(pair.bobby.channel_messages(channel).len(), 1);
        Ok(())
    }

    /// Hard removal is refused for everyone, the author included.
    #[test]
    fn a_message_cannot_be_removed_outright() -> Result<()> {
        let (mut pair, _, _, channel) = with_channel()?;

        let id = pair.bobby.post_message(channel, "hello", 2_000)?;
        pair.bobby_to_alice()?;

        pair.bobby.doc().get_map(ROOT).delete(&id.to_hex())?;
        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&update, author).is_err());
        assert!(pair.alice.message(id).is_some());
        Ok(())
    }

    /// Only the author may retract. Bobby marking Alice's message deleted is a moderation action,
    /// and V1 has no moderation.
    #[test]
    fn a_writer_may_not_delete_someone_elses_message() -> Result<()> {
        let (mut pair, _, _, channel) = with_channel()?;

        let id = pair.alice.post_message(channel, "hello", 2_000)?;
        pair.alice_to_bobby()?;

        assert!(pair.bobby.delete_message(id).is_err());

        super::super::item_map(pair.bobby.doc(), ROOT, &id.to_hex())
            .expect("message")
            .insert(DELETED, true)?;
        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&update, author).is_err());
        assert!(!pair.alice.message(id).expect("message").deleted);
        Ok(())
    }

    /// A reply records the edge and stays in the main flow. The edge may point at a message we have
    /// not synced yet, which is fine — see the module docs on dangling references.
    #[test]
    fn a_reply_records_its_edge_and_stays_in_the_channel() -> Result<()> {
        let (mut pair, _, _, channel) = with_channel()?;

        let original = pair.alice.post_message(channel, "question?", 2_000)?;
        pair.alice_to_bobby()?;
        let reply = pair
            .bobby
            .reply_to_message(channel, original, "answer!", 3_000)?;
        pair.bobby_to_alice()?;

        assert_eq!(
            pair.alice.message(reply).expect("reply").reply_to,
            Some(original)
        );
        assert_eq!(pair.alice.channel_messages(channel).len(), 2);
        Ok(())
    }

    /// A reply to a message that never arrives is still a valid, displayable message. Rejecting it
    /// would make acceptance depend on network timing.
    #[test]
    fn a_reply_to_an_unknown_message_is_accepted() -> Result<()> {
        let (mut pair, _, _, channel) = with_channel()?;

        let never_sent = Hash::digest(b"never sent");
        let reply = pair
            .bobby
            .reply_to_message(channel, never_sent, "answer!", 3_000)?;
        pair.bobby_to_alice()?;

        assert_eq!(
            pair.alice.message(reply).expect("reply").reply_to,
            Some(never_sent)
        );
        assert!(pair.alice.message(never_sent).is_none());
        Ok(())
    }
}
