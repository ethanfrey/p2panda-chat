# todo-auth

A collaborative, peer-to-peer todo list where **not everyone may write to it**, and where a person
can use **several devices**. The list's creator is its admin; they add other people as readers or
writers, and each person manages their own set of devices. Every peer independently enforces those
rights on the operations it receives.

Start with [`todo-loro`](../todo-loro) — it explains the Loro data model, why p2panda needs so little
glue to carry a CRDT, and how identity and the operation log survive a restart. All of that is
inherited here and not repeated. **This crate is about five things:**

1. **Two-level groups** — a top group of *people*, and under each person a subgroup of *devices*
   ([`p2panda-auth`]).
2. **Multi-group sync and ordering** — the groups replicate over the same topic as the data, and
   every todo edit names the group operations it was written against, across all of them.
3. **Write control** — those names make the verdict deterministic: a device's rights are resolved
   transitively (person → device), at the operations the edit depends on, so every peer decides the
   same way.
4. **A list id that commits to its creator** — so there is exactly one valid genesis per list, with
   no "whoever I heard from first" ambiguity.
5. **What read control would take** — still a separate, larger job (`p2panda-spaces`).

## Major Caveat

The ordering shows that a write happens *after* a group action (adding a user) to guarantee all nodes process the auth check
with the same validity regardless of sync ordering. It even evaluates the historical group state at that point in time -
see [`AuthGroup::access_at` in auth.rs](./src/auth.rs#L358-375). This is a best effort to handle changing states and does allow
deterministic evaluation of authorization across all nodes (independent of the sync order of auth vs todo messages).

However, actually handling removals properly and the race conditions (two admins remove each other on concurrent "forks") is a big area of research.
If admin A removes grants write access to B with op O and later removes it with op O', B can continue to post changes all referencing O as a dependency.
To properly handle this there needs to be some way to set an epoch or checkpoint or similar saying "after this point" (which is very hard to define in distributed systems),
only messages that references a group dependency "after O'" (defined as O' or any operation with a dependency chain back to O') will be processed and all others will be rejected (even ones "concurrent" with O').

We avoid any such checks here to avoid getting way into the DAG / CRDT auth weeds, which means demotion/removal is not strongly enforced here (a malicious client can always reference a group state before they were removed). To dig into this point, please [participate in this discussion](https://github.com/ethanfrey/p2panda-examples/issues/1).

## Minor Caveat

This is just write control enforced by the operation processing. If you want to handle read control, p2panda-spaces and encrypted data would be needed, but that is
a much larger project and currently not integrated into the high level p2panda Node API. For current state there, please follow the [p2panda-spaces related issues on GitHub](https://github.com/p2panda/p2panda/issues?q=is%3Aissue%20state%3Aopen%20label%3Ap2panda-spaces)

## Run it

```bash
# 1. Alice creates the list. She becomes its admin, and gets her own device subgroup.
cargo run -p todo-auth -- --name alice
```

```text
★ todo list id: 7560c2e9…6b078090
★ my node id:   7be076ae…22ebad43
⚿ 7be076ae created their device subgroup
⚿ 7be076ae created the list
⚿ you now have manage access to this list
```

```bash
# 2. Bob's laptop creates an identity and stops. He cannot join yet.
cargo run -p todo-auth -- --name bob-laptop setup      # prints Bob's laptop node id

# 3. Bob sends Alice that node id, out of band. In Alice's prompt:
/add <bob-laptop-node-id> writer

# 4. Bob joins as a new user. He syncs the group, creates his own subgroup, and can write.
cargo run -p todo-auth -- --name bob-laptop join 7560c2e9…6b078090
```

```text
⚿ 7be076ae created the list
⚿ 7be076ae added a write user (subgroup 9f0a1b2c)
⚿ 4c1d… created their device subgroup
★ your access: write
```

Bob can now add his **own** other devices, without involving Alice:

```bash
# In Bob's laptop prompt (he manages his own subgroup):
/add-device <bob-phone-node-id> writer

# Bob's phone joins as an additional device of the same person:
cargo run -p todo-auth -- --name bob-phone join 7560c2e9…6b078090 --device
```

| command | who | does |
|---|---|---|
| `/create`, `/update`, `/delete` | any writer | change the list |
| `/add <node-id> reader\|writer` | the list admin | add a **person** to the list |
| `/add-device <node-id> reader\|writer\|manager` | a subgroup manager | add a **device** to their own subgroup |
| `/members` | anyone | print the people and, under each, their devices |
| `/show`, `/help`, `/quit` | | as in `todo-loro` |

Everything persists as in `todo-loro`: `--name bob-laptop` on its own resumes the list, the identity,
and the groups, with no arguments.

## 1. Two levels: people, and their devices

The earlier version of this crate had one flat group per list. Real local-first apps need two levels,
because a *person* is not a *device* — you have a laptop and a phone, and both should be able to edit
the list as you:

```text
  top group  (the list)
  ├── Individual(Alice's laptop)  @ Manage      ← the list admin (a device, see below)
  ├── Group(Alice's subgroup)     @ Write        ← Alice, the person
  │     ├── Individual(Alice laptop)  @ Manage    ← Alice's manager device
  │     └── Individual(Alice phone)   @ Write     ← a device Alice added herself
  └── Group(Bob's subgroup)       @ Read          ← Bob, a read-only person
        └── Individual(Bob laptop)   @ Manage
```

* The **top group** is the list. Its members are people (as subgroups). The admin adds and removes
  people — "top level stays as is": you add readers and writers.
* A **person subgroup** is one human's devices. One device manages it and adds the person's others.

A device's effective access to the list is its access **within its subgroup, capped by that
subgroup's access within the top group**. This is `p2panda-auth`'s nested-group rule — a deeper edge
takes the lesser access, and multiple paths combine by the greater. So a `Write` device in a `Write`
subgroup may write; a `Write` device in a `Read` subgroup may not (the cap wins). The
`a_reader_persons_devices_may_not_write_even_if_managed_as_writers` test, and its end-to-end twin,
pin exactly that.

### The one place reality bends the request: subgroups cannot be managers

`p2panda-auth` forbids giving a *group* member `Manage` access
(`GroupCrdtError::ManagerGroupsNotAllowed`, [issue #779] — concurrent cross-group removals aren't
resolved yet). So the **list admin cannot be "a person"; it must be a device**, held directly in the
top group as `Individual(..) @ Manage`. That is why the creator's *device* is a direct top-group
member *and* the manager of the creator's own subgroup. Within a subgroup `Manage` is unrestricted —
a person can absolutely make a second device a manager of their devices. This is the only structural
compromise the CRDT forces, and it is a sensible one: "who administers the list" is a more sensitive
capability than "who is a user", and pinning it to a specific device rather than a whole person is
arguably the safer default anyway.

## 2. Group ids are derived, and the list id commits to the creator

This is the resolution of the loose end the flat version left open. There, a group's id was its
creator's key and "the first `Create` wins" — two peers who both knew the topic could publish rival
genesis operations. Here every id is **derived**, and the list id itself commits to the creator:

| id | derived from | is |
|---|---|---|
| `person_group_id(d)` | device `d` | the subgroup of the person whose manager device is `d` |
| `top_group_id(d)`    | creator `d` | the list's top group |
| `list_topic(d)`      | creator `d` | **the list id (topic) itself** |

Each is `hash(label ‖ key)`, turned into a `VerifyingKey` (via `SigningKey::from_bytes`, since every
32-byte hash is a valid ed25519 seed) or a `Topic`. The corresponding secret is nobody's — a group id
identifies, it never signs.

Because the **list id is a hash of the creator's key**, `main` derives it from that key before it ever
spawns a node, and `AuthGroup::apply` accepts a top-group `Create` only when:

```rust
// src/auth.rs
if list_topic(&author) != self.topic { return Err("genesis does not match this list id") }
if !members_match(initial_members, &top_genesis_members(&author)) { return Err("non-canonical") }
```

So a given list id admits **exactly one** valid initial state — the canonical two-level genesis built
by whoever's key hashes to it. There is no race and nothing to disambiguate: a peer who tried to mint
a different genesis for your list id would have `list_topic(them) != your list id`, and be rejected.
(`a_genesis_is_only_valid_for_the_list_id_it_commits_to`.) Subgroups are pinned the same way, one
level down: a `Create` for `person_group_id(d)` is believed only from `d` itself, so nobody can plant
a subgroup under someone else's identity.

### Genesis is two operations, so both levels exist from the first commit

Founding a list publishes **two** `Create`s — the creator's own subgroup, then the top group
referencing it:

```text
Create person_group_id(alice) { Individual(alice) @ Manage }
Create top_group_id(alice)    { Individual(alice) @ Manage, Group(person_group_id(alice)) @ Write }
```

The list id `= list_topic(alice)` commits to this exact shape. So the hash uniquely defines one
initial two-level state — which is what makes "bind the topic to the creation" meaningful rather than
merely convenient.

## 3. Ordering, now across several groups

`Message::Todo` carries the group operations the edit was written against:

```rust
pub struct TodoMessage {
    /// Tips across *all* group DAGs the author had applied. Never empty.
    pub depends_on: Vec<Hash>,
    #[serde(with = "serde_bytes")]
    pub update: Vec<u8>,
}
```

With multiple groups this dependency does the same two jobs as before, but now spanning them:

- **It orders across groups.** p2panda orders within one author's log, never across authors or across
  groups. `src/ordering.rs` (unchanged from the flat version) holds an operation until everything it
  names has been applied. Two failure modes make this mandatory, not optional:
  - `GroupCrdt::process` **panics** if an operation's dependencies aren't applied yet — so group
    operations go through the orderer first. With several groups the rule is per-group: a group's
    `Create` must be applied before any operation on it.
  - `apply_action` **panics** if the *parent* group of an operation is absent. A crafted operation
    naming an unknown group, with lying dependencies, would otherwise take every node down — so
    before handing a non-`Create` to the CRDT, `AuthGroup::apply` checks the parent exists
    (`an_operation_on_an_unknown_group_is_refused_not_panicked`).
- **It fixes the evaluation point** for the write check (next section).

**Group operations depend narrowly; todo edits depend widely.** A group operation names only *its own
group's* tips (`AuthGroup::heads_for`), so one person's device changes stay causally independent of
another's — Bob adding his phone does not have to wait on, or order against, Carol's subgroup. A todo
edit, by contrast, names *all* group tips (`AuthGroup::heads`), because judging it needs to see both
the top group and the author's subgroup at once.

> **Why not p2panda's own orderer?** Same reason as the flat version: the high-level `Node`'s pipeline
> is `pub(crate)` and hard-codes `Ingest` + `LogPrune`, with a closed `Extensions` enum. Its `Causal`
> variant, which would carry dependencies, is `unimplemented!()`. So dependencies ride in our payload,
> and this module consumes them.

## 4. Write control: deterministic, and now transitive

When the orderer releases an edit, everything it names has been applied, so we resolve the author's
access **at those operations**:

```rust
// src/main.rs
if !self.group.may_write_at(author, &message.depends_on) {
    println!("✖ rejected an edit from {…}: not a writer as of the group operation it depends on");
    return;   // never handed to Loro
}
```

The subtle part is *how* `may_write_at` resolves a device through the two levels, deterministically.
`p2panda-auth`'s own transitive `members()` walks nested groups against **current** state
(`members_inner` reads `current_state()`), so it cannot answer a point-in-time question. But
`state_at(deps)` returns the **direct** membership of *every* group at that point — so `access_at`
walks that snapshot itself, top group down through the author's subgroup, applying the same min-cap
and max-combine rules `members_inner` uses:

```rust
// src/auth.rs — resolve_access, in essence
for (member, edge) in states[group].access_levels() {
    let effective = min(cap, edge);                 // capped by the path taken in
    match member {
        Individual(id) if id == device => best = max(best, effective),
        Group(subgroup)                => recurse(subgroup, cap = effective),
    }
}
```

Because this reads only the `state_at(deps)` snapshot, the verdict is a function of the edit and the
group DAGs alone:

- **Every peer reaches the same verdict**, whatever else it has since learned.
- **A restart reaches the same verdict**, because the log replays into the same snapshots.
- **The answer never changes retroactively** — an edit made before a device was reachable stays
  unauthorized, even after it becomes a writer (`access_is_evaluated_at_the_operation_the_edit…`).

**An edit that names no group operation is rejected**, always: there is no snapshot to judge it
against. And, as in the flat version, the local refusal you see when *you* can't write is only a
courtesy — the enforcement is on the **receiving** side, in every peer. The e2e suite proves it with
the hidden `--force-write` flag: a reader, and a writer-device-inside-a-reader-subgroup, and a
non-member each force an edit out, and the admin's node rejects every one.

## 5. What is *not* here

### Read control — still the next commit

Anyone who knows the 32-byte list id can still read the whole list: in p2panda the topic *is* the
capability, and `Read` versus `Pull` means nothing without encryption. Making it mean something needs
`p2panda-spaces` (encrypt payloads for members above `Pull`; enforcement is math, not policy), which
`p2panda::Node` has no integration for — the official bridge still carries `@TODO: persist groups
state` and isn't enabled. The full write-up of what that glue involves is unchanged; it remains a
crate's worth of load-bearing code and gets its own commit. The two-level group here is exactly the
membership a space would bind its encryption to, so this work is a prerequisite for it, not a detour.

### Member removal — also its own commit, and now with more edges

`Remove` and `Demote` exist in the CRDT and this crate still never issues them. With two levels the
interesting cases multiply, and each deserves deliberate treatment:

- Removing a **person** (a subgroup edge in the top group) versus removing a **device** (an individual
  in a subgroup) — and the freshness rule that stops a removed party from writing against an old group
  head at which they still had access.
- `StrongRemove` can retroactively invalidate operations (a manager removed concurrently with the
  actions they took). An edit depending on an invalidated group operation must flip from authorized to
  rejected — and its data is already merged into Loro.
- Removal still isn't revocation of *reading*: without `p2panda-spaces` rotating keys, a removed
  device keeps every byte it ever synced.

**TODO:** implement `Remove`/`Demote` across both levels, with a freshness requirement on `depends_on`
and a policy for edits whose dependency the resolver later invalidates.

## Tests

```bash
make test                                           # everything
cargo test -p todo-auth                             # groups, ordering, CLI, CRDT
python3 crates/todo-auth/tests/integration_test.py  # e2e (needs a debug build first)
```

The unit tests cover the two-level rules against the CRDT directly: the two-level genesis; a genesis
being valid only for the list id it commits to; a subgroup being unable to hold `Manage`; a writer
person's devices being able to write while a reader person's cannot however they are managed; access
evaluated at the depended-on operations and never changing retroactively; and an operation on an
unknown group being refused rather than panicking.

The e2e tests run real nodes over mDNS and cover what unit tests cannot: the full add-person → join →
create-subgroup → write flow; a user adding a **second device** that then writes; the list id being
derived (two runs agree on it); the min-cap rule across the wire; forced edits from a reader, a
capped device, and a non-member all being rejected on arrival; a writer being unable to add people;
and a restart replaying every group and the list.

## Layout

| file | |
|---|---|
| `src/auth.rs` | the two-level groups: derivation, genesis, the transitive point-in-time write check |
| `src/ordering.rs` | the cross-group dependency buffer |
| `src/message.rs` | what goes on the wire, and why the author is not in it |
| `src/todo.rs` | the Loro data type (`todo-loro`'s, near enough unchanged) |
| `src/persistence.rs` | identity + storage (`todo-loro`'s) |
| `src/cli.rs` | arguments and the prompt |
| `src/main.rs` | the event loop: receive → order → authorize → apply |

## Further reading

- [`p2panda-auth`] — the group CRDT: [`Access`], [`GroupAction`], `GroupCrdt`, `GroupMember`, `StrongRemove`
- [`p2panda-spaces`](https://docs.rs/p2panda-spaces) — encryption bound to an auth group; read control
- [`Node::stream_from`](https://docs.rs/p2panda/latest/p2panda/node/struct.Node.html#method.stream_from) — replay, and how the groups are rebuilt on restart
- [`todo-loro`](../todo-loro) — the CRDT, the persistence, and the identity story this crate builds on

[`p2panda-auth`]: https://docs.rs/p2panda-auth
[`Access`]: https://docs.rs/p2panda-auth/latest/p2panda_auth/struct.Access.html
[`GroupAction`]: https://docs.rs/p2panda-auth/latest/p2panda_auth/group/enum.GroupAction.html
[issue #779]: https://github.com/p2panda/p2panda/issues/779
