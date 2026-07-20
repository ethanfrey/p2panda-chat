// SPDX-License-Identifier: MIT OR Apache-2.0

//! Profiles: a display name and status, one per person.
//!
//! ## Ownership needs no owner field
//!
//! The `profiles` map is keyed by [`AuthorId`] — the person subgroup id — so the key *is* the owner.
//! [`authorize`] is a one-line comparison and there is nothing to forge: an author writing to a key
//! that is not their own is refused, whatever the contents.
//!
//! This is the simplest of the five item types, and it is the one worth reading first, because it
//! shows the shape the others follow: a flat map, an ownership rule stated over the item id and its
//! before/after states, and no cross-item references to keep consistent.
//!
//! ## Why the name is a value and not a `LoroText`
//!
//! Message bodies are [`loro::LoroText`], so two of your devices editing the same message merge
//! character by character rather than clobbering each other. A display name is not that: nobody
//! collaboratively types a nickname, and "Ali" + "ice" merging into "Aliice" is a worse outcome than
//! one device simply winning. So `name` and `status` are plain values in the map, which makes them
//! last-writer-wins.
//!
//! "Last writer" here means Loro's deterministic resolution over the two concurrent versions, not
//! whoever's clock was later — both peers land on the same answer, which is what matters. In
//! practice the conflict only arises between one person's own devices, since nobody else may write
//! the key at all.

use loro::LoroDoc;

use super::{AuthorId, ChatDoc, parse_author, str_field};
use crate::Result;

/// Root `LoroMap`: author id (hex) -> profile map.
pub const ROOT: &str = "profiles";

const NAME: &str = "name";
const STATUS: &str = "status";

/// A person's display identity on this server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// The person subgroup this profile belongs to. Also its key in the map.
    pub author: AuthorId,
    pub name: String,
    /// Free-text status ("away", "on holiday"). Empty when never set.
    pub status: String,
}

/// Read one profile, or `None` if this person has never set one.
///
/// A person with no profile is normal, not an error — they have simply not written one yet, and the
/// UI falls back to their id. Nothing else in the model depends on a profile existing.
pub fn read(doc: &LoroDoc, id: &str) -> Option<Profile> {
    let map = super::item_map(doc, ROOT, id)?;
    Some(Profile {
        author: parse_author(id)?,
        name: str_field(&map, NAME).unwrap_or_default(),
        status: str_field(&map, STATUS).unwrap_or_default(),
    })
}

/// May `author` change the profile stored under `item`?
///
/// Only if it is theirs. The key is the owner, so no before/after comparison is needed and there are
/// no immutable fields to protect — every field of your own profile is yours to change.
pub fn authorize(item: &str, author: AuthorId) -> Result<()> {
    if item != author.to_hex() {
        return Err("a profile may only be written by the person it belongs to".into());
    }
    Ok(())
}

impl ChatDoc {
    /// Set our own display name and status. There is no API to write anyone else's.
    pub fn set_profile(&self, name: &str, status: &str) -> Result<()> {
        let map = self
            .doc()
            .get_map(ROOT)
            .ensure_mergeable_map(&self.me().to_hex())?;
        map.insert(NAME, name)?;
        map.insert(STATUS, status)?;
        Ok(())
    }

    /// One person's profile, if they have set one.
    pub fn profile(&self, author: AuthorId) -> Option<Profile> {
        read(self.doc(), &author.to_hex())
    }

    /// Every profile on the server, ordered by author id so the listing is stable across peers.
    pub fn profiles(&self) -> Vec<Profile> {
        super::item_ids(self.doc(), ROOT)
            .iter()
            .filter_map(|id| read(self.doc(), id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    #[test]
    fn a_person_sets_their_own_profile_and_it_replicates() -> Result<()> {
        let (mut pair, alice_id, _) = Pair::new();

        pair.alice.set_profile("Alice", "building things")?;
        pair.alice_to_bobby()?;

        let profile = pair.bobby.profile(alice_id).expect("alice has a profile");
        assert_eq!(profile.name, "Alice");
        assert_eq!(profile.status, "building things");
        assert_eq!(profile.author, alice_id);
        Ok(())
    }

    /// The core rule. Bobby is a perfectly legitimate writer on this server — the group says so —
    /// and he still may not touch Alice's profile.
    #[test]
    fn a_writer_may_not_write_someone_elses_profile() -> Result<()> {
        let (mut pair, alice_id, _) = Pair::new();

        // Bobby writes directly to the container, bypassing the local API that would stop him.
        pair.bobby
            .doc()
            .get_map(ROOT)
            .ensure_mergeable_map(&alice_id.to_hex())?
            .insert(NAME, "Bobby was here")?;

        let author = pair.bobby.me();
        let update = pair.bobby.export()?.expect("bobby has changes");
        assert!(pair.alice.import_checked(&update, author).is_err());
        assert!(pair.alice.profile(alice_id).is_none());
        Ok(())
    }

    /// Two of one person's own devices renaming concurrently. Both must converge, and — because the
    /// name is a last-writer-wins value rather than merged text — to one of the two names, not a
    /// character-level splice of them.
    #[test]
    fn concurrent_renames_converge_on_one_of_the_two_names() -> Result<()> {
        let (mut pair, alice_id, _) = Pair::new();

        // Both documents write as the *same* person: this is Alice's laptop and Alice's phone.
        pair.alice.set_profile("Alice", "")?;
        let mut phone = second_device(&pair.alice, alice_id)?;

        pair.alice.set_profile("Alice Laptop", "")?;
        phone.set_profile("Alice Phone", "")?;

        let from_laptop = pair.alice.export()?.expect("laptop has changes");
        let from_phone = phone.export()?.expect("phone has changes");
        pair.alice.import_checked(&from_phone, alice_id)?;
        phone.import_checked(&from_laptop, alice_id)?;

        let laptop_name = pair.alice.profile(alice_id).expect("profile").name;
        let phone_name = phone.profile(alice_id).expect("profile").name;
        assert_eq!(laptop_name, phone_name, "the two devices must converge");
        assert!(
            laptop_name == "Alice Laptop" || laptop_name == "Alice Phone",
            "a name must win outright, not merge character-wise: {laptop_name}"
        );
        Ok(())
    }

    /// A profile that has never been written reads as absent rather than as an empty profile, so the
    /// UI can tell "no profile yet" from "cleared their name".
    #[test]
    fn an_unset_profile_is_absent() {
        let (pair, _, bobby_id) = Pair::new();
        assert!(pair.alice.profile(bobby_id).is_none());
        assert!(pair.alice.profiles().is_empty());
    }
}
