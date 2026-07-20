// SPDX-License-Identifier: MIT OR Apache-2.0

//! What travels over the topic.
//!
//! One topic carries two kinds of message, so they are one enum:
//!
//! * [`Message::Group`] — a membership change. The group is a CRDT and this is one of its
//!   operations, replicated like any other data.
//! * [`Message::Todo`] — a Loro update, plus **the group operations it was written against**.
//!
//! ## The dependency is the whole point
//!
//! `TodoMessage::depends_on` holds the tips of the group DAGs as the author saw them when they made
//! the edit. With the two-level group structure there are *several* groups (the top group and each
//! person's subgroup), so this is the tips across all of them — enough that a receiver can resolve
//! the author transitively from the top group down through their subgroup. It does two jobs at once:
//!
//! * It tells every receiver *which membership snapshot to judge the edit against*, so the write
//!   check is deterministic (see [`crate::auth::AuthGroup::access_at`]).
//! * It tells [`crate::ordering::Orderer`] *when* the edit can be judged: not until those group
//!   operations have been applied.
//!
//! (A *group* operation, by contrast, names only its own group's tips — see
//! `AuthGroup::heads_for` — so people's subgroups stay causally independent of one another.)
//!
//! An edit that names no group operation is rejected. There is no membership snapshot to judge it
//! against, and accepting it would mean accepting writes from an author whose rights we cannot
//! establish — so "no dependency" is not a shortcut, it is a refusal.
//!
//! ## What is *not* in the payload
//!
//! The author. `GroupsOperation` has an `author` field, and we fill it in on receipt from
//! `ProcessedOperation::author()` — the key p2panda verified the signature against — rather than
//! from anything the sender wrote down. Same for the operation's id, which is the p2panda operation
//! hash. Trusting a self-declared author would make the entire group meaningless: anyone could sign
//! an operation claiming to be the admin.
//!
//! This is why the payload carries the *action* and its *dependencies* but not the identity: those
//! two fields come from a layer that cannot be lied to.

use p2panda_auth::group::GroupAction;
use p2panda_core::{Hash, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::auth::Conditions;

/// The message type replicated over the topic.
///
/// The group variant is boxed only to keep the enum small: a `GroupAction::Create` carries its
/// initial member list inline, which makes it an order of magnitude bigger than a todo update, and
/// todo updates are the common case. It changes nothing about the encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Group(Box<GroupMessage>),
    Todo(TodoMessage),
}

/// A membership change: create the group, add a member, ...
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMessage {
    /// The group being changed. Equal to the public key of whoever created it.
    pub group_id: VerifyingKey,
    /// What is being done: `Create`, `Add`, `Remove`, `Promote`, `Demote`.
    pub action: GroupAction<VerifyingKey, Conditions>,
    /// The tips of the group DAG this action was built on.
    pub dependencies: Vec<Hash>,
}

/// A change to the todo list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoMessage {
    /// The group operations this edit was written against — the group DAG tips the author had
    /// applied. Never empty: an edit with no dependency cannot be authorized.
    pub depends_on: Vec<Hash>,
    /// Opaque Loro update bytes. `serde_bytes` makes CBOR encode this as a byte string rather than
    /// an array of integers.
    #[serde(with = "serde_bytes")]
    pub update: Vec<u8>,
}
