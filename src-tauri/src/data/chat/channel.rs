// SPDX-License-Identifier: MIT OR Apache-2.0

//! Channels: the named rooms a server is divided into, plus the order they are browsed in.
//!
//! Channels are the one item type with **no owner**. Any writer may create a channel, rename it, and
//! archive it. That is a deliberate reading of CHAT.md's open question ("rename (owner or any
//! write?)"): a channel is shared furniture, not a personal possession, and a server whose channel
//! list can only be curated by whoever happened to create each channel gets stuck the moment that
//! person leaves. Moderation is a V2 problem — when there is a role between "writer" and "admin",
//! that is where a stricter rule belongs, and [`authorize`] is the single place to put it.
//!
//! What is still protected is provenance: `created_by` and `created_at` are immutable. Without that,
//! "any writer may edit a channel" would extend to "any writer may rewrite who created it".
//!
//! ## No deletion, only archiving
//!
//! There is no way to remove a channel, by design (CHAT.md: "no delete -> reversible archive"). A
//! deletion in a CRDT is a genuinely destructive operation — it removes the container that every
//! message's `channel` field points at, and a peer that had not yet synced those messages has no way
//! to tell "this channel was deleted" from "this channel has not arrived yet". Archiving is a
//! boolean, it is reversible, and it leaves every reference intact. [`authorize`] enforces this:
//! a hard delete of a channel key is refused, not merely discouraged by the local API.
//!
//! ## The channel order is a `LoroMovableList`
//!
//! CHAT.md asks for a "moveable list of channels to browse", and a movable list is precisely the
//! CRDT for it: `mov` is a first-class operation, so two people concurrently dragging different
//! channels around converge on a sensible order instead of one move being re-expressed as a
//! delete-and-insert that duplicates or drops an entry.
//!
//! The order is **server-wide and shared**, not per-person. That is the simpler thing and matches
//! how Discord/Slack treat channel ordering. A per-person ordering would belong in the "Personal
//! Area" private space CHAT.md sketches for V2, not here — it is exactly the kind of thing that
//! should not be visible to everyone else on the server.
//!
//! Note the asymmetry this creates: a channel appearing in `channels` but not in `channel_order`
//! (because the two halves of a creation arrived separately, or because a peer never wrote an order
//! entry) must still be visible. [`ChatDoc::channels`] therefore treats the order list as a *hint*
//! and appends anything missing, rather than using it as the source of truth for which channels
//! exist.

use loro::{LoroDoc, LoroMap};
use p2panda_core::Hash;

use super::{
    AuthorId, ChatDoc, ServerId, bool_field, i64_field, parse_author, parse_id, str_field,
};
use crate::Result;

/// Root `LoroMap`: channel id (hex) -> channel map.
pub const ROOT: &str = "channels";

/// Root `LoroMovableList` of channel ids, in browsing order.
pub const ORDER_ROOT: &str = "channel_order";

const NAME: &str = "name";
const PURPOSE: &str = "purpose";
const ARCHIVED: &str = "archived";
const CREATED_BY: &str = "created_by";
const CREATED_AT: &str = "created_at";

pub type ChannelId = Hash;

/// A room within a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub id: ChannelId,
    pub name: String,
    /// One-line description of what the channel is for. Empty when never set.
    pub purpose: String,
    /// Archived channels stay readable and keep their messages; they are hidden from the default
    /// browsing list and can be un-archived.
    pub archived: bool,
    pub created_by: AuthorId,
    /// Author-supplied wall-clock milliseconds. A hint, like every timestamp here.
    pub created_at: i64,
}

/// Read one channel, or `None` if it does not exist or is malformed.
pub fn read(doc: &LoroDoc, id: &str) -> Option<Channel> {
    let map = super::item_map(doc, ROOT, id)?;
    Some(Channel {
        id: parse_id(id)?,
        name: str_field(&map, NAME).unwrap_or_default(),
        purpose: str_field(&map, PURPOSE).unwrap_or_default(),
        archived: bool_field(&map, ARCHIVED).unwrap_or(false),
        created_by: parse_author(&str_field(&map, CREATED_BY)?)?,
        created_at: i64_field(&map, CREATED_AT).unwrap_or(0),
    })
}

/// May `author` make this change to this channel?
///
/// Any writer may create, rename, re-purpose and (un)archive. Nobody may rewrite provenance or
/// delete the channel outright.
pub fn authorize(before: Option<&Channel>, after: Option<&Channel>, author: AuthorId) -> Result<()> {
    match (before, after) {
        // Creation. The only constraint is that you cannot create a channel in someone else's name.
        (None, Some(after)) => {
            if after.created_by != author {
                return Err("a channel must be created in the creator's own name".into());
            }
            Ok(())
        }
        // Modification by any writer, as long as provenance is untouched.
        (Some(before), Some(after)) => {
            if before.created_by != after.created_by || before.created_at != after.created_at {
                return Err("a channel's creator and creation time are immutable".into());
            }
            Ok(())
        }
        // Deletion. Refused for everyone — archive instead, so the messages pointing here keep a
        // channel to point at.
        (Some(_), None) => Err("channels cannot be deleted, only archived".into()),
        // A key that exists in neither state: a write of something unreadable as a channel.
        (None, None) => Err("malformed channel".into()),
    }
}

impl ChatDoc {
    /// Create a channel and append it to the browsing order. Returns its new id.
    pub fn create_channel(&self, name: &str, now: i64) -> Result<ChannelId> {
        let id: ChannelId = ServerId::random().into();
        let hex = id.to_hex();

        let map = self
            .doc()
            .get_map(ROOT)
            .insert_container(&hex, LoroMap::new())?;
        map.insert(NAME, name)?;
        map.insert(PURPOSE, "")?;
        map.insert(ARCHIVED, false)?;
        map.insert(CREATED_BY, self.me().to_hex())?;
        map.insert(CREATED_AT, now)?;

        self.doc().get_movable_list(ORDER_ROOT).push(hex)?;
        Ok(id)
    }

    /// Rename a channel. Any writer may do this — see the module docs.
    pub fn rename_channel(&self, id: ChannelId, name: &str) -> Result<()> {
        self.channel_map(id)?.insert(NAME, name)?;
        Ok(())
    }

    pub fn set_channel_purpose(&self, id: ChannelId, purpose: &str) -> Result<()> {
        self.channel_map(id)?.insert(PURPOSE, purpose)?;
        Ok(())
    }

    /// Archive or un-archive a channel. This is what "delete" means here, and it is reversible.
    pub fn set_channel_archived(&self, id: ChannelId, archived: bool) -> Result<()> {
        self.channel_map(id)?.insert(ARCHIVED, archived)?;
        Ok(())
    }

    /// Move a channel within the browsing order.
    pub fn move_channel(&self, from: usize, to: usize) -> Result<()> {
        self.doc().get_movable_list(ORDER_ROOT).mov(from, to)?;
        Ok(())
    }

    pub fn channel(&self, id: ChannelId) -> Option<Channel> {
        read(self.doc(), &id.to_hex())
    }

    /// Every channel, archived ones included, in browsing order.
    ///
    /// The order list is a hint, not the index: anything in `channels` that the order list does not
    /// mention is appended (by id, so peers agree), and anything the order list mentions that does
    /// not exist is skipped. That keeps a partially-synced document showing all the channels it
    /// knows about instead of hiding some until the order entry catches up.
    pub fn channels(&self) -> Vec<Channel> {
        let order = self.doc().get_movable_list(ORDER_ROOT);
        let mut seen = Vec::new();
        let mut channels = Vec::new();

        for value in order.to_vec() {
            let loro::LoroValue::String(id) = value else {
                continue;
            };
            let id = id.to_string();
            if seen.contains(&id) {
                continue;
            }
            if let Some(channel) = read(self.doc(), &id) {
                seen.push(id);
                channels.push(channel);
            }
        }

        for id in super::item_ids(self.doc(), ROOT) {
            if !seen.contains(&id)
                && let Some(channel) = read(self.doc(), &id)
            {
                channels.push(channel);
            }
        }

        channels
    }

    /// The channels shown by default: everything not archived.
    pub fn active_channels(&self) -> Vec<Channel> {
        self.channels()
            .into_iter()
            .filter(|channel| !channel.archived)
            .collect()
    }

    fn channel_map(&self, id: ChannelId) -> Result<LoroMap> {
        super::item_map(self.doc(), ROOT, &id.to_hex())
            .ok_or_else(|| format!("unknown channel {id}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    #[test]
    fn a_channel_is_created_and_replicates_with_its_order_entry() -> Result<()> {
        let (mut pair, alice_id, _) = Pair::new();

        let general = pair.alice.create_channel("general", 1_000)?;
        let random = pair.alice.create_channel("random", 1_001)?;
        pair.alice_to_bobby()?;

        let channels = pair.bobby.channels();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].name, "general");
        assert_eq!(channels[1].name, "random");
        assert_eq!(channels[0].id, general);
        assert_eq!(channels[0].created_by, alice_id);
        assert_eq!(channels[0].created_at, 1_000);
        assert_eq!(channels[1].id, random);
        Ok(())
    }

    /// The permissive rule, stated as a test so that tightening it later is a deliberate act: Bobby
    /// did not create this channel and may still rename it.
    #[test]
    fn any_writer_may_rename_a_channel_they_did_not_create() -> Result<()> {
        let (mut pair, _, _) = Pair::new();

        let general = pair.alice.create_channel("general", 1_000)?;
        pair.alice_to_bobby()?;

        pair.bobby.rename_channel(general, "general-chat")?;
        pair.bobby_to_alice()?;

        assert_eq!(pair.alice.channel(general).expect("channel").name, "general-chat");
        Ok(())
    }

    /// Provenance is immutable even though the rest of the channel is not.
    #[test]
    fn a_writer_may_not_rewrite_who_created_a_channel() -> Result<()> {
        let (mut pair, _, bobby_id) = Pair::new();

        let general = pair.alice.create_channel("general", 1_000)?;
        pair.alice_to_bobby()?;

        super::super::item_map(pair.bobby.doc(), ROOT, &general.to_hex())
            .expect("channel")
            .insert(CREATED_BY, bobby_id.to_hex())?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("provenance rewrite must be refused");
        assert!(err.to_string().contains("immutable"), "{err}");
        Ok(())
    }

    /// Creating a channel and attributing it to someone else is refused at creation time, where the
    /// before-state is empty and only the after-state can be checked.
    #[test]
    fn a_channel_cannot_be_created_in_someone_elses_name() -> Result<()> {
        let (mut pair, alice_id, _) = Pair::new();

        let map = pair
            .bobby
            .doc()
            .get_map(ROOT)
            .insert_container(&Hash::digest(b"forged").to_hex(), LoroMap::new())?;
        map.insert(NAME, "trap")?;
        map.insert(CREATED_BY, alice_id.to_hex())?;
        map.insert(CREATED_AT, 1_000)?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&update, author).is_err());
        assert!(pair.alice.channels().is_empty());
        Ok(())
    }

    /// Hard deletion is refused for everyone, including the channel's own creator — the whole point
    /// is that the messages pointing at it keep something to point at.
    #[test]
    fn a_channel_cannot_be_deleted_even_by_its_creator() -> Result<()> {
        let (mut pair, _, _) = Pair::new();

        let general = pair.bobby.create_channel("general", 1_000)?;
        pair.bobby_to_alice()?;

        pair.bobby.doc().get_map(ROOT).delete(&general.to_hex())?;
        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        let err = pair
            .alice
            .import_checked(&update, author)
            .expect_err("deletion must be refused");
        assert!(err.to_string().contains("archived"), "{err}");
        assert!(pair.alice.channel(general).is_some());
        Ok(())
    }

    /// Archiving is the supported alternative, and it is reversible.
    #[test]
    fn archiving_hides_a_channel_without_losing_it() -> Result<()> {
        let (mut pair, _, _) = Pair::new();

        let general = pair.alice.create_channel("general", 1_000)?;
        pair.alice.set_channel_archived(general, true)?;
        pair.alice_to_bobby()?;

        assert!(pair.bobby.active_channels().is_empty());
        assert_eq!(pair.bobby.channels().len(), 1);
        assert!(pair.bobby.channel(general).expect("channel").archived);

        pair.bobby.set_channel_archived(general, false)?;
        pair.bobby_to_alice()?;
        assert_eq!(pair.alice.active_channels().len(), 1);
        Ok(())
    }

    /// Two writers renaming the same channel at the same time. Both sides must converge, and to one
    /// of the two names — `name` is a last-writer-wins value, not merged text.
    #[test]
    fn concurrent_renames_converge() -> Result<()> {
        let (mut pair, _, _) = Pair::new();

        let general = pair.alice.create_channel("general", 1_000)?;
        pair.alice_to_bobby()?;

        pair.alice.rename_channel(general, "alice-name")?;
        pair.bobby.rename_channel(general, "bobby-name")?;
        pair.sync()?;

        let from_alice = pair.alice.channel(general).expect("channel").name;
        let from_bobby = pair.bobby.channel(general).expect("channel").name;
        assert_eq!(from_alice, from_bobby, "peers must converge");
        assert!(
            from_alice == "alice-name" || from_alice == "bobby-name",
            "one rename must win outright: {from_alice}"
        );
        Ok(())
    }

    /// Two people concurrently creating channels: both survive, and both peers list them in the same
    /// order. This is the case a plain list would get wrong.
    #[test]
    fn concurrent_channel_creation_keeps_both_in_one_agreed_order() -> Result<()> {
        let (mut pair, _, _) = Pair::new();

        pair.alice.create_channel("from-alice", 1_000)?;
        pair.bobby.create_channel("from-bobby", 1_000)?;
        pair.sync()?;

        let alice_view: Vec<String> = pair.alice.channels().iter().map(|c| c.name.clone()).collect();
        let bobby_view: Vec<String> = pair.bobby.channels().iter().map(|c| c.name.clone()).collect();
        assert_eq!(alice_view.len(), 2);
        assert_eq!(alice_view, bobby_view, "peers must agree on channel order");
        Ok(())
    }

    /// Reordering converges too, which is why the order is a `LoroMovableList` rather than a list of
    /// ids that would have to be deleted and re-inserted to move one.
    #[test]
    fn concurrent_reordering_converges_without_losing_channels() -> Result<()> {
        let (mut pair, _, _) = Pair::new();

        pair.alice.create_channel("one", 1_000)?;
        pair.alice.create_channel("two", 1_001)?;
        pair.alice.create_channel("three", 1_002)?;
        pair.alice_to_bobby()?;

        pair.alice.move_channel(2, 0)?;
        pair.bobby.move_channel(0, 2)?;
        pair.sync()?;

        let alice_view: Vec<String> = pair.alice.channels().iter().map(|c| c.name.clone()).collect();
        let bobby_view: Vec<String> = pair.bobby.channels().iter().map(|c| c.name.clone()).collect();
        assert_eq!(alice_view, bobby_view, "peers must agree on channel order");
        assert_eq!(alice_view.len(), 3, "no channel may be lost to a move");
        Ok(())
    }

    /// A channel whose order entry never arrived is still browsable. The order list is a hint.
    #[test]
    fn a_channel_missing_from_the_order_list_is_still_listed() -> Result<()> {
        let (pair, _, _) = Pair::new();

        let id = Hash::digest(b"orphan").to_hex();
        let map = pair
            .alice
            .doc()
            .get_map(ROOT)
            .insert_container(&id, LoroMap::new())?;
        map.insert(NAME, "orphan")?;
        map.insert(CREATED_BY, pair.alice.me().to_hex())?;
        map.insert(CREATED_AT, 1_000)?;

        assert_eq!(pair.alice.channels().len(), 1);
        Ok(())
    }
}
