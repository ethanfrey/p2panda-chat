// SPDX-License-Identifier: MIT OR Apache-2.0

//! The IPC surface: every `invoke` the frontend can make.
//!
//! Two kinds, and the split is the contract described in [`crate::backend`]:
//!
//! * **Queries** read the current document and return [`crate::views`] types. The UI calls these on
//!   mount to get initial state, and again for whatever a `chat:changed` event says has moved.
//! * **Mutations** change the document and publish the result. They return as soon as the update is
//!   on the wire; the resulting UI update arrives as an event, through exactly the same path a
//!   remote peer's change would. There is no separate "local" code path in the frontend, and so no
//!   way for local and remote rendering to drift apart.
//!
//! ## Timestamps enter here
//!
//! The data layer takes `created_at` as a parameter and never reads a clock — that is what makes it
//! deterministically testable. This is the layer that supplies one, from the system clock. It is
//! author-supplied and untrusted by every peer including us; see the ordering note in
//! [`crate::data::chat`].
//!
//! ## Errors
//!
//! Commands return `Result<T, String>`. A panic in a command takes the app down, and the boxed error
//! type used internally is not `Serialize`, so everything is flattened to a message at this boundary
//! — which is all the frontend can do with it anyway.

use std::time::{SystemTime, UNIX_EPOCH};

use p2panda_core::{Hash, VerifyingKey};
use tauri::State;

use crate::backend::{AppState, Backend, status_of};
use crate::data::auth;
use crate::data::chat::channel::ChannelId;
use crate::data::chat::message::MessageId;
use crate::data::chat::thread::{ThreadId, thread_id};
use crate::views::{ChannelView, MessageView, ProfileView, StatusView, ThreadView};

/// Flatten any error to a string for the IPC boundary.
type IpcResult<T> = std::result::Result<T, String>;

fn fail(err: impl std::fmt::Display) -> String {
    err.to_string()
}

/// Wall-clock milliseconds. Untrusted by every peer, ourselves included — a hint for ordering.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn parse_hash(what: &str, hex: &str) -> IpcResult<Hash> {
    hex.parse::<Hash>()
        .map_err(|err| format!("{what} {hex:?} is not a valid id: {err}"))
}

// --- Queries ------------------------------------------------------------------------------------

/// Who we are, what we may do, and who else is here. The UI's first call.
#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> IpcResult<StatusView> {
    let backend = state.backend().map_err(fail)?;
    Ok(backend.with_session(status_of))
}

/// Every channel in browsing order. `include_archived` is what the UI's "show archived" toggle maps
/// to — archived channels are hidden, never deleted.
#[tauri::command]
pub fn list_channels(
    state: State<'_, AppState>,
    include_archived: Option<bool>,
) -> IpcResult<Vec<ChannelView>> {
    let backend = state.backend().map_err(fail)?;
    Ok(backend.with_session(|session| {
        let channels = if include_archived.unwrap_or(false) {
            session.chat().channels()
        } else {
            session.chat().active_channels()
        };
        channels.into_iter().map(ChannelView::new).collect()
    }))
}

/// One channel. `None` when we have not synced it.
#[tauri::command]
pub fn get_channel(
    state: State<'_, AppState>,
    channel_id: String,
) -> IpcResult<Option<ChannelView>> {
    let backend = state.backend().map_err(fail)?;
    let channel: ChannelId = parse_hash("channel", &channel_id)?;
    Ok(backend.with_session(|session| session.chat().channel(channel).map(ChannelView::new)))
}

/// A channel's main flow, oldest first. Thread replies are not included — ask
/// [`list_thread_messages`] for those.
#[tauri::command]
pub fn list_messages(
    state: State<'_, AppState>,
    channel_id: String,
) -> IpcResult<Vec<MessageView>> {
    let backend = state.backend().map_err(fail)?;
    let channel: ChannelId = parse_hash("channel", &channel_id)?;
    Ok(backend.with_session(|session| {
        let chat = session.chat();
        chat.channel_messages(channel)
            .into_iter()
            .map(|message| MessageView::new(chat, message))
            .collect()
    }))
}

/// A thread's messages, oldest first.
#[tauri::command]
pub fn list_thread_messages(
    state: State<'_, AppState>,
    thread_id: String,
) -> IpcResult<Vec<MessageView>> {
    let backend = state.backend().map_err(fail)?;
    let thread: ThreadId = parse_hash("thread", &thread_id)?;
    Ok(backend.with_session(|session| {
        let chat = session.chat();
        chat.thread_messages(thread)
            .into_iter()
            .map(|message| MessageView::new(chat, message))
            .collect()
    }))
}

/// Every thread in a channel, for a thread sidebar.
#[tauri::command]
pub fn list_threads(state: State<'_, AppState>, channel_id: String) -> IpcResult<Vec<ThreadView>> {
    let backend = state.backend().map_err(fail)?;
    let channel: ChannelId = parse_hash("channel", &channel_id)?;
    Ok(backend.with_session(|session| {
        let chat = session.chat();
        chat.channel_threads(channel)
            .into_iter()
            .map(|thread| ThreadView::new(chat, thread))
            .collect()
    }))
}

/// One message, for resolving a `replyTo` reference the UI has not already loaded. `None` when we
/// have not synced it — a normal state, not an error. See the note on dangling references in
/// [`crate::data::chat`].
#[tauri::command]
pub fn get_message(
    state: State<'_, AppState>,
    message_id: String,
) -> IpcResult<Option<MessageView>> {
    let backend = state.backend().map_err(fail)?;
    let message: MessageId = parse_hash("message", &message_id)?;
    Ok(backend.with_session(|session| {
        let chat = session.chat();
        chat.message(message)
            .map(|message| MessageView::new(chat, message))
    }))
}

/// Everyone's profile.
#[tauri::command]
pub fn list_profiles(state: State<'_, AppState>) -> IpcResult<Vec<ProfileView>> {
    let backend = state.backend().map_err(fail)?;
    Ok(backend.with_session(|session| {
        session
            .chat()
            .profiles()
            .into_iter()
            .map(ProfileView::new)
            .collect()
    }))
}

// --- Mutations ----------------------------------------------------------------------------------
//
// Each follows the same shape: refuse early if we may not write, change the document under the
// session lock, then publish. `publish_chat` is a no-op when nothing changed, so it is always safe
// to call.

/// Set our own display name and status. There is no command to write anyone else's.
#[tauri::command]
pub async fn set_profile(
    state: State<'_, AppState>,
    name: String,
    status: String,
) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    write(&backend, |session| {
        session.chat().set_profile(&name, &status)
    })?;
    backend.publish_chat().await.map_err(fail)
}

/// Create a channel. Returns its new id.
#[tauri::command]
pub async fn create_channel(state: State<'_, AppState>, name: String) -> IpcResult<String> {
    let backend = state.backend().map_err(fail)?;
    let id = write(&backend, |session| {
        session.chat().create_channel(&name, now())
    })?;
    backend.publish_chat().await.map_err(fail)?;
    Ok(id.to_hex())
}

/// Rename a channel. Any writer may do this — see [`crate::data::chat::channel`].
#[tauri::command]
pub async fn rename_channel(
    state: State<'_, AppState>,
    channel_id: String,
    name: String,
) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    let channel = parse_hash("channel", &channel_id)?;
    write(&backend, |session| {
        session.chat().rename_channel(channel, &name)
    })?;
    backend.publish_chat().await.map_err(fail)
}

/// Set a channel's one-line description. Any writer may do this, as with renaming.
#[tauri::command]
pub async fn set_channel_purpose(
    state: State<'_, AppState>,
    channel_id: String,
    purpose: String,
) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    let channel = parse_hash("channel", &channel_id)?;
    write(&backend, |session| {
        session.chat().set_channel_purpose(channel, &purpose)
    })?;
    backend.publish_chat().await.map_err(fail)
}

/// Archive or un-archive a channel. This is what "delete" means here, and it is reversible.
#[tauri::command]
pub async fn archive_channel(
    state: State<'_, AppState>,
    channel_id: String,
    archived: bool,
) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    let channel = parse_hash("channel", &channel_id)?;
    write(&backend, |session| {
        session.chat().set_channel_archived(channel, archived)
    })?;
    backend.publish_chat().await.map_err(fail)
}

/// Reorder the channel list. Indices are into the list [`list_channels`] returned.
#[tauri::command]
pub async fn move_channel(state: State<'_, AppState>, from: usize, to: usize) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    write(&backend, |session| session.chat().move_channel(from, to))?;
    backend.publish_chat().await.map_err(fail)
}

/// Post a message, optionally as a reply or into a thread. Returns its new id.
#[tauri::command]
pub async fn post_message(
    state: State<'_, AppState>,
    channel_id: String,
    body: String,
    reply_to: Option<String>,
    thread_id: Option<String>,
) -> IpcResult<String> {
    let backend = state.backend().map_err(fail)?;
    let channel = parse_hash("channel", &channel_id)?;
    let reply_to = reply_to
        .map(|id| parse_hash("message", &id))
        .transpose()?;
    let thread = thread_id.map(|id| parse_hash("thread", &id)).transpose()?;

    let id = write(&backend, |session| {
        let chat = session.chat();
        let timestamp = now();
        match (thread, reply_to) {
            (Some(thread), _) => chat.post_to_thread(channel, thread, &body, timestamp),
            (None, Some(reply_to)) => {
                chat.reply_to_message(channel, reply_to, &body, timestamp)
            }
            (None, None) => chat.post_message(channel, &body, timestamp),
        }
    })?;

    backend.publish_chat().await.map_err(fail)?;
    Ok(id.to_hex())
}

/// Edit our own message. Refused for anyone else's, here and on every peer.
#[tauri::command]
pub async fn edit_message(
    state: State<'_, AppState>,
    message_id: String,
    body: String,
) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    let message = parse_hash("message", &message_id)?;
    write(&backend, |session| {
        session.chat().edit_message(message, &body, now())
    })?;
    backend.publish_chat().await.map_err(fail)
}

/// Retract our own message. A tombstone: the message keeps its place in the flow.
#[tauri::command]
pub async fn delete_message(state: State<'_, AppState>, message_id: String) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    let message = parse_hash("message", &message_id)?;
    write(&backend, |session| {
        session.chat().delete_message(message)
    })?;
    backend.publish_chat().await.map_err(fail)
}

/// Toggle a reaction on a message. Adding twice is a no-op by construction, so this is safe to wire
/// straight to a click handler.
#[tauri::command]
pub async fn toggle_reaction(
    state: State<'_, AppState>,
    message_id: String,
    emoji: String,
    on: bool,
) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    let message = parse_hash("message", &message_id)?;
    write(&backend, |session| {
        let chat = session.chat();
        if on {
            chat.add_reaction(message, &emoji).map(|_| ())
        } else {
            chat.remove_reaction(message, &emoji)
        }
    })?;
    backend.publish_chat().await.map_err(fail)
}

/// Start the thread on a message. Idempotent — the id derives from the anchor, so two people
/// clicking at once get one thread. Returns its id.
#[tauri::command]
pub async fn start_thread(
    state: State<'_, AppState>,
    channel_id: String,
    anchor_id: String,
) -> IpcResult<String> {
    let backend = state.backend().map_err(fail)?;
    let channel = parse_hash("channel", &channel_id)?;
    let anchor = parse_hash("message", &anchor_id)?;
    let id = write(&backend, |session| {
        session.chat().start_thread(channel, anchor, now())
    })?;
    backend.publish_chat().await.map_err(fail)?;
    Ok(id.to_hex())
}

/// The id of the thread on a message, computed rather than looked up. Lets the UI address a thread
/// before it exists.
#[tauri::command]
pub fn thread_id_for(message_id: String) -> IpcResult<String> {
    Ok(thread_id(parse_hash("message", &message_id)?).to_hex())
}

// --- Membership ---------------------------------------------------------------------------------

/// Admin action: add a person to this server as a reader or writer, by their device's node id.
///
/// `access` is `"reader"` or `"writer"`. A person cannot be a manager — a subgroup may not hold
/// `Manage`, so admin is always a specific device. See [`crate::data::auth`].
#[tauri::command]
pub async fn add_user(
    state: State<'_, AppState>,
    node_id: String,
    access: String,
) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    let (device, access) = parse_member(&node_id, &access)?;
    let message = backend
        .with_session(|session| session.add_user_message(device, access))
        .map_err(fail)?;
    backend.publish_group(message).await.map_err(fail)
}

/// Add one of *our own* devices to our subgroup, at any level including manager.
#[tauri::command]
pub async fn add_device(
    state: State<'_, AppState>,
    node_id: String,
    access: String,
) -> IpcResult<()> {
    let backend = state.backend().map_err(fail)?;
    let (device, access) = parse_member(&node_id, &access)?;
    let message = backend
        .with_session(|session| session.add_device_message(device, access))
        .map_err(fail)?;
    backend.publish_group(message).await.map_err(fail)
}

fn parse_member(
    node_id: &str,
    access: &str,
) -> IpcResult<(VerifyingKey, p2panda_auth::Access<crate::data::auth::Conditions>)> {
    let device = node_id
        .parse::<VerifyingKey>()
        .map_err(|err| format!("{node_id:?} is not a node id: {err}"))?;
    let access = auth::parse_access(access)
        .ok_or_else(|| format!("unknown access level {access:?}: use reader, writer or manager"))?;
    Ok((device, access))
}

/// Refuse early if we may not write, then run a local change under the session lock.
///
/// The refusal is a courtesy — every peer would reject the operation anyway — but it turns a message
/// that silently vanishes into an error the UI can show, and it saves the publish.
fn write<T>(
    backend: &Backend,
    change: impl FnOnce(&mut crate::session::Session) -> crate::Result<T>,
) -> IpcResult<T> {
    backend.with_session(|session| {
        session.require_write().map_err(fail)?;
        change(session).map_err(fail)
    })
}
