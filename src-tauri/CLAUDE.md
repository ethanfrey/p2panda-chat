# Backend (`src-tauri/`)

The Rust side of the Tauri app, and where nearly all the real work happens: peer-to-peer sync, the
CRDT data model, group/device access control, and local persistence. The frontend is a view layer
over this.

## Layers

Outermost first. Each layer knows about the one below it and nothing above it.

| module | what |
| --- | --- |
| `src/commands.rs` | every `invoke` the frontend can make — queries and mutations |
| `src/views.rs` | the serializable shapes those commands return (ids as hex strings, `camelCase`) |
| `src/backend.rs` | the p2panda node, the stream task, and the events pushed to the UI |
| `src/session.rs` | receive → order → authorize → apply, with no p2panda or Tauri in it |
| `src/data/` | the chat model, access groups, ordering, persistence |
| `src/lib.rs` | Tauri builder: plugins, managed state, `setup`, the command list |
| `src/main.rs` | thin binary entry, calls `p2panda_chat_lib::run()` |

Inside `src/data/`:

| file | what |
| --- | --- |
| `chat/mod.rs` | **read this first** — the document layout and the two layers of authorization |
| `chat/{profile,channel,message,reaction,thread}.rs` | one item type each: its fields, its ownership rule, its tests |
| `auth.rs` | two-level access groups (people, and their devices) over `p2panda-auth` |
| `message.rs` | what goes on the wire. Not to be confused with `chat/message.rs` |
| `ordering.rs` | holds an operation until the operations it depends on have been applied |
| `persistence.rs` | key custody, the log database, the stored server id |

`ref/` is the CLI example this was built from — reference only, not compiled.

## How it fits together

**One topic is one chat server**: one membership, one set of channels, all the messages in them.

Authorization has **two layers**, and both matter:

1. *Coarse* — p2panda's group answers "may this device write here at all?", evaluated at the group
   operations the update named, so every peer decides identically regardless of sync order.
2. *Fine* — `ChatDoc::import_checked` answers "may that person write *these particular items*?" by
   merging into a throwaway fork, checking each touched item, and only then committing. Nothing that
   fails validation ever reaches the real document, because Loro has no un-merge.

Authors are **people, not devices**: `AuthGroup::identity_at` resolves a signing device to its person
subgroup, so you can edit from your phone a message you posted from your laptop.

## Lifecycle

`lib.rs` `setup` spawns `Backend::start` on the async runtime and returns immediately, so the window
paints while the keyring, database and network come up. Commands issued before it finishes return
"the backend is still starting up"; the UI waits for a `chat:ready` event.

The `p2panda::Node` is owned by `Backend`, which is owned by Tauri's managed state, so it lives
exactly as long as the app. **p2panda 0.7 has no `shutdown` — dropping the node is the shutdown** — so
that ownership is the lifecycle contract, not an incidental detail.

On first run the app **founds a server** whose id is derived from this device's key, and stores it.
Later runs resume it.

## Events

The UI queries for initial state and then reacts to events:

| event | payload | meaning |
| --- | --- | --- |
| `chat:ready` | `StatusView` | backend is up; query initial state now |
| `chat:changed` | `Change[]` | these items changed — re-query them |
| `chat:status` | `StatusView` | our access or the membership changed |
| `chat:sync` | `SyncView` | a sync session started or ended |

`chat:changed` says *what* changed, never *how*; the UI re-queries. That keeps payloads bounded and
means a dropped or coalesced event can never leave the UI showing something the document does not
say. A local mutation produces the same event a remote peer's change would, so there is no separate
"local" path in the frontend to drift out of step.

## Conventions

- **Never hold the session lock across an `await`.** It is a `std::sync::Mutex`. Every publish path
  is three steps: build the message under the lock, publish without it, apply the result under it
  again.
- Commands return `Result<T, String>`; internal code uses `crate::Result`. A panic in a command takes
  the app down.
- The data layer never reads a clock — `created_at` is a parameter. `commands.rs` supplies it. That
  is what keeps the data layer deterministically testable, so keep timestamps out of it.
- New IPC surface means a `#[tauri::command]` in `commands.rs`, a view type in `views.rs`, and an
  entry in the `generate_handler!` list in `lib.rs`. Missing the third is the usual mistake.
- Expose functionality to the UI as commands and events; don't make the frontend poll.

## Commands

```sh
cargo build            # from src-tauri/
cargo test             # 75 tests, including one against a real p2panda node
cargo clippy --lib --all-targets
pnpm tauri dev         # from the repo root — runs the whole app
```

`P2PANDA_CHAT_PROFILE=<name>` uses a different profile directory, which is how you run two instances
side by side to exercise peer-to-peer behaviour on one machine. `P2PANDA_CHAT_KEY_FILE=1` stores the
private key in a file instead of the OS keyring — needed on a headless box or container with no
Secret Service, where the app otherwise cannot start.

State lives in `~/.p2panda-chat/chat/<profile>/`. **The key and the database are one unit** — keeping
the key while losing the database forks your own append-only log and peers will reject you forever.
See the module docs in `persistence.rs`.

## Known gaps

- **No network configuration.** Default local-area node: mDNS discovery, no relay, no bootstrap.
  Fine on one machine; reaching peers across a NAT needs at least a relay. TODO in `backend.rs`.
- **No joining someone else's server.** Everything under it exists (`Session::subgroup_message`, the
  `wants_subgroup` flag); it needs UI to paste a server id into and a stream restart.
- **No removal or demotion.** The group CRDT has `Remove`/`Demote` and nothing issues them; a removed
  member can still write against an old group head. See `ref/README.md`.
- **A rejected update silences that peer permanently**, because one peer's Loro ops form a chain. See
  the module docs in `data/chat/mod.rs` — this is documented behaviour, not a bug to be surprised by.
- **No read control.** Anyone who knows the server id syncs everything; that needs `p2panda-spaces`.
