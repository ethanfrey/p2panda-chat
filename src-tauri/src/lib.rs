// SPDX-License-Identifier: MIT OR Apache-2.0

//! A peer-to-peer chat app: p2panda for replication and access control, Loro for the data.
//!
//! The layers, outermost first:
//!
//! | module | |
//! |---|---|
//! | [`commands`] | the `invoke` surface the frontend calls |
//! | [`views`] | the serializable shapes those commands return |
//! | [`backend`] | the p2panda node, the stream task, and the events pushed to the UI |
//! | [`session`] | receive → order → authorize → apply, with no p2panda or Tauri in sight |
//! | [`data`] | the chat model, the access groups, ordering, and persistence |
//!
//! Read [`data::chat`] first if you are here to change behaviour: it explains the document layout
//! and the two layers of authorization that everything above it assumes.

mod backend;
mod commands;
mod data;
mod session;
mod views;

use backend::{AppState, Backend};

// TODO: Revisit this choice. Maybe anyhow?
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                // p2panda and its network stack are chatty at info level, and none of it is ours.
                .level_for("p2panda", log::LevelFilter::Warn)
                .level_for("iroh", log::LevelFilter::Warn)
                .build(),
        )
        .manage(AppState::default())
        .setup(|app| {
            // Startup is async and slow — a keyring round trip, a database migration, a network
            // bind — so it runs on a task and the window paints immediately. Commands issued before
            // it lands get "the backend is still starting up"; the UI is told to proceed by the
            // `chat:ready` event.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = Backend::start(handle).await {
                    // Nothing works without the backend, so this is loud on purpose.
                    log::error!("could not start the chat backend: {err}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // queries
            commands::get_status,
            commands::list_channels,
            commands::get_channel,
            commands::list_messages,
            commands::list_thread_messages,
            commands::list_threads,
            commands::get_message,
            commands::list_profiles,
            commands::thread_id_for,
            // mutations
            commands::set_profile,
            commands::create_channel,
            commands::rename_channel,
            commands::set_channel_purpose,
            commands::archive_channel,
            commands::move_channel,
            commands::post_message,
            commands::edit_message,
            commands::delete_message,
            commands::toggle_reaction,
            commands::start_thread,
            // membership
            commands::add_user,
            commands::add_device,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
