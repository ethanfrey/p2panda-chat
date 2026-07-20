// SPDX-License-Identifier: MIT OR Apache-2.0

//! What travels over the topic.
//!
//! One topic carries two kinds of message, so they are one enum:
//!
//! * [`Message::Group`] — a membership change. The group is a CRDT and this is one of its
//!   operations, replicated like any other data.
//! * [`Message::Chat`] — a Loro update to the chat document, plus **the group operations it was
//!   written against**.
//!
//! ## The dependency is the whole point
//!
//! [`ChatUpdate::depends_on`] holds the tips of the group DAGs as the author saw them when they made
//! the edit. With the two-level group structure there are *several* groups (the top group and each
//! person's subgroup), so this is the tips across all of them — enough that a receiver can resolve
//! the author transitively from the top group down through their subgroup. It does two jobs at once:
//!
//! * It tells every receiver *which membership snapshot to judge the edit against*, so the write
//!   check is deterministic (see [`super::auth::AuthGroup::access_at`]) and *who the author is* as
//!   the chat model understands authorship — a person, not a device (see
//!   [`super::auth::AuthGroup::identity_at`]).
//! * It tells [`super::ordering::Orderer`] *when* the edit can be judged: not until those group
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
/// initial member list inline, which makes it an order of magnitude bigger than a chat update, and
/// chat updates are the common case. It changes nothing about the encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Group(Box<GroupMessage>),
    Chat(ChatUpdate),
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

/// A change to the chat document: new messages, edits, reactions, channel changes — the Loro layer
/// does not distinguish them, and neither does the wire.
///
/// Note what a receiver does with this, because it is two checks and not one. `depends_on` gets it
/// past the group ("may this device write here at all, and who are they?"), and then the update
/// itself must still get past [`super::chat::ChatDoc::import_checked`] ("may that person write
/// *these particular items*?"). The first is the `todo-auth` model this grew from; the second is
/// what chat needs on top of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatUpdate {
    /// The group operations this edit was written against — the group DAG tips the author had
    /// applied. Never empty: an edit with no dependency cannot be authorized.
    pub depends_on: Vec<Hash>,
    /// Opaque Loro update bytes. `serde_bytes` makes CBOR encode this as a byte string rather than
    /// an array of integers.
    #[serde(with = "serde_bytes")]
    pub update: Vec<u8>,
}
