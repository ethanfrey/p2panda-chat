# Backend (`src-tauri/`)

The Rust side of the Tauri app. This is where the real work happens: peer-to-peer sync, the CRDT
data model, group/device auth, and local persistence.

## Files

- `src/main.rs` — thin binary entry, calls `p2panda_chat_lib::run()`.
- `src/lib.rs` — the actual entry point: builds the Tauri app, registers plugins and the
  `invoke_handler` command list. Currently only the template `greet` command.
- `Cargo.toml` — crate is `p2panda-chat`, library target `p2panda_chat_lib`.
- `tauri.conf.json` — app config, window setup, bundle identifier, and the Vite dev-server hookup.
- `capabilities/` — Tauri v2 permissions. A plugin command the frontend calls must be allowed here
  or the `invoke` fails at runtime.

## Stack (to come)

Two libraries will define this layer; neither is wired up yet:

- **[p2panda](https://p2panda.org/)** — the p2p layer: peer discovery, gossip/topic-based sync,
  append-only logs, and the identity primitives behind the group/device auth tree sketched in
  `CHAT.md`.
- **[Loro](https://loro.dev/)** — CRDTs for the chat data model (channels, messages, reactions,
  threads), so concurrent edits from multiple peers and devices merge without a server.

Roughly: p2panda moves and authenticates bytes between peers; Loro decides what the merged state
means. Persistence and materialized views for the UI sit on top. Details get filled in here as the
V1 model from `CHAT.md` lands — don't invent an architecture ahead of that, ask.

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
