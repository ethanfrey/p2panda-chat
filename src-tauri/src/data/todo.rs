// SPDX-License-Identifier: MIT OR Apache-2.0

//! The todo list data type: one Loro document, and nothing else.
//!
//! **This is `todo-loro`'s `src/todo.rs`, near enough unchanged** — read that crate first if you
//! want the argument for why the application data model should be an off-the-shelf CRDT and why
//! p2panda needs so little glue to carry it. The short version: the root `LoroMap` `"items"` maps
//! item id → `LoroText` description, so concurrent edits to one description merge
//! character-by-character; there are no timestamps and no hand-rolled conflict resolution.
//!
//! The one change for `todo-auth`: [`TodoList::export`] hands back plain bytes rather than a
//! ready-made wire message, because here an update never travels alone — `main` pairs it with the
//! group operations it depends on (see [`crate::message`]).
//!
//! Note what is *absent*: the CRDT knows nothing about access control. It merges whatever it is
//! given. Authorization happens strictly before `import` is called — an unauthorized update is
//! never handed to Loro, because once it is merged there is no such thing as un-merging it.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use loro::event::Diff;
use loro::{ExportMode, Index, LoroDoc, LoroText, UpdateOptions, ValueOrContainer, VersionVector};
use p2panda_core::{Hash, Topic};

use crate::Result;

pub type TodoItemId = Hash;

pub type TodoListId = Topic;

/// Name of the root `LoroMap` holding every todo item, keyed by item id.
const ITEMS: &str = "items";

/// Todo list state, backed entirely by a single Loro document.
pub struct TodoList {
    id: TodoListId,
    doc: LoroDoc,
    /// Everything already broadcast by us or imported from someone else. Anything beyond this
    /// version is a local change nobody else has seen yet.
    shared: VersionVector,
    /// Keeps the change subscription alive; dropping it would silence the notifications.
    _subscription: loro::Subscription,
}

impl TodoList {
    pub fn from_id(id: TodoListId) -> Self {
        let doc = LoroDoc::new();

        // Fires for local commits and remote imports alike, so one code path reports our edits,
        // everyone else's, and the replay at startup.
        let subscription = doc.subscribe_root(Arc::new(|event| {
            // Creating an item is two diffs in one commit (the map gains a key, the new text gains
            // its content), so collect the created ids first and skip their text diffs, or every
            // "created" would be followed by a spurious "updated".
            let mut created = HashSet::new();

            for container in &event.events {
                if let Diff::Map(delta) = &container.diff {
                    for (id, value) in &delta.updated {
                        match value {
                            Some(_) => {
                                created.insert(id.to_string());
                                println!("➭ created todo item with id {id}");
                            }
                            None => println!("➭ deleted todo item with id {id}"),
                        }
                    }
                }
            }

            for container in &event.events {
                if let Diff::Text(_) = &container.diff
                    && let Some((_, Index::Key(id))) = container.path.last()
                    && !created.contains(&id.to_string())
                {
                    println!("➭ updated todo item with id {id}");
                }
            }
        }));

        Self {
            id,
            doc,
            shared: VersionVector::default(),
            _subscription: subscription,
        }
    }

    pub fn id(&self) -> TodoListId {
        self.id
    }

    fn text(&self, id: TodoItemId) -> Option<LoroText> {
        match self.doc.get_map(ITEMS).get(&id.to_hex()) {
            Some(ValueOrContainer::Container(container)) => container.into_text().ok(),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.doc.get_map(ITEMS).is_empty()
    }

    /// All items as (id, description) pairs, sorted by id so the display is stable.
    pub fn items(&self) -> Vec<(String, String)> {
        let items = self.doc.get_map(ITEMS);
        let mut result: Vec<_> = items
            .keys()
            .filter_map(|key| {
                let key = key.to_string();
                match items.get(&key) {
                    Some(ValueOrContainer::Container(container)) => {
                        let text = container.into_text().ok()?;
                        Some((key, text.to_string()))
                    }
                    _ => None,
                }
            })
            .collect();
        result.sort();
        result
    }

    pub fn find_item_id(&self, prefix: &str) -> Option<TodoItemId> {
        self.doc
            .get_map(ITEMS)
            .keys()
            .map(|key| key.to_string())
            .find(|key| key.starts_with(prefix))
            .and_then(|key| Hash::from_str(&key).ok())
    }

    pub fn create(&self, description: &str) -> Result<()> {
        let id: TodoItemId = Topic::random().into();
        let text = self
            .doc
            .get_map(ITEMS)
            .insert_container(&id.to_hex(), LoroText::new())?;
        text.insert(0, description)?;
        Ok(())
    }

    /// Update an item's description. `LoroText::update` applies the minimal set of inserts and
    /// deletes, which is what makes concurrent edits to one description merge rather than clobber.
    pub fn update(&self, id: TodoItemId, description: &str) -> Result<()> {
        let Some(text) = self.text(id) else {
            return Err(format!("unknown item with id {id}").into());
        };
        text.update(description, UpdateOptions::default())
            .map_err(|err| format!("could not update item: {err}"))?;
        Ok(())
    }

    pub fn delete(&self, id: TodoItemId) -> Result<()> {
        if self.text(id).is_none() {
            return Err(format!("unknown item with id {id}").into());
        }
        self.doc.get_map(ITEMS).delete(&id.to_hex())?;
        Ok(())
    }

    /// Commit local changes and export the update bytes nobody else has seen yet, or `None` when
    /// there is nothing new to send.
    pub fn export(&mut self) -> Result<Option<Vec<u8>>> {
        self.doc.commit();

        let version = self.doc.oplog_vv();
        if version == self.shared {
            return Ok(None);
        }

        let bytes = self.doc.export(ExportMode::updates(&self.shared))?;
        self.shared = version;

        Ok(Some(bytes))
    }

    /// Merge an update. Idempotent, so replayed and duplicated deliveries are harmless.
    ///
    /// The caller must have authorized it first — see `apply_todo` in `main.rs`.
    ///
    /// `import` implicitly commits our own pending changes, so `oplog_vv()` afterwards can include
    /// a local edit we have not published yet; taking it wholesale as `shared` would mean that edit
    /// is never sent. Keeping our own peer's counter where it was avoids that.
    pub fn import(&mut self, update: &[u8]) -> Result<()> {
        let peer = self.doc.peer_id();
        let published = self.shared.get(&peer).copied().unwrap_or(0);

        self.doc.import(update)?;

        let mut version = self.doc.oplog_vv();
        version.insert(peer, published);
        self.shared = version;

        Ok(())
    }

    pub fn show(&self) {
        println!("⎯⎯⎯⎯⎯");
        println!("TODO LIST: {}", self.id());

        if self.is_empty() {
            println!(".. no items yet ..");
        } else {
            println!("⎯⎯⎯⎯⎯");
            for (id, description) in self.items() {
                println!("◆ [{}]: {}", &id[0..4], description);
            }
        }

        println!("⎯⎯⎯⎯⎯");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_list() -> TodoList {
        TodoList::from_id(TodoListId::random())
    }

    fn first_id(list: &TodoList) -> TodoItemId {
        Hash::from_str(&list.items()[0].0).expect("valid item id")
    }

    /// The merge semantics are `todo-loro`'s and are tested exhaustively there. These two cover the
    /// properties `todo-auth` leans on hardest.
    #[test]
    fn concurrent_edits_to_one_description_merge() -> Result<()> {
        let mut alice = new_list();
        let mut bobby = TodoList::from_id(alice.id());

        alice.create("Buy milk")?;
        bobby.import(&alice.export()?.expect("alice has changes"))?;

        let id = first_id(&alice);
        alice.update(id, "Buy oat milk")?;
        bobby.update(id, "Buy milk today")?;

        let from_alice = alice.export()?.expect("alice has changes");
        let from_bobby = bobby.export()?.expect("bobby has changes");
        alice.import(&from_bobby)?;
        bobby.import(&from_alice)?;

        assert_eq!(alice.items(), bobby.items());
        let merged = alice.items()[0].1.clone();
        assert!(
            merged.contains("oat") && merged.contains("today"),
            "an edit was dropped: {merged}"
        );
        Ok(())
    }

    /// Re-importing is a no-op. `todo-auth` leans on this the way `todo-loro` does — the log is
    /// replayed in full on every restart — and additionally because an update can wait in the
    /// ordering buffer while the same update arrives again from another peer.
    #[test]
    fn reimporting_the_same_update_is_idempotent() -> Result<()> {
        let mut alice = new_list();
        let mut bobby = TodoList::from_id(alice.id());

        alice.create("Buy milk")?;
        let update = alice.export()?.expect("alice has changes");

        bobby.import(&update)?;
        bobby.import(&update)?;

        assert_eq!(bobby.items().len(), 1);
        assert_eq!(alice.items(), bobby.items());
        Ok(())
    }
}
