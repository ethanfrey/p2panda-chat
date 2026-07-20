# Backend (`src-tauri/`)

The Rust side of the Tauri app. This is where the real work happens: peer-to-peer sync, the CRDT
data model, group/device auth, and local persistence.

## Files

- `src/main.rs` — thin binary entry, calls `p2panda_chat_lib::run()`.
- `src/lib.rs` — the actual entry point: builds the Tauri app, registers plugins and the
  `invoke_handler` command list. Currently only the template `greet` command.
- `src/data/` — the data layer. Read `data/chat/mod.rs` first; its module docs explain the whole
  design and every other file assumes them.
  - `auth.rs` — two-level access groups (people, and their devices) over `p2panda-auth`.
  - `chat/` — the V1 chat model in one Loro document: `profile`, `channel`, `message`, `reaction`,
    `thread`, one file each.
  - `message.rs` — what goes on the wire. Not to be confused with `chat/message.rs`.
  - `ordering.rs` — holds an operation until its dependencies have been applied.
  - `persistence.rs` — key custody, log database, topic ids.
- `Cargo.toml` — crate is `p2panda-chat`, library target `p2panda_chat_lib`.
- `tauri.conf.json` — app config, window setup, bundle identifier, and the Vite dev-server hookup.
- `capabilities/` — Tauri v2 permissions. A plugin command the frontend calls must be allowed here
  or the `invoke` fails at runtime.

## Stack

Two libraries define this layer:

- **[p2panda](https://p2panda.org/)** — the p2p layer: peer discovery, topic-based sync, append-only
  logs, and the group/device access control in `data/auth.rs`.
- **[Loro](https://loro.dev/)** — CRDTs for the chat data model, so concurrent edits from multiple
  peers and devices merge without a server.

Roughly: p2panda moves and authenticates bytes between peers; Loro decides what the merged state
means.

One topic is one chat server: one membership, one set of channels, all the messages inside them.
Authorization happens in **two layers**, and both matter — p2panda's group answers "may this device
write here at all?", and `ChatDoc::import_checked` answers "may that person write *these particular
items*?" by merging into a throwaway fork and validating before committing. Nothing that fails
validation ever reaches the real document, because Loro has no un-merge.

Nothing here is wired into the Tauri commands yet, so the whole `data` module reads as dead code —
that is expected, not rot.

## Conventions

- Expose functionality to the frontend as `#[tauri::command]` fns registered in
  `generate_handler![]` in `lib.rs`. Payload and return types derive `serde::{Serialize,
  Deserialize}`.
- Commands return `Result<T, String>` (or a serializable error type) rather than panicking — a panic
  in a command takes down the app.
- Push updates to the UI with `AppHandle::emit` / `emit_to` rather than making the frontend poll.
- Long-running p2p tasks belong on their own tokio tasks, with shared state behind Tauri's
  `.manage()` — commands must not block the IPC thread.
- `cargo clippy` should be clean; run `cargo check` from this directory.
