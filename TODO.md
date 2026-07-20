# TODO — finishing V1

Where V1 stands and what to do next, in the order that de-risks fastest. See [CHAT.md](./CHAT.md)
for what V1 is supposed to be, and `src-tauri/CLAUDE.md` for how the backend is organised.

## Where things stand

The backend is complete and the frontend has not been started.

**Done:** the V1 data model (channels, messages, replies, reactions, threads, profiles) on Loro, with
two-level access groups over p2panda; a live p2panda node wired into the Tauri lifecycle; ~20 IPC
commands and four `chat:` events; 75 passing tests.

**Not done:** any UI. `src/App.tsx` is still the Tauri starter template.

**Tested:** the data model and its authorization rules; the session loop (founding, joining, out-of
-order delivery, rejected writes) against a hand-rolled wire; one test against a real p2panda node
covering serialization, author attribution, publish-hash stability, and rebuilding a server from the
stream alone.

**Untested:** everything above `session.rs`. No test invokes a `#[tauri::command]`, so a wrong
argument name, a bad `serde` rename, or a command missing from the `generate_handler!` list in
`lib.rs` will only surface when the frontend calls it. Nothing has ever run as an actual app.

---

## 1. Run the app solo, by hand

Smallest possible step, and it validates the entire stack end to end for the first time. Do this
before writing any UI — if the keyring, the SQLite store or the node bind is wrong, you want to know
now and not while also debugging React.

```sh
pnpm tauri dev
```

Then from the browser devtools console in the app window:

```js
const { invoke } = window.__TAURI__.core
await invoke('get_status')                                  // who am I, what may I do
await invoke('create_channel', { name: 'general' })
await invoke('list_channels', {})
await invoke('post_message', { channelId: '<id>', body: 'hello' })
await invoke('list_messages', { channelId: '<id>' })
```

What to check:
- `get_status` reports `access: "manage"` and `mayWrite: true` on first run — the founder is admin.
- Restarting the app keeps the channel and the message. That proves the log replay path, which is the
  one thing nothing else exercises.
- The Rust log shows `chat backend ready: profile=default (...) server=... device=...`.

Two things likely to bite here:
- **The OS keyring.** No Secret Service (headless, container, some minimal desktops) means the app
  cannot start. Set `P2PANDA_CHAT_KEY_FILE=1` to fall back to a key file.
- **Argument naming.** Commands take `camelCase` from JS (`channelId`, not `channel_id`). If a
  command 404s or complains about a missing argument, that is the first thing to check.

## 2. Build the V1 frontend

The bulk of the remaining work. `src/CLAUDE.md` has the frontend conventions; the backend contract is
the command list in `src-tauri/src/commands.rs` and the event table in `src-tauri/CLAUDE.md`.

The shape the backend expects:

1. On mount, wait for `chat:ready` (or call `get_status` and retry on "still starting up").
2. Query initial state: `get_status`, `list_channels`, `list_messages`.
3. Subscribe to `chat:changed` and re-query what it names. It says *what* changed, never *how* — that
   is deliberate, so the UI cannot drift out of step with the document.
4. Mutations do not update local state directly. They publish, and the resulting `chat:changed` comes
   back through the same path a remote peer's change would. Resist the urge to add an optimistic
   local path; having one is how local and remote rendering diverge.

Roughly in order:
- [ ] Channel sidebar (`list_channels`, `create_channel`, `rename_channel`, `archive_channel`,
      `move_channel`)
- [ ] Message list and composer (`list_messages`, `post_message`, `edit_message`, `delete_message`).
      Render `deleted` as a tombstone and `editedAt` as "(edited)".
- [ ] Replies — `postMessage({ replyTo })`, and resolve the quoted message with `get_message`, which
      returns `null` for a message not yet synced. That is normal, not an error; show a placeholder.
- [ ] Reactions (`toggle_reaction`). `MessageView.reactions` already carries `{emoji, count, mine}`,
      so the row needs no extra queries.
- [ ] Threads (`start_thread`, `list_threads`, `list_thread_messages`, `post_message` with
      `threadId`). `thread_id_for` computes a thread's id from its anchor without a round trip.
- [ ] Profiles (`set_profile`, `list_profiles`). `MessageView.authorName` is resolved server-side, so
      the message list does not need the profile table.
- [ ] Members panel (`StatusView.members`) and an "add user" form (`add_user` with a node id).
- [ ] TypeScript types mirroring `src-tauri/src/views.rs`. Hand-written is fine at this size; note
      that nothing keeps them in step with Rust automatically.

## 3. Integration tests across the IPC boundary

Once the UI exists there is something to drive. Two layers worth having:

- [ ] **Rust-side command tests.** Build a Tauri app with `tauri::test::mock_builder`, invoke each
      command, assert on the JSON. Cheap, and it closes the "wrong argument name / missing from
      `generate_handler!`" gap that nothing currently covers.
- [ ] **End-to-end.** WebDriver via `tauri-driver`, or drive the devtools console. Worth it for the
      flows that span layers: post a message and see it render; restart and see it persist.

## 4. Networking

`Backend::start` currently spawns a default local-area node — mDNS discovery, no relay, no bootstrap
— with a TODO where the configuration goes. Enough for one machine; nothing crosses a NAT.

- [ ] Decide on a `network_id` so unrelated p2panda apps do not share a swarm.
- [ ] Relay and bootstrap configuration (`NodeBuilder::relay_url`, `::bootstrap`), and where those
      values come from — hardcoded, config file, or UI.
- [ ] Surface connectivity in the UI. `chat:sync` already fires on session start/end with the peer id.

## 5. Multi-node testing

- [ ] **Two instances, one machine.** `P2PANDA_CHAT_PROFILE=alice` and `P2PANDA_CHAT_PROFILE=bob` use
      separate profile directories and identities. This is the first real test of the group flow:
      Alice founds, Bob runs `get_status` for his device id, Alice calls `add_user`, Bob's subgroup is
      created automatically on the next sync, Bob writes.
- [ ] **Two machines**, once §4 lands.
- [ ] **The rules, over a real wire.** These are unit-tested but never proven against real nodes:
      a non-member's write is rejected; a reader may not write; one person may not edit another's
      message; a message arriving before the group operation that authorizes it waits rather than
      being refused.
- [ ] Multi-device: one person, two devices, `add_device`, and confirm a message posted on one can be
      edited on the other — the payoff of authorship being a person rather than a device.

## 6. Joining someone else's server

Currently every profile founds its own server on first run and there is no way to join another.
Everything underneath exists — `Session::subgroup_message`, the `wants_subgroup` flag, the whole
add-user flow — so this is a UI to paste a server id into plus restarting the stream on a different
topic. Needed before §5 is genuinely useful.

---

## Known gaps, deliberately deferred

Documented where they live; listed here so they are not rediscovered as surprises.

- **A rejected update silences that peer permanently.** One peer's Loro operations form a chain, so
  refusing one leaves every later update from them unapplicable. Documented at length in
  `src-tauri/src/data/chat/mod.rs`. V1 accepts it; fixing it means recoverable rejection.
- **No removal or demotion.** The group CRDT has `Remove`/`Demote` and nothing issues them. A removed
  member can still write against a group head at which they had access. Needs a freshness rule — see
  `src-tauri/ref/README.md`.
- **No read control.** Anyone who knows the server id syncs everything. Needs `p2panda-spaces`.
- **Validation forks the document per remote update**, which loro documents as O(n). Fine at "fits in
  memory", not forever. TODO on `ChatDoc::preview`.
- **Message ordering is `(created_at, id)`** with author-supplied, untrusted timestamps. CHAT.md's
  causal ordering is the better answer; `reply_to` already records the edge, so switching later needs
  no data migration.

## Small things

- [ ] `Cargo.lock` is gitignored by the root `.gitignore`. For an application you usually want it
      committed, so builds are reproducible.
- [ ] `AuthGroup::my_id` is unused (the session tracks the device key itself) — the one remaining
      compiler warning. Delete it or use it.
- [ ] `src-tauri/ref/` is the CLI example this was built from. Gitignored, reference only.
