// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cross-log causal ordering: hold an operation until the operations it depends on are applied.
//!
//! ## Why this has to exist
//!
//! p2panda enforces order **within** one author's log — an operation whose backlink is missing is
//! rejected outright — but there is no ordering **across** authors. Alice creates the group, adds
//! Bobby, and Bobby immediately writes a todo item: three operations in two logs, and the network
//! may hand any of them to you first. Two things then go wrong without a buffer here:
//!
//! * **The group CRDT panics.** `GroupCrdt::process` requires every dependency of an operation to
//!   have been applied already. Give it Bobby's `Add` before Alice's `Create` and it panics inside
//!   the CRDT. A remote peer could crash us just by having its operations arrive in the wrong
//!   order, which is not exotic — it is the normal case during sync.
//! * **The write check becomes a race.** Bobby's todo edit is authorized by the group operation
//!   that added him. Evaluate it before that operation lands and you reject a legitimate edit; and
//!   whether you rejected it would depend on network timing, so two peers would disagree about the
//!   contents of the list. Buffering makes the verdict a property of the data, not of the arrival
//!   order.
//!
//! ## Why we wrote our own instead of using p2panda's
//!
//! `p2panda-stream` ships a real `Orderer` processor, and it is the right thing if you build your
//! own stack on `p2panda-stream`. It is not reachable from the high-level `Node` we use here:
//! `Node`'s processing pipeline is `pub(crate)` and hard-codes `Ingest` + `LogPrune`, so you cannot
//! insert a processor into it, and `Node`'s `Extensions` type is a closed enum with no slot to hang
//! dependencies off. (Its `Causal` variant, which would carry `previous`, is `unimplemented!()`.)
//!
//! So dependencies travel in our own message payload — where we are free to put anything — and this
//! module is the ~60 lines that consume them. It is deliberately a plain data structure: no async,
//! no store, no tokio `LocalSet`, and it is unit-testable by feeding it operations in the worst
//! order you can think of.
//!
//! ## Semantics
//!
//! * An operation is **released** once every dependency it names has been released before it.
//! * Releasing is transitive: releasing one operation can unblock a chain of others.
//! * An operation is released exactly once. p2panda promises at-least-once delivery and echoes our
//!   own operations back to us, and a restart replays the whole log, so duplicates are the norm and
//!   are dropped here.
//! * Release order among operations that are *concurrently* ready is by operation hash — an
//!   arbitrary rule, but the same arbitrary rule on every peer.
//! * An operation whose dependencies never arrive stays pending forever. That is the honest
//!   behaviour: we cannot judge it, so we do not apply it.
//!
//! "Released" is not "accepted". A group operation that the CRDT rejects, or a todo edit from a
//! non-writer, still counts as released — we saw it and reached a verdict. Anything waiting on it
//! is then evaluated against a group state that does not contain it, which is exactly right: an
//! edit depending on a rejected operation is itself unauthorized.

use std::collections::BTreeMap;
use std::collections::HashSet;

use p2panda_core::Hash;

/// A dependency buffer.
///
/// `T` is whatever the caller wants back once an operation is ready to apply.
#[derive(Debug, Default)]
pub struct Orderer<T> {
    /// Operations we have released. Also our duplicate filter.
    released: HashSet<Hash>,
    /// Operations waiting for their dependencies, keyed by id so that release order is stable.
    pending: BTreeMap<Hash, (Vec<Hash>, T)>,
}

impl<T> Orderer<T> {
    pub fn new() -> Self {
        Self {
            released: HashSet::new(),
            pending: BTreeMap::new(),
        }
    }

    /// Take in an operation; get back everything that is now ready to apply, in dependency order.
    ///
    /// The returned operations must be applied in the order given, and the caller must apply *all*
    /// of them: their release has already been recorded here.
    pub fn process(&mut self, id: Hash, dependencies: Vec<Hash>, operation: T) -> Vec<T> {
        // Already seen: a duplicate delivery, our own operation echoed back, or a replayed log.
        if self.released.contains(&id) || self.pending.contains_key(&id) {
            return Vec::new();
        }

        self.pending.insert(id, (dependencies, operation));
        self.release_ready()
    }

    /// How many operations are still waiting for dependencies we have not seen.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    // TODO: This is a very naive algorithm and scales O(pending * dependencies) which can block.
    // This can definitely be optimized with better use of indexes (Map keys)
    fn release_ready(&mut self) -> Vec<T> {
        let mut ready = Vec::new();

        // Each release can unblock others, so keep sweeping until a pass frees nothing.
        loop {
            let next = self
                .pending
                .iter()
                .find(|(_, (dependencies, _))| {
                    dependencies
                        .iter()
                        .all(|dependency| self.released.contains(dependency))
                })
                .map(|(id, _)| *id);

            let Some(id) = next else {
                return ready;
            };

            let (_, operation) = self.pending.remove(&id).expect("id came from the map");
            self.released.insert(id);
            ready.push(operation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(name: &str) -> Hash {
        Hash::digest(name)
    }

    /// The case that would panic the group CRDT: a chain delivered backwards.
    #[test]
    fn a_reversed_chain_is_released_in_causal_order() {
        let (create, add, edit) = (hash("create"), hash("add"), hash("edit"));
        let mut orderer = Orderer::new();

        // The edit depends on the add, which depends on the create — and they arrive in exactly
        // the wrong order.
        assert!(orderer.process(edit, vec![add], "edit").is_empty());
        assert!(orderer.process(add, vec![create], "add").is_empty());
        assert_eq!(orderer.pending(), 2);

        // The create arrives last and unblocks the whole chain, transitively and in order.
        assert_eq!(
            orderer.process(create, vec![], "create"),
            vec!["create", "add", "edit"]
        );
        assert_eq!(orderer.pending(), 0);
    }

    #[test]
    fn an_operation_with_no_missing_dependencies_is_released_immediately() {
        let mut orderer = Orderer::new();
        assert_eq!(
            orderer.process(hash("create"), vec![], "create"),
            ["create"]
        );
    }

    /// At-least-once delivery, our own echoed operations, and log replay on restart all mean the
    /// same operation shows up more than once. It must only be applied once.
    #[test]
    fn duplicates_are_dropped() {
        let create = hash("create");
        let mut orderer = Orderer::new();

        assert_eq!(orderer.process(create, vec![], "create"), ["create"]);
        assert!(orderer.process(create, vec![], "create").is_empty());
        assert!(orderer.process(create, vec![], "create").is_empty());
    }

    /// A duplicate that arrives while the original is still waiting must not queue twice.
    #[test]
    fn duplicates_while_pending_are_dropped() {
        let (create, add) = (hash("create"), hash("add"));
        let mut orderer = Orderer::new();

        assert!(orderer.process(add, vec![create], "add").is_empty());
        assert!(orderer.process(add, vec![create], "add").is_empty());
        assert_eq!(orderer.pending(), 1);

        assert_eq!(
            orderer.process(create, vec![], "create"),
            vec!["create", "add"]
        );
    }

    /// An operation naming several dependencies waits for all of them. A todo edit does this: it
    /// depends on every current tip of the group DAG.
    #[test]
    fn all_dependencies_must_arrive() {
        let (one, two, edit) = (hash("one"), hash("two"), hash("edit"));
        let mut orderer = Orderer::new();

        assert!(orderer.process(edit, vec![one, two], "edit").is_empty());
        assert_eq!(orderer.process(one, vec![], "one"), ["one"]);
        assert!(
            orderer.pending() == 1,
            "edit still waits for the second dep"
        );
        assert_eq!(orderer.process(two, vec![], "two"), vec!["two", "edit"]);
    }

    /// An operation whose dependency never arrives is never applied — we cannot judge it.
    #[test]
    fn an_orphan_waits_forever() {
        let mut orderer = Orderer::new();
        assert!(
            orderer
                .process(hash("edit"), vec![hash("never-sent")], "edit")
                .is_empty()
        );
        assert_eq!(orderer.pending(), 1);
    }
}
