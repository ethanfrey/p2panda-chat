// SPDX-License-Identifier: MIT OR Apache-2.0

//! What crosses the IPC boundary.
//!
//! Every type here is a plain serializable mirror of something in [`crate::data`], and that
//! duplication is the point rather than an accident:
//!
//! * **Ids are hex strings.** `Hash` and `VerifyingKey` have their own serde encodings, and none of
//!   them is "a string a TypeScript `Map` can key on". Converting once, here, keeps every id on the
//!   frontend a `string` and stops the shape of a p2panda type leaking into React.
//! * **The wire format is ours to keep stable.** `Message` and friends are free to change with the
//!   data model; the UI contract changes only when this file does.
//! * **Views can join.** [`MessageView`] carries its reaction summary and the author's display name
//!   inline, because the alternative is the frontend issuing a query per message. The data layer
//!   deliberately does not join — see the note on dangling references in [`crate::data::chat`] — so
//!   joining is exactly this layer's job.
//!
//! All fields are `camelCase` on the wire.

use serde::Serialize;

use crate::data::chat::channel::Channel;
use crate::data::chat::message::Message;
use crate::data::chat::profile::Profile;
use crate::data::chat::thread::Thread;
use crate::data::chat::{AuthorId, ChatDoc};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelView {
    pub id: String,
    pub name: String,
    pub purpose: String,
    pub archived: bool,
    pub created_by: String,
    pub created_at: i64,
}

impl ChannelView {
    pub fn new(channel: Channel) -> Self {
        Self {
            id: channel.id.to_hex(),
            name: channel.name,
            purpose: channel.purpose,
            archived: channel.archived,
            created_by: channel.created_by.to_hex(),
            created_at: channel.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageView {
    pub id: String,
    pub author: String,
    /// The author's display name, or their short id when they have not set a profile. Resolved here
    /// so the frontend never has to hold a profile lookup table just to render a message list.
    pub author_name: String,
    pub channel: String,
    pub thread: Option<String>,
    pub reply_to: Option<String>,
    pub text: String,
    pub created_at: i64,
    pub edited_at: Option<i64>,
    pub deleted: bool,
    pub reactions: Vec<ReactionView>,
    /// Whether this message has a thread hanging off it, so the UI can show the affordance without
    /// a query per message.
    pub has_thread: bool,
}

impl MessageView {
    pub fn new(chat: &ChatDoc, message: Message) -> Self {
        let reactions = chat
            .reaction_summary(message.id)
            .into_iter()
            .map(|(emoji, count)| ReactionView {
                mine: chat
                    .message_reactions(message.id)
                    .iter()
                    .any(|r| r.emoji == emoji && r.author == chat.me()),
                emoji,
                count,
            })
            .collect();

        Self {
            id: message.id.to_hex(),
            author_name: display_name(chat, message.author),
            author: message.author.to_hex(),
            channel: message.channel.to_hex(),
            thread: message.thread.map(|id| id.to_hex()),
            reply_to: message.reply_to.map(|id| id.to_hex()),
            text: message.text,
            created_at: message.created_at,
            edited_at: message.edited_at,
            deleted: message.deleted,
            reactions,
            has_thread: chat.thread_on(message.id).is_some(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReactionView {
    pub emoji: String,
    pub count: usize,
    /// Whether *we* are one of the reactors, so the UI can highlight it and toggle it off.
    pub mine: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileView {
    pub author: String,
    pub name: String,
    pub status: String,
}

impl ProfileView {
    pub fn new(profile: Profile) -> Self {
        Self {
            author: profile.author.to_hex(),
            name: profile.name,
            status: profile.status,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadView {
    pub id: String,
    pub channel: String,
    pub anchor: String,
    pub created_by: String,
    pub created_at: i64,
    pub message_count: usize,
}

impl ThreadView {
    pub fn new(chat: &ChatDoc, thread: Thread) -> Self {
        Self {
            message_count: chat.thread_messages(thread.id).len(),
            id: thread.id.to_hex(),
            channel: thread.channel.to_hex(),
            anchor: thread.anchor.to_hex(),
            created_by: thread.created_by.to_hex(),
            created_at: thread.created_at,
        }
    }
}

/// Who we are and what we may do. The first thing the UI asks for, and re-fetched whenever a
/// `chat:status` event says our standing changed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    /// The chat server's topic id. Share this to let someone else join.
    pub server_id: String,
    /// This device's node id. What an admin needs in order to add you.
    pub device_id: String,
    /// Who we are as an author — a person, so shared across our devices.
    pub author_id: String,
    /// `"manage"`, `"write"`, `"read"`, `"pull"`, or `"none"`.
    pub access: String,
    pub may_write: bool,
    pub may_manage: bool,
    /// Operations waiting on group operations we have not seen. Non-zero mid-sync is normal.
    pub pending: usize,
    pub members: Vec<MemberView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberView {
    pub id: String,
    pub access: String,
    /// A person (a subgroup) rather than a device.
    pub is_subgroup: bool,
    /// `0` for a top-level member, `1` for a device inside someone's subgroup.
    pub depth: u8,
    pub is_me: bool,
}

/// A person's display name, falling back to a short id.
///
/// Nobody has a profile until they write one, and a message from someone whose profile has not
/// synced yet still has to render — so this never fails.
fn display_name(chat: &ChatDoc, author: AuthorId) -> String {
    match chat.profile(author) {
        Some(profile) if !profile.name.is_empty() => profile.name,
        _ => author.to_hex()[..8].to_string(),
    }
}
