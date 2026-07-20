# Frontend (`src/`)

React 19 + TypeScript, bundled by Vite. This is the view layer for the Rust backend described in
`src-tauri/CLAUDE.md`.

## Files

- `main.tsx` — entry point, mounts `<App />` into `#root` (see `index.html` at the repo root).
- `App.tsx` — root component. Currently the Tauri starter template; will become the chat shell.
- `App.css` — styles. No CSS framework is in use yet.
- `assets/` — static assets imported by components; `public/` at the repo root serves files as-is.

## Talking to the backend

Everything goes through `@tauri-apps/api`:

```ts
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const channels = await invoke<Channel[]>("list_channels");
const unlisten = await listen<Message>("message:new", (e) => { /* ... */ });
```

Rules of thumb:

- **The backend owns state.** React holds only what's needed to render. When a message arrives, the
  Rust side emits an event and the UI re-reads or patches — the frontend never derives chat state
  from its own writes.
- Every `invoke` payload/response gets an explicit TS type mirroring the Rust `serde` struct. Keep
  those types in one place as they grow (a `src/types.ts` or `src/bindings/`).
- Chat is real-time: assume updates arrive unprompted at any time, including for views that aren't
  focused.

## Conventions

- Function components with hooks; no class components.
- `pnpm build` runs `tsc` first — it must typecheck. Avoid `any`.
- Don't add a state library, router, or component kit without a reason; the app is small so far.
