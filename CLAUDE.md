# p2panda-chat

A fully peer-to-peer group chat app (a self-hosted-free alternative to Rocket.Chat / Discord),
built as a Tauri v2 desktop app. See [CHAT.md](./CHAT.md) for the product idea and roadmap —
that document is the source of truth for *what* we're building; this one covers *how*.

## Layout

| Path         | What                                                            |
| ------------ | --------------------------------------------------------------- |
| `src/`       | React + TypeScript frontend (Vite). See `src/CLAUDE.md`.          |
| `src-tauri/` | Rust backend — p2p networking, CRDT state, persistence. See `src-tauri/CLAUDE.md`. |
| `CHAT.md`    | Design notes and versioned feature roadmap (V1/V2/V3).            |

The split is deliberate: **all p2p, crypto, and data-model logic lives in Rust.** The frontend is a
view layer that calls Tauri commands and listens for events. Don't push chat state management into
React.

## Commands

Run from the repo root; the package manager is `pnpm`.

```sh
pnpm install          # install frontend deps
pnpm tauri dev        # run the app (starts Vite + Rust, hot-reloads both)
pnpm tauri build      # production bundle
pnpm build            # typecheck + build frontend only (tsc && vite build)
```

For Rust-only checks, `cargo check` / `cargo clippy` / `cargo test` from `src-tauri/`.

## Conventions

- Frontend and backend talk over Tauri's IPC only: `invoke()` for commands, `listen()` for
  backend-pushed events. No other channel.
- Keep the IPC surface small and typed on both sides — a Rust `serde` struct and a matching TS
  interface for every payload.
- The project is early. Prefer getting the data model and sync right over polish; UI is scaffolding
  until the V1 model in `CHAT.md` works end to end.
