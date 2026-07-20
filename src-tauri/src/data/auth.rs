// SPDX-License-Identifier: MIT OR Apache-2.0

//! The access-control groups: a two-level structure of people and their devices.
//!
//! The first version of this crate had one flat group per list. This version has **two levels**,
//! which is how a real local-first app models "a person with several devices":
//!
//! ```text
//!   top group  (the list)
//!   ├── Individual(creator's manager device) @ Manage     ← the list admin
//!   ├── Group(person subgroup: Alice)        @ Write       ← a user
//!   │     ├── Individual(Alice laptop)  @ Manage           ← Alice's manager device
//!   │     └── Individual(Alice phone)   @ Write            ← Alice added her own phone
//!   └── Group(person subgroup: Bob)          @ Read        ← a read-only user
//!         └── Individual(Bob laptop)   @ Manage
//! ```
//!
//! * The **top group** is the list. Its members are *people* (as subgroups) plus the admin device.
//!   The list admin adds people. "Top level stays as is" from the caller's point of view: you add
//!   readers and writers.
//! * A **person subgroup** is one human's set of devices. One device is that subgroup's manager and
//!   adds the person's other devices, each as a reader, writer or (device-level) manager.
//!
//! A device's effective access to the list is its access *within its subgroup*, capped by that
//! subgroup's access *within the top group* — exactly `p2panda-auth`'s nested-group rule (a deeper
//! edge takes the lesser access; multiple paths combine by the greater). So Alice's phone, a
//! `Write` device in a `Write` subgroup, may write; were Alice's subgroup only `Read`, none of her
//! devices could write however she set their device-level roles.
//!
//! ## The one place reality bends the request: subgroups cannot be `Manage`
//!
//! `p2panda-auth` forbids adding or promoting a *group* member to `Manage`
//! (`GroupCrdtError::ManagerGroupsNotAllowed`, issue #779 — cross-group removal cycles are not yet
//! resolved). So the list admin cannot be "a person"; it must be a specific **device**, held
//! directly in the top group as `Individual(..) @ Manage`. That is why the creator's *device* is a
//! direct top-group member as well as the manager of the creator's own subgroup. Within a subgroup,
//! `Manage` is unaffected — a person may absolutely make a second device a manager of their devices.
//!
//! ## Group ids are derived, and the list id commits to the creator
//!
//! Every id here is derived from a device's public key, so nobody has to invent or exchange them,
//! and the derivation is what closes the old "competing `Create`" loose end:
//!
//! | id | derived from | is |
//! |---|---|---|
//! | [`person_group_id`]`(d)` | device `d` | the subgroup of the person whose manager device is `d` |
//! | [`top_group_id`]`(d)`    | creator `d` | the list's top group |
//! | [`list_topic`]`(d)`      | creator `d` | **the list id itself** |
//!
//! Because the list id *is* a hash of the creator's key, only the creator can produce a genesis that
//! matches the list you are on: [`AuthGroup::apply`] accepts a top-group `Create` only when
//! `list_topic(author) == topic` and its members are exactly the canonical template. There is no
//! "first `Create` wins" race any more — a given list id admits exactly one valid initial state, the
//! one built by whoever's key hashes to it. (See `main`, which derives the topic from the creator's
//! key before it ever spawns a node.)
//!
//! ## What we still build around the CRDT
//!
//! The same three sharp edges as the flat version, now spanning several groups:
//!
//! 1. **Causal order is mandatory or `process` panics**, so group operations go through
//!    [`crate::ordering::Orderer`] first. With multiple groups the rule is per-group: a group's
//!    `Create` must be applied before any operation on it. A todo edit names *all* group tips, so
//!    the write check can see the top group and the author's subgroup together.
//! 2. **A missing parent group panics `apply_action`** (`groups_y.remove(..).expect(..)`), so before
//!    handing a non-`Create` operation to the CRDT we check the parent group exists — otherwise a
//!    crafted operation naming an unknown group, with lying dependencies, would take every node down.
//! 3. **`GroupCrdtState` is not `Clone` and `process` consumes it**, so we keep the accepted
//!    operations and rebuild after a rejection.
//!
//! ## Nested queries have to be point-in-time by hand
//!
//! `p2panda-auth`'s transitive `members()` resolves subgroups against *current* state
//! (`members_inner` reads `current_state()`), so it cannot answer "was this device a writer at the
//! operation this edit depends on?". But `state_at(deps)` returns the *direct* membership of **every**
//! group at that point, so [`AuthGroup::access_at`] walks that snapshot transitively itself. That is
//! what keeps the write check deterministic even though membership is now nested.

use std::collections::{HashMap, HashSet};

use p2panda_auth::group::resolver::StrongRemove;
use p2panda_auth::group::{GroupAction, GroupCrdt, GroupCrdtState, GroupMember, GroupMembersState};
use p2panda_auth::{Access, AccessLevel, GroupsOperation};
use p2panda_core::{Hash, SigningKey, Topic, VerifyingKey};

use crate::Result;

/// We attach no conditions to access levels. `()` is the "no conditions" `Conditions` impl.
pub type Conditions = ();

/// The operation that mutates a group. Goes on the wire inside our message enum.
pub type GroupOperation = GroupsOperation<Conditions>;

type Resolver = StrongRemove<VerifyingKey, Hash, GroupOperation, Conditions>;
type Crdt = GroupCrdt<VerifyingKey, Hash, GroupOperation, Conditions, Resolver>;
type GroupState = GroupCrdtState<VerifyingKey, Hash, GroupOperation, Conditions>;
type MembersState = GroupMembersState<GroupMember<VerifyingKey>, Conditions>;

// Domain-separated labels, so the three derivations of one key never collide.
const TOP_LABEL: &[u8] = b"p2panda-chat:top-group:";
const PERSON_LABEL: &[u8] = b"p2panda-chat:person-group:";
const TOPIC_LABEL: &[u8] = b"p2panda-chat:server-topic:";

/// Derive a group's `VerifyingKey` from a seed key. Every 32-byte hash is a valid ed25519 signing
/// seed, so this always yields a usable key; the corresponding secret is nobody's — a group id is an
/// identifier, never something that signs.
fn derive_key(label: &[u8], seed: &VerifyingKey) -> VerifyingKey {
    let mut input = Vec::with_capacity(label.len() + seed.as_bytes().len());
    input.extend_from_slice(label);
    input.extend_from_slice(seed.as_bytes());
    SigningKey::from_bytes(Hash::digest(&input).as_bytes()).verifying_key()
}

/// The subgroup of the person whose manager device is `device`.
pub fn person_group_id(device: &VerifyingKey) -> VerifyingKey {
    derive_key(PERSON_LABEL, device)
}

/// The top group of the list created by `creator`.
pub fn top_group_id(creator: &VerifyingKey) -> VerifyingKey {
    derive_key(TOP_LABEL, creator)
}

/// The list id (p2panda topic) of the list created by `creator`.
///
/// This is the binding: the list id commits to the creator's key, so exactly one genesis is valid
/// for a given list.
pub fn list_topic(creator: &VerifyingKey) -> Topic {
    let mut input = Vec::with_capacity(TOPIC_LABEL.len() + creator.as_bytes().len());
    input.extend_from_slice(TOPIC_LABEL);
    input.extend_from_slice(creator.as_bytes());
    Topic::from(Hash::digest(&input))
}

/// The canonical initial members of a top group created by `creator`.
fn top_genesis_members(
    creator: &VerifyingKey,
) -> Vec<(GroupMember<VerifyingKey>, Access<Conditions>)> {
    vec![
        (GroupMember::Individual(*creator), Access::manage()),
        (
            GroupMember::Group(person_group_id(creator)),
            Access::write(),
        ),
    ]
}

/// The canonical initial members of a person's own subgroup.
fn person_genesis_members(
    owner: &VerifyingKey,
) -> Vec<(GroupMember<VerifyingKey>, Access<Conditions>)> {
    vec![(GroupMember::Individual(*owner), Access::manage())]
}

/// The two levels governing one todo list, plus our own device identity.
pub struct AuthGroup {
    /// This device. Author of what we publish, and our member id in whatever subgroup we belong to.
    my_id: VerifyingKey,
    /// The list id. Known before we have seen any operation, and what we check the genesis against.
    topic: Topic,
    /// The top group id, learned by accepting the genesis. `None` until then.
    top_group_id: Option<VerifyingKey>,
    /// `Option` only so we can move the state into `Crdt::process`, which consumes it. Always `Some`
    /// between calls.
    state: Option<GroupState>,
    /// Every operation we have accepted, kept so we can rebuild after a rejection destroys the
    /// (non-`Clone`) CRDT state.
    accepted: Vec<GroupOperation>,
}

impl AuthGroup {
    pub fn new(my_id: VerifyingKey, topic: Topic) -> Self {
        Self {
            my_id,
            topic,
            top_group_id: None,
            state: Some(Crdt::init()),
            accepted: Vec::new(),
        }
    }

    pub fn my_id(&self) -> VerifyingKey {
        self.my_id
    }

    pub fn top_group_id(&self) -> Option<VerifyingKey> {
        self.top_group_id
    }

    /// The id of the subgroup this device manages (creates, and adds other devices to). Only a valid
    /// target once we have created it — see [`AuthGroup::i_manage_a_subgroup`].
    pub fn my_subgroup_id(&self) -> VerifyingKey {
        person_group_id(&self.my_id)
    }

    // --- Building operations to publish -------------------------------------------------------

    /// The two `Create` operations that found a list: the creator's own subgroup, then the top group
    /// referencing it. Two levels from the very first commit.
    pub fn genesis_actions(&self) -> Vec<(VerifyingKey, GroupAction<VerifyingKey, Conditions>)> {
        let me = self.my_id;
        vec![
            (
                person_group_id(&me),
                GroupAction::Create {
                    initial_members: person_genesis_members(&me),
                },
            ),
            (
                top_group_id(&me),
                GroupAction::Create {
                    initial_members: top_genesis_members(&me),
                },
            ),
        ]
    }

    /// The `Create` that establishes this device's own subgroup, for a person joining a list they
    /// were added to. Harmless to re-issue: a duplicate `Create` is dropped by the orderer.
    pub fn create_my_subgroup_action(
        &self,
    ) -> (VerifyingKey, GroupAction<VerifyingKey, Conditions>) {
        let me = self.my_id;
        (
            person_group_id(&me),
            GroupAction::Create {
                initial_members: person_genesis_members(&me),
            },
        )
    }

    /// Admin action: add a *person* to the list at reader or writer level.
    ///
    /// The person is named by their manager device; their subgroup is `person_group_id(device)`.
    /// `Manage` is not offered here — a subgroup cannot hold it (see the module docs).
    pub fn add_user_action(
        &self,
        device: VerifyingKey,
        access: Access<Conditions>,
    ) -> Result<(VerifyingKey, GroupAction<VerifyingKey, Conditions>)> {
        let Some(top) = self.top_group_id else {
            return Err("the list has no top group yet".into());
        };
        if access.level >= AccessLevel::Manage {
            return Err(
                "a person (subgroup) cannot be a list manager; add them as reader or writer".into(),
            );
        }
        Ok((
            top,
            GroupAction::Add {
                member: GroupMember::Group(person_group_id(&device)),
                access,
            },
        ))
    }

    /// Subgroup-manager action: add one of *my* devices, at any level including device-level manage.
    pub fn add_device_action(
        &self,
        device: VerifyingKey,
        access: Access<Conditions>,
    ) -> Result<(VerifyingKey, GroupAction<VerifyingKey, Conditions>)> {
        if !self.i_manage_a_subgroup() {
            return Err(
                "you don't manage a subgroup; only your subgroup's manager adds devices".into(),
            );
        }
        Ok((
            self.my_subgroup_id(),
            GroupAction::Add {
                member: GroupMember::Individual(device),
                access,
            },
        ))
    }

    // --- Applying operations ------------------------------------------------------------------

    /// The current tips across *all* groups. A todo edit names these, so the write check can see the
    /// top group and the author's subgroup at once.
    pub fn heads(&self) -> Vec<Hash> {
        self.state().heads()
    }

    /// The tips relevant to one group — the minimal causal dependencies for a new operation on it.
    /// Keeping a group operation depending only on its own group's history stops a person's
    /// device-management from being entangled with unrelated list activity.
    pub fn heads_for(&self, group: VerifyingKey) -> Vec<Hash> {
        self.state().heads_filtered(&[group])
    }

    /// Apply a group operation. Dependencies must already be applied (the orderer guarantees this).
    ///
    /// `author` and the operation id come from p2panda, not the payload — see [`crate::message`].
    pub fn apply(&mut self, operation: GroupOperation) -> Result<()> {
        let author = operation.author;

        match &operation.action {
            GroupAction::Create { initial_members } => {
                if operation.group_id == top_group_id(&author) {
                    // A top-group genesis is believed only for the creator this list id commits to,
                    // and only in its one canonical shape. This is what makes the initial state
                    // unique per list id.
                    if list_topic(&author) != self.topic {
                        return Err("top-group genesis does not match this list id".into());
                    }
                    if !members_match(initial_members, &top_genesis_members(&author)) {
                        return Err("top-group genesis has non-canonical members".into());
                    }
                    if let Some(existing) = self.top_group_id
                        && existing != operation.group_id
                    {
                        return Err("ignoring a second, conflicting top group".into());
                    }
                } else if operation.group_id == person_group_id(&author) {
                    // A person's subgroup may only be created by, and is named after, that person's
                    // own device — so nobody can plant a subgroup under someone else's identity.
                    if !members_match(initial_members, &person_genesis_members(&author)) {
                        return Err("subgroup genesis has non-canonical members".into());
                    }
                } else {
                    return Err("group id is not a valid derivation of its creator".into());
                }
            }
            // Every other action touches an existing group. `apply_action` panics if that group is
            // absent, so we refuse the operation here rather than let a crafted one crash us.
            _ => {
                if !self.state().has_group(operation.group_id) {
                    return Err("operation on a group we have no Create for".into());
                }
            }
        }

        let is_top_genesis = matches!(operation.action, GroupAction::Create { .. })
            && operation.group_id == top_group_id(&author);

        let state = self.state.take().expect("state is always present");
        match Crdt::process(state, &operation) {
            Ok(state) => {
                self.state = Some(state);
                if is_top_genesis {
                    self.top_group_id = Some(operation.group_id);
                }
                self.accepted.push(operation);
                Ok(())
            }
            Err(err) => {
                self.rebuild();
                Err(format!("group operation rejected: {err}").into())
            }
        }
    }

    // --- Queries ------------------------------------------------------------------------------

    /// What access does `device` have to the list **at** the given group operations?
    ///
    /// The deterministic write check. It resolves `device` transitively from the top group over the
    /// membership snapshot at `deps` — our own walk, because the built-in transitive query is not
    /// point-in-time (see the module docs). Every peer, evaluating the same `deps`, gets the same
    /// answer, whatever else it has since learned.
    ///
    /// `None` means "no access at that point": not reachable from the top group, or the operations
    /// named are ones we did not accept.
    pub fn access_at(&self, device: VerifyingKey, deps: &[Hash]) -> Option<Access<Conditions>> {
        let top = self.top_group_id?;
        if deps.is_empty() {
            return None;
        }
        let dep_set: HashSet<Hash> = deps.iter().copied().collect();
        let states = self.state().inner.state_at(&dep_set).ok()?;
        resolve_access(&states, top, device)
    }

    /// Is `device` a writer as of `deps`?
    pub fn may_write_at(&self, device: VerifyingKey, deps: &[Hash]) -> bool {
        self.access_at(device, deps)
            .is_some_and(|access| access.level >= AccessLevel::Write)
    }

    /// Which **person** is this device, as of `deps`?
    ///
    /// The chat data model identifies authors by person subgroup, not by device, so that a message
    /// written on someone's laptop can be edited on their phone and a profile survives adding a new
    /// device. This is the resolution from one to the other: find the subgroup of the top group that
    /// holds `device`, over the membership snapshot at `deps`.
    ///
    /// Like [`AuthGroup::access_at`] it is point-in-time, so every peer evaluating the same update
    /// attributes it to the same author whatever else it has since learned.
    ///
    /// Two edge cases, both resolved deterministically rather than by rejecting:
    ///
    /// * **In several subgroups.** Nothing forbids a device being a member of two people's
    ///   subgroups. We pick the lowest id, so peers agree; a device in that position has no single
    ///   honest answer anyway.
    /// * **In none.** The list admin is a direct `Individual` member of the top group (subgroups
    ///   cannot be managers — see the module docs), so it may have no subgroup at all. Such a device
    ///   is its own author identity. Since a person subgroup id is derived through a domain-separated
    ///   hash, it can never collide with a raw device key, so the two kinds of author id share a
    ///   keyspace safely.
    ///
    /// `None` means the device had no access at all at `deps` — the same condition under which
    /// [`AuthGroup::may_write_at`] is false, so callers that checked write access first will always
    /// get an answer here.
    pub fn identity_at(&self, device: VerifyingKey, deps: &[Hash]) -> Option<VerifyingKey> {
        let top = self.top_group_id?;
        if deps.is_empty() || self.access_at(device, deps).is_none() {
            return None;
        }

        let dep_set: HashSet<Hash> = deps.iter().copied().collect();
        let states = self.state().inner.state_at(&dep_set).ok()?;

        // Direct subgroups of the top group that contain this device.
        let mut holders: Vec<VerifyingKey> = states
            .get(&top)?
            .access_levels()
            .into_iter()
            .filter_map(|(member, _)| match member {
                GroupMember::Group(subgroup) => Some(subgroup),
                GroupMember::Individual(_) => None,
            })
            .filter(|subgroup| {
                states.get(subgroup).is_some_and(|state| {
                    state
                        .access_levels()
                        .into_iter()
                        .any(|(member, _)| member == GroupMember::Individual(device))
                })
            })
            .collect();

        holders.sort_by_key(|id| id.to_hex());
        Some(holders.first().copied().unwrap_or(device))
    }

    /// Our current access to the list, resolved transitively against *current* membership. Used only
    /// for the courtesy checks below — the enforcement is `access_at`, above.
    pub fn my_access(&self) -> Option<Access<Conditions>> {
        let top = self.top_group_id?;
        self.state()
            .members(top)
            .into_iter()
            .find(|(id, _)| *id == self.my_id)
            .map(|(_, access)| access)
    }

    pub fn i_may_write(&self) -> bool {
        self.my_access()
            .is_some_and(|access| access.level >= AccessLevel::Write)
    }

    /// May we add *people*? Only a direct `Manage` member of the top group can, and — per the
    /// subgroup-manage restriction — that is always a device, not a person.
    pub fn i_may_manage_list(&self) -> bool {
        self.my_access()
            .is_some_and(|access| access.level >= AccessLevel::Manage)
    }

    /// Do we manage our own subgroup — i.e. have we created it, so we may add our own devices? Only
    /// the namesake device can have created `person_group_id(my_id)`, so its mere existence answers
    /// the question.
    pub fn i_manage_a_subgroup(&self) -> bool {
        self.top_group_id.is_some() && self.state().has_group(self.my_subgroup_id())
    }

    /// The list membership, laid out for display: each top-group entry, and for a subgroup its own
    /// members one level down.
    pub fn describe_membership(&self) -> Vec<MemberLine> {
        let Some(top) = self.top_group_id else {
            return Vec::new();
        };

        let mut lines = Vec::new();
        let mut top_members = self.state().root_members(top);
        top_members.sort_by_key(|(member, _)| member.id().to_hex());

        for (member, access) in top_members {
            match member {
                GroupMember::Individual(id) => lines.push(MemberLine {
                    id,
                    access,
                    is_subgroup: false,
                    depth: 0,
                }),
                GroupMember::Group(subgroup) => {
                    lines.push(MemberLine {
                        id: subgroup,
                        access,
                        is_subgroup: true,
                        depth: 0,
                    });
                    let mut devices = self.state().root_members(subgroup);
                    devices.sort_by_key(|(member, _)| member.id().to_hex());
                    for (device, device_access) in devices {
                        lines.push(MemberLine {
                            id: device.id(),
                            access: device_access,
                            is_subgroup: device.is_group(),
                            depth: 1,
                        });
                    }
                }
            }
        }
        lines
    }

    fn state(&self) -> &GroupState {
        self.state.as_ref().expect("state is always present")
    }

    fn rebuild(&mut self) {
        let mut state = Crdt::init();
        for operation in &self.accepted {
            state = Crdt::process(state, operation).expect("operation was accepted before");
        }
        self.state = Some(state);
    }
}

/// One row of the membership display.
pub struct MemberLine {
    pub id: VerifyingKey,
    pub access: Access<Conditions>,
    pub is_subgroup: bool,
    /// 0 for a top-group member, 1 for a device inside a subgroup.
    pub depth: u8,
}

/// Walk the group graph transitively from `top`, over a point-in-time membership snapshot, and
/// return `device`'s effective access.
///
/// This mirrors `p2panda-auth`'s own `members_inner`: descending an edge caps access at the lesser
/// of the parent edge and the child edge, and multiple paths to the same device combine by the
/// greater. The difference is that this reads a `state_at` snapshot rather than current state, which
/// is the whole point — it makes the answer a function of `deps` alone.
fn resolve_access(
    states: &HashMap<VerifyingKey, MembersState>,
    top: VerifyingKey,
    device: VerifyingKey,
) -> Option<Access<Conditions>> {
    let mut best: Option<Access<Conditions>> = None;
    // Skip a group only if we have already reached it with an equal-or-higher cap; the group graph
    // is a DAG (the CRDT rejects cycles), so this terminates.
    let mut seen: HashMap<VerifyingKey, AccessLevel> = HashMap::new();
    let mut stack: Vec<(VerifyingKey, Option<Access<Conditions>>)> = vec![(top, None)];

    while let Some((group_id, cap)) = stack.pop() {
        let cap_level = cap
            .as_ref()
            .map_or(AccessLevel::Manage, |c| c.level.clone());
        if seen.get(&group_id).is_some_and(|prev| *prev >= cap_level) {
            continue;
        }
        seen.insert(group_id, cap_level);

        let Some(group_state) = states.get(&group_id) else {
            continue;
        };

        for (member, edge) in group_state.access_levels() {
            let effective = match &cap {
                Some(cap) => min_access(cap, &edge),
                None => edge,
            };
            match member {
                GroupMember::Individual(id) => {
                    if id == device && best.as_ref().is_none_or(|b| effective.level > b.level) {
                        best = Some(effective);
                    }
                }
                GroupMember::Group(subgroup) => stack.push((subgroup, Some(effective))),
            }
        }
    }

    best
}

fn min_access(a: &Access<Conditions>, b: &Access<Conditions>) -> Access<Conditions> {
    if a.level <= b.level {
        a.clone()
    } else {
        b.clone()
    }
}

/// Compare two member lists ignoring order — for checking a genesis against its canonical template.
fn members_match(
    actual: &[(GroupMember<VerifyingKey>, Access<Conditions>)],
    expected: &[(GroupMember<VerifyingKey>, Access<Conditions>)],
) -> bool {
    actual.len() == expected.len()
        && expected.iter().all(|(member, access)| {
            actual
                .iter()
                .any(|(m, a)| m == member && a.level == access.level)
        })
}

/// Parse "reader" / "writer" / "manager" from the prompt.
pub fn parse_access(name: &str) -> Option<Access<Conditions>> {
    match name {
        "reader" | "read" => Some(Access::read()),
        "writer" | "write" => Some(Access::write()),
        "manager" | "manage" | "admin" => Some(Access::manage()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive `AuthGroup` the way `main` does: build operations, then apply them with an id and
    /// author supplied from "p2panda". Stands in for the network and for the orderer's job of
    /// delivering dependencies first.
    struct World {
        next_id: u64,
    }

    impl World {
        fn new() -> Self {
            Self { next_id: 0 }
        }

        fn device() -> VerifyingKey {
            SigningKey::generate().verifying_key()
        }

        fn op(
            &mut self,
            author: VerifyingKey,
            group_id: VerifyingKey,
            action: GroupAction<VerifyingKey, Conditions>,
            dependencies: Vec<Hash>,
        ) -> GroupOperation {
            self.next_id += 1;
            let id = Hash::digest(self.next_id.to_le_bytes());
            GroupsOperation {
                id,
                author,
                dependencies,
                group_id,
                action,
            }
        }

        /// Found a list under `creator`; return the group and the two genesis ids (subgroup, top).
        fn found(&mut self, creator: VerifyingKey) -> (AuthGroup, Hash, Hash) {
            let mut group = AuthGroup::new(creator, list_topic(&creator));
            let actions = group.genesis_actions();

            let (person_gid, person_action) = actions[0].clone();
            let person_op = self.op(creator, person_gid, person_action, vec![]);
            let person_id = person_op.id;
            group.apply(person_op).expect("subgroup genesis");

            let (top_gid, top_action) = actions[1].clone();
            let top_op = self.op(creator, top_gid, top_action, vec![person_id]);
            let top_id = top_op.id;
            group.apply(top_op).expect("top genesis");

            (group, person_id, top_id)
        }

        /// A person creates their own subgroup (as they do on join).
        fn create_subgroup(&mut self, group: &mut AuthGroup, device: VerifyingKey, topic: Topic) {
            let (person, create) = AuthGroup::new(device, topic).create_my_subgroup_action();
            group
                .apply(self.op(device, person, create, vec![]))
                .expect("subgroup create");
        }
    }

    #[test]
    fn founding_sets_up_two_levels_with_the_creator_as_admin_and_writer() {
        let mut world = World::new();
        let alice = World::device();
        let (group, _, top_id) = world.found(alice);

        assert_eq!(group.top_group_id(), Some(top_group_id(&alice)));
        assert!(group.i_may_manage_list(), "creator manages the list");
        assert!(group.i_may_write(), "creator may write");
        assert!(
            group.i_manage_a_subgroup(),
            "creator manages their own subgroup"
        );
        assert!(group.may_write_at(alice, &[top_id]));
    }

    #[test]
    fn a_genesis_is_only_valid_for_the_list_id_it_commits_to() {
        let mut world = World::new();
        let alice = World::device();
        let mallory = World::device();

        // A group opened for Alice's list, but Mallory publishes his own top-group genesis.
        let mut group = AuthGroup::new(mallory, list_topic(&alice));
        let action = GroupAction::Create {
            initial_members: top_genesis_members(&mallory),
        };
        let forged = world.op(mallory, top_group_id(&mallory), action, vec![]);

        assert!(group.apply(forged).is_err());
        assert!(group.top_group_id().is_none());
    }

    #[test]
    fn a_person_may_only_create_their_own_subgroup() {
        let mut world = World::new();
        let alice = World::device();
        let mallory = World::device();
        let (mut group, _, _) = world.found(alice);

        // Mallory tries to create the subgroup named after Alice's device.
        let action = GroupAction::Create {
            initial_members: person_genesis_members(&mallory),
        };
        let forged = world.op(mallory, person_group_id(&alice), action, vec![]);
        assert!(group.apply(forged).is_err());
    }

    #[test]
    fn admin_adds_a_writer_person_and_their_devices_can_write() {
        let mut world = World::new();
        let alice = World::device();
        let bob_laptop = World::device();
        let bob_phone = World::device();
        let (mut group, _, _) = world.found(alice);

        // Alice adds Bob (a person) as a writer.
        let (top, add) = group.add_user_action(bob_laptop, Access::write()).unwrap();
        group
            .apply(world.op(alice, top, add, group.heads_for(top)))
            .unwrap();

        // Bob creates his subgroup and adds his phone as a writer.
        world.create_subgroup(&mut group, bob_laptop, list_topic(&alice));
        let (person, add_phone) = (
            person_group_id(&bob_laptop),
            GroupAction::Add {
                member: GroupMember::Individual(bob_phone),
                access: Access::write(),
            },
        );
        group
            .apply(world.op(bob_laptop, person, add_phone, group.heads_for(person)))
            .unwrap();

        // As of everything applied, both of Bob's devices may write.
        let deps = group.heads();
        assert!(group.may_write_at(bob_laptop, &deps));
        assert!(group.may_write_at(bob_phone, &deps));
    }

    #[test]
    fn a_reader_persons_devices_may_not_write_even_if_managed_as_writers() {
        let mut world = World::new();
        let alice = World::device();
        let carol_laptop = World::device();
        let carol_phone = World::device();
        let (mut group, _, _) = world.found(alice);

        let (top, add) = group.add_user_action(carol_laptop, Access::read()).unwrap();
        group
            .apply(world.op(alice, top, add, group.heads_for(top)))
            .unwrap();
        world.create_subgroup(&mut group, carol_laptop, list_topic(&alice));

        // Carol makes her phone a *writer within her subgroup* — but her subgroup is only a reader
        // of the list, so the min-cap rule keeps the phone at reader for the list.
        let person = person_group_id(&carol_laptop);
        let add_phone = GroupAction::Add {
            member: GroupMember::Individual(carol_phone),
            access: Access::write(),
        };
        group
            .apply(world.op(carol_laptop, person, add_phone, group.heads_for(person)))
            .unwrap();

        let deps = group.heads();
        assert!(!group.may_write_at(carol_laptop, &deps));
        assert!(!group.may_write_at(carol_phone, &deps));
    }

    #[test]
    fn a_subgroup_cannot_be_made_a_list_manager() {
        let mut world = World::new();
        let alice = World::device();
        let (mut group, _, _) = world.found(alice);

        // The action builder refuses it up front...
        assert!(
            group
                .add_user_action(World::device(), Access::manage())
                .is_err()
        );

        // ... and even a hand-built one is rejected by the CRDT (ManagerGroupsNotAllowed).
        let top = group.top_group_id().unwrap();
        let bogus = GroupAction::Add {
            member: GroupMember::Group(person_group_id(&World::device())),
            access: Access::manage(),
        };
        assert!(
            group
                .apply(world.op(alice, top, bogus, group.heads_for(top)))
                .is_err()
        );
    }

    #[test]
    fn access_is_evaluated_at_the_operation_the_edit_depends_on() {
        let mut world = World::new();
        let alice = World::device();
        let bob = World::device();
        let (mut group, _, top_id) = world.found(alice);

        // Before Bob is added, an edit naming only the genesis is unauthorized for him.
        assert!(!group.may_write_at(bob, &[top_id]));

        let (top, add) = group.add_user_action(bob, Access::write()).unwrap();
        let add_op = world.op(alice, top, add, group.heads_for(top));
        let add_id = add_op.id;
        group.apply(add_op).unwrap();
        world.create_subgroup(&mut group, bob, list_topic(&alice));

        // Judged at the old genesis: still unauthorized. Judged at the current heads (which include
        // his add and his subgroup): authorized. The verdict is fixed by the dependency named.
        assert!(!group.may_write_at(bob, &[top_id]));
        assert!(group.may_write_at(bob, &group.heads()));
        assert!(add_id != top_id);
    }

    #[test]
    fn an_edit_with_no_dependency_is_never_authorized() {
        let mut world = World::new();
        let alice = World::device();
        let (group, _, _) = world.found(alice);
        assert!(!group.may_write_at(alice, &[]));
    }

    #[test]
    fn an_operation_on_an_unknown_group_is_refused_not_panicked() {
        let mut world = World::new();
        let alice = World::device();
        let (mut group, _, _) = world.found(alice);

        // An Add naming a group we have no Create for. `apply_action` would panic on this; we must
        // reject it cleanly instead.
        let unknown = person_group_id(&World::device());
        let action = GroupAction::Add {
            member: GroupMember::Individual(World::device()),
            access: Access::write(),
        };
        assert!(
            group
                .apply(world.op(alice, unknown, action, vec![]))
                .is_err()
        );
    }
}
