// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reactions: an emoji, a message, and who put it there.
//!
//! ## The id is derived, which makes double-reacting impossible
//!
//! A reaction id is `H(author ‖ message ‖ emoji)` rather than a random value. Everything else in
//! this model uses random ids; this one does not, and the reason is worth stating because it is the
//! whole design:
//!
//! * **Idempotence for free.** Tapping 👍 twice — or tapping it on your laptop and your phone before
//!   they sync — produces the *same key*, so the two writes merge into one entry rather than
//!   becoming two identical reactions that both have to be counted and de-duplicated at read time.
//! * **Toggling is well defined.** Un-reacting deletes a key whose name both devices can compute
//!   without having seen each other's write. With random ids, "remove my 👍" would first require
//!   finding it.
//! * **Nothing to forge.** The id commits to the author, so a reaction whose contents do not hash to
//!   its own key is self-evidently wrong. [`authorize`] checks this, which means a reaction cannot be
//!   planted under someone else's name even in the create case where there is no before-state to
//!   compare against.
//!
//! The cost is that a reaction cannot be *edited*, only added and removed. That is exactly the
//! lifecycle CHAT.md asks for ("create -> REACTION-ID ... delete by author"), so it costs nothing.
//!
//! ## Reactions are removable, unlike everything else here
//!
//! Channels cannot be deleted and messages cannot be removed, because things point at them. Nothing
//! points at a reaction, so it is the one item type where a hard delete is the right model — an
//! un-react should leave nothing behind, not a tombstone that has to be filtered out of every count.
//!
//! ## Reactions to messages we do not have
//!
//! A reaction may name a message that has not arrived. It is stored and simply does not appear in
//! any message's reaction list until the message shows up. See the module docs on [`super`].

use loro::LoroDoc;
use p2panda_core::Hash;

use super::message::MessageId;
use super::{AuthorId, ChatDoc, parse_author, parse_id, str_field};
use crate::Result;

/// Root `LoroMap`: reaction id (hex) -> reaction map.
pub const ROOT: &str = "reactions";

const AUTHOR: &str = "author";
const MESSAGE: &str = "message";
const EMOJI: &str = "emoji";

pub type ReactionId = Hash;

/// One person's one emoji on one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub id: ReactionId,
    pub author: AuthorId,
    pub message: MessageId,
    pub emoji: String,
}

/// The deterministic id of a reaction. Same author, message and emoji always give the same id, on
/// every device and every peer.
pub fn reaction_id(author: AuthorId, message: MessageId, emoji: &str) -> ReactionId {
    let mut input = Vec::new();
    input.extend_from_slice(author.as_bytes());
    input.extend_from_slice(message.as_bytes());
    input.extend_from_slice(emoji.as_bytes());
    Hash::digest(&input)
}

/// Read one reaction, or `None` if it does not exist or is malformed.
pub fn read(doc: &LoroDoc, id: &str) -> Option<Reaction> {
    let map = super::item_map(doc, ROOT, id)?;
    Some(Reaction {
        id: parse_id(id)?,
        author: parse_author(&str_field(&map, AUTHOR)?)?,
        message: parse_id(&str_field(&map, MESSAGE)?)?,
        emoji: str_field(&map, EMOJI)?,
    })
}

/// May `author` make this change to this reaction?
///
/// A reaction is create-or-delete, never edit, and its id must commit to its own contents.
pub fn authorize(before: Option<&Reaction>, after: Option<&Reaction>, author: AuthorId) -> Result<()> {
    match (before, after) {
        (None, Some(after)) => {
            if after.author != author {
                return Err("a reaction must be made in the author's own name".into());
            }
            // The id is the proof. A reaction stored under any other key could be used to shadow or
            // impersonate someone else's reaction, so the key must hash from the contents.
            if reaction_id(after.author, after.message, &after.emoji) != after.id {
                return Err("a reaction's id must be derived from its contents".into());
            }
            Ok(())
        }
        (Some(before), None) => {
            if before.author != author {
                return Err("only its author may remove a reaction".into());
            }
            Ok(())
        }
        // Every field is immutable, and the derived id makes any change a different reaction
        // anyway — so an in-place edit is always someone tampering.
        (Some(_), Some(_)) => Err("a reaction cannot be edited, only added and removed".into()),
        (None, None) => Err("malformed reaction".into()),
    }
}

impl ChatDoc {
    /// React to a message. Reacting twice with the same emoji is a no-op, by construction.
    pub fn add_reaction(&self, message: MessageId, emoji: &str) -> Result<ReactionId> {
        let id = reaction_id(self.me(), message, emoji);

        let map = self
            .doc()
            .get_map(ROOT)
            .ensure_mergeable_map(&id.to_hex())?;
        map.insert(AUTHOR, self.me().to_hex())?;
        map.insert(MESSAGE, message.to_hex())?;
        map.insert(EMOJI, emoji)?;

        Ok(id)
    }

    /// Remove our own reaction. Removing one that is not there is a no-op.
    pub fn remove_reaction(&self, message: MessageId, emoji: &str) -> Result<()> {
        let id = reaction_id(self.me(), message, emoji);
        if super::item_map(self.doc(), ROOT, &id.to_hex()).is_none() {
            return Ok(());
        }
        self.doc().get_map(ROOT).delete(&id.to_hex())?;
        Ok(())
    }

    /// Every reaction on a message, ordered by id so peers agree.
    pub fn message_reactions(&self, message: MessageId) -> Vec<Reaction> {
        super::item_ids(self.doc(), ROOT)
            .iter()
            .filter_map(|id| read(self.doc(), id))
            .filter(|reaction| reaction.message == message)
            .collect()
    }

    /// Reactions on a message grouped for display: `(emoji, count)`, most-used first, ties broken by
    /// the emoji itself so the row is stable across peers and across renders.
    pub fn reaction_summary(&self, message: MessageId) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for reaction in self.message_reactions(message) {
            match counts.iter_mut().find(|(emoji, _)| *emoji == reaction.emoji) {
                Some((_, count)) => *count += 1,
                None => counts.push((reaction.emoji, 1)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        counts
    }
}

#[cfg(test)]
mod tests {
    use loro::LoroMap;

    use super::super::channel::ChannelId;
    use super::super::test_support::*;
    use super::*;

    fn with_message() -> Result<(Pair, AuthorId, AuthorId, ChannelId, MessageId)> {
        let (mut pair, alice_id, bobby_id) = Pair::new();
        let channel = pair.alice.create_channel("general", 1_000)?;
        let message = pair.alice.post_message(channel, "hello", 2_000)?;
        pair.alice_to_bobby()?;
        Ok((pair, alice_id, bobby_id, channel, message))
    }

    #[test]
    fn a_reaction_is_added_and_replicates() -> Result<()> {
        let (mut pair, _, bobby_id, _, message) = with_message()?;

        pair.bobby.add_reaction(message, "👍")?;
        pair.bobby_to_alice()?;

        let reactions = pair.alice.message_reactions(message);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji, "👍");
        assert_eq!(reactions[0].author, bobby_id);
        assert_eq!(pair.alice.reaction_summary(message), [("👍".to_string(), 1)]);
        Ok(())
    }

    /// The payoff of the derived id: the same reaction twice is one reaction, with no de-duplication
    /// logic anywhere at read time.
    #[test]
    fn reacting_twice_with_the_same_emoji_is_one_reaction() -> Result<()> {
        let (mut pair, _, _, _, message) = with_message()?;

        pair.bobby.add_reaction(message, "👍")?;
        pair.bobby.add_reaction(message, "👍")?;
        pair.bobby_to_alice()?;

        assert_eq!(pair.alice.message_reactions(message).len(), 1);
        Ok(())
    }

    /// The same, but across two devices that never saw each other's write — the case a random id
    /// would get wrong.
    #[test]
    fn the_same_reaction_from_two_devices_converges_to_one() -> Result<()> {
        let (mut pair, alice_id, _, _, message) = with_message()?;

        let mut phone = second_device(&pair.alice, alice_id)?;

        pair.alice.add_reaction(message, "🎉")?;
        phone.add_reaction(message, "🎉")?;

        let from_laptop = pair.alice.export()?.expect("laptop has changes");
        let from_phone = phone.export()?.expect("phone has changes");
        pair.alice.import_checked(&from_phone, alice_id)?;
        phone.import_checked(&from_laptop, alice_id)?;

        assert_eq!(pair.alice.message_reactions(message).len(), 1);
        assert_eq!(pair.alice.reaction_summary(message), [("🎉".to_string(), 1)]);
        Ok(())
    }

    /// Two *different* people reacting with the same emoji are two reactions, counted as two.
    #[test]
    fn two_people_reacting_alike_are_counted_separately() -> Result<()> {
        let (mut pair, _, _, _, message) = with_message()?;

        pair.alice.add_reaction(message, "👍")?;
        pair.bobby.add_reaction(message, "👍")?;
        pair.sync()?;

        assert_eq!(pair.alice.reaction_summary(message), [("👍".to_string(), 2)]);
        assert_eq!(
            pair.alice.reaction_summary(message),
            pair.bobby.reaction_summary(message)
        );
        Ok(())
    }

    #[test]
    fn removing_our_own_reaction_leaves_nothing_behind() -> Result<()> {
        let (mut pair, _, _, _, message) = with_message()?;

        pair.bobby.add_reaction(message, "👍")?;
        pair.bobby_to_alice()?;
        pair.bobby.remove_reaction(message, "👍")?;
        pair.bobby_to_alice()?;

        assert!(pair.alice.message_reactions(message).is_empty());
        assert!(pair.alice.reaction_summary(message).is_empty());
        Ok(())
    }

    #[test]
    fn a_writer_may_not_remove_someone_elses_reaction() -> Result<()> {
        let (mut pair, alice_id, _, _, message) = with_message()?;

        pair.alice.add_reaction(message, "👍")?;
        pair.alice_to_bobby()?;

        let id = reaction_id(alice_id, message, "👍");
        pair.bobby.doc().get_map(ROOT).delete(&id.to_hex())?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("removing another author's reaction must be refused");
        assert!(err.to_string().contains("only its author"), "{err}");
        assert_eq!(pair.alice.message_reactions(message).len(), 1);
        Ok(())
    }

    #[test]
    fn a_reaction_cannot_be_made_in_someone_elses_name() -> Result<()> {
        let (mut pair, alice_id, _, _, message) = with_message()?;

        let id = reaction_id(alice_id, message, "👎");
        let map = pair
            .bobby
            .doc()
            .get_map(ROOT)
            .insert_container(&id.to_hex(), LoroMap::new())?;
        map.insert(AUTHOR, alice_id.to_hex())?;
        map.insert(MESSAGE, message.to_hex())?;
        map.insert(EMOJI, "👎")?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&update, author).is_err());
        assert!(pair.alice.message_reactions(message).is_empty());
        Ok(())
    }

    /// A reaction whose key does not hash from its contents is refused, even when the author is
    /// honest — otherwise the key could be chosen to collide with, and shadow, someone else's.
    #[test]
    fn a_reaction_stored_under_the_wrong_id_is_refused() -> Result<()> {
        let (mut pair, _, bobby_id, _, message) = with_message()?;

        let map = pair
            .bobby
            .doc()
            .get_map(ROOT)
            .insert_container(&Hash::digest(b"not derived").to_hex(), LoroMap::new())?;
        map.insert(AUTHOR, bobby_id.to_hex())?;
        map.insert(MESSAGE, message.to_hex())?;
        map.insert(EMOJI, "👍")?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("a mis-keyed reaction must be refused");
        assert!(err.to_string().contains("derived from its contents"), "{err}");
        Ok(())
    }

    /// A reaction is create-or-delete. Editing one in place is always tampering.
    #[test]
    fn a_reaction_cannot_be_edited_in_place() -> Result<()> {
        let (mut pair, _, _, _, message) = with_message()?;

        pair.bobby.add_reaction(message, "👍")?;
        pair.bobby_to_alice()?;

        let id = reaction_id(pair.bobby.me(), message, "👍");
        super::super::item_map(pair.bobby.doc(), ROOT, &id.to_hex())
            .expect("reaction")
            .insert(EMOJI, "👎")?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("editing a reaction must be refused");
        assert!(err.to_string().contains("cannot be edited"), "{err}");
        assert_eq!(pair.alice.message_reactions(message)[0].emoji, "👍");
        Ok(())
    }

    /// A reaction may name a message we do not have. It is accepted and stored, stays invisible
    /// because nothing joins to it, and appears the moment the message arrives.
    #[test]
    fn a_reaction_to_an_unknown_message_is_accepted_and_resolves_later() -> Result<()> {
        let (mut pair, _, _, channel, _) = with_message()?;

        let unknown = Hash::digest(b"not yet synced");
        let id = pair.bobby.add_reaction(unknown, "👀")?;
        pair.bobby_to_alice()?;

        // Stored, and reachable once you know the message id...
        assert_eq!(pair.alice.message_reactions(unknown).len(), 1);
        assert_eq!(pair.alice.message_reactions(unknown)[0].id, id);
        // ...but attached to nothing that exists, so no view shows it.
        assert!(pair.alice.message(unknown).is_none());
        assert!(pair.alice.channel_messages(channel).iter().all(|message| {
            pair.alice.message_reactions(message.id).is_empty()
        }));
        Ok(())
    }
}
