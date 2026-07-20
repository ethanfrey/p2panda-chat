// SPDX-License-Identifier: MIT OR Apache-2.0

//! Everything that has to survive a restart: the identity, the log database, and the list id.
//!
//! p2panda's defaults are *ephemeral* — a fresh random `SigningKey` and an in-memory SQLite
//! database on every run — so persistence is entirely opt-in, and it is two builder calls:
//!
//! ```ignore
//! p2panda::builder()
//!     .signing_key(key)                   // else: a brand new network identity every run
//!     .database_url(&url)                 // else: SQLite `:memory:`, wiped on exit
//!     .spawn()
//! ```
//!
//! This module is the "and then you have to write the rest yourself" part. There are three pieces
//! of state, and p2panda owns exactly one of them.
//!
//! ## 1. The identity — ours to keep (`SigningKey`, 32 bytes)
//!
//! A p2panda identity is a single ed25519 keypair, and it is three things at once: the node id on
//! the network, the author id on every operation you sign, and the proof that your append-only log
//! is yours. Generating a key *is* creating a user; there is no account, no registration, no server.
//!
//! p2panda 0.7 does not persist it for you. `SigningKey` deliberately has no `Serialize` and no
//! `FromStr` — the private half is not meant to be lying around in a config file — so the only
//! serialisation contract is the raw 32 bytes, via `as_bytes()` and `TryFrom<&[u8]>`. Key custody
//! is the application's problem, and this module offers the two answers that matter:
//!
//! * [`KeyStorage::Keyring`] (the default) — the OS credential store: Secret Service on Linux,
//!   Keychain on macOS, Credential Manager on Windows. The secret never touches our directory.
//! * [`KeyStorage::File`] — 32 raw bytes in `key.bin`, `chmod 0600`, sitting next to the database.
//!   Simple, greppable, portable, and readable by anything that can read your home directory.
//!
//! ## 2. The log database — p2panda's, but only if you give it a path
//!
//! One SQLite file holds the append-only operation logs (with their sequence numbers, backlinks and
//! signatures), the topic→log map, the per-topic stream cursors, the causal-ordering buffers, and
//! the address book of peers we have met. `database_url` takes a SQLite URI, and the store creates
//! the file and runs its migrations itself.
//!
//! ## 3. The list id — nobody's, so also ours
//!
//! **p2panda does not remember which topics you subscribed to.** A `Topic` is just 32 bytes you
//! hand to `node.stream()`; the stack will happily sync it, but on restart it has no list of them
//! to show you. So we write the current list id to a file. A real app keeps a `topics` table
//! instead and shows you a landing page of documents; this example keeps one list per profile.
//!
//! ## The dangerous failure mode: a key without its log
//!
//! **The key and the database are one atomic unit. Keep them together, back them up together,
//! delete them together.**
//!
//! Your next operation's `seq_num` and `backlink` are derived *entirely from local storage*: an
//! empty store means "start a new log at seq 0". So if you keep the key but lose the database, your
//! next publish is a *second* genesis operation, validly signed by a key that already has a
//! different genesis out in the network. That is a fork of your own append-only log. Every peer who
//! still holds the old one will reject everything you publish from then on (they expect seq 42, you
//! sent seq 0), and no amount of re-syncing repairs it. The asymmetry is worth internalising:
//!
//! * **Lose the key, keep the store** → your old log stays valid and replicated, you just can't
//!   append to it. You become a new author. Annoying, recoverable.
//! * **Keep the key, lose the store** → you corrupt your own log and the network permanently
//!   rejects you. Much worse.
//!
//! The keyring makes that second case *easy* to reach by accident, because the key outlives
//! `rm -rf ~/.p2panda-examples`. [`Profile::check_key_without_log`] exists to shout about it.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use p2panda_core::{SigningKey, Topic};

use crate::Result;

/// All state written by any of these examples lives under `~/.p2panda-examples/`.
const ROOT_DIR: &str = ".p2panda-examples";

/// ... and this crate's under `~/.p2panda-examples/todo-auth/<profile>/`.
const CRATE_DIR: &str = "todo-auth";

/// What the OS credential store files our keys under. The profile name is the "username".
const KEYRING_SERVICE: &str = "p2panda-examples.todo-auth";

const DATABASE_FILE: &str = "node.sqlite";
const KEY_FILE: &str = "key.bin";
const LIST_ID_FILE: &str = "list-id";

/// Where the private key lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStorage {
    /// The OS credential store (Secret Service / Keychain / Credential Manager).
    Keyring,
    /// A `chmod 0600` file next to the database.
    File,
}

/// Whether we found an existing identity or minted one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOrigin {
    Created,
    Loaded,
}

/// A named directory holding one node's identity, log database and current list id.
///
/// One profile is one p2panda identity is one database. Running two profiles side by side is how
/// you get two peers on one machine — and it is why they must never share a directory or a key: two
/// nodes writing one log with one key is the fork scenario above, continuously.
#[derive(Debug, Clone)]
pub struct Profile {
    name: String,
    dir: PathBuf,
}

impl Profile {
    /// Open (creating if needed) `~/.p2panda-examples/todo-auth/<name>/`.
    pub fn open(name: &str) -> Result<Self> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err(format!(
                "invalid profile name {name:?}: use letters, digits, '-' and '_' only"
            )
            .into());
        }

        let Some(home) = home_dir() else {
            return Err("could not determine the home directory".into());
        };

        let dir = home.join(ROOT_DIR).join(CRATE_DIR).join(name);
        fs::create_dir_all(&dir)?;
        restrict_permissions(&dir, 0o700)?;

        Ok(Self {
            name: name.to_string(),
            dir,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn database_path(&self) -> PathBuf {
        self.dir.join(DATABASE_FILE)
    }

    /// The SQLite URI to hand to `NodeBuilder::database_url`. The store creates the file and runs
    /// its own migrations; there is no provisioning step.
    pub fn database_url(&self) -> String {
        format!("sqlite://{}", self.database_path().display())
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.join(KEY_FILE)
    }

    /// Get the identity for this profile, creating one on first run.
    ///
    /// Blocking: the keyring talks to the OS credential store over D-Bus on Linux, so callers
    /// inside an async runtime should put this on `tokio::task::spawn_blocking`.
    pub fn load_or_create_key(&self, storage: KeyStorage) -> Result<(SigningKey, KeyOrigin)> {
        match storage {
            KeyStorage::Keyring => self.load_or_create_key_in_keyring(),
            KeyStorage::File => self.load_or_create_key_in_file(),
        }
    }

    fn load_or_create_key_in_keyring(&self) -> Result<(SigningKey, KeyOrigin)> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &self.name).map_err(keyring_error)?;

        match entry.get_secret() {
            Ok(bytes) => Ok((SigningKey::try_from(&bytes[..])?, KeyOrigin::Loaded)),
            Err(keyring::Error::NoEntry) => {
                let key = SigningKey::generate();
                entry.set_secret(key.as_bytes()).map_err(keyring_error)?;
                Ok((key, KeyOrigin::Created))
            }
            Err(err) => Err(keyring_error(err)),
        }
    }

    fn load_or_create_key_in_file(&self) -> Result<(SigningKey, KeyOrigin)> {
        let path = self.key_path();

        match fs::read(&path) {
            Ok(bytes) => Ok((SigningKey::try_from(&bytes[..])?, KeyOrigin::Loaded)),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let key = SigningKey::generate();
                fs::write(&path, key.as_bytes())?;
                restrict_permissions(&path, 0o600)?;
                Ok((key, KeyOrigin::Created))
            }
            Err(err) => Err(err.into()),
        }
    }

    /// The list this profile was last using, if any. p2panda does not remember topics for us.
    pub fn load_list_id(&self) -> Result<Option<Topic>> {
        match fs::read_to_string(self.dir.join(LIST_ID_FILE)) {
            Ok(contents) => Ok(Some(Topic::from_str(contents.trim())?)),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn store_list_id(&self, id: Topic) -> Result<()> {
        fs::write(self.dir.join(LIST_ID_FILE), format!("{id}\n"))?;
        Ok(())
    }

    /// Warn if we loaded an identity whose log database is gone.
    ///
    /// Publishing now would append a second genesis operation to a log the network already knows
    /// under a different genesis — see the module docs. The keyring makes this easy to hit by
    /// accident, since the key survives `rm -rf` of this directory.
    ///
    /// The `list-id` check is what keeps `todo-auth setup` from tripping this: setup mints an
    /// identity and nothing else, so a key with no database and no list is a profile that has never
    /// joined anything, not a profile that lost its log.
    pub fn check_key_without_log(&self, origin: KeyOrigin, storage: KeyStorage) -> Option<String> {
        let joined_before = self.dir.join(LIST_ID_FILE).exists();

        if origin == KeyOrigin::Loaded && joined_before && !self.database_path().exists() {
            let key_location = match storage {
                KeyStorage::Keyring => format!("the OS keyring ({KEYRING_SERVICE}/{})", self.name),
                KeyStorage::File => self.key_path().display().to_string(),
            };
            return Some(format!(
                "identity for profile '{}' was restored from {key_location}, but its log database \
                 is missing.\n  Anything you publish now starts a *second* log under the same key, \
                 which peers holding the old one will reject forever.\n  Safe options: restore \
                 {}, or run under a fresh --name.",
                self.name,
                self.database_path().display(),
            ));
        }
        None
    }
}

/// `keyring::Error` is not `Send + Sync`, so it cannot cross into our boxed error type as-is.
///
/// Failures here are mostly environmental — a headless box with no Secret Service running, a locked
/// keyring — so point at the escape hatch while we are at it.
fn keyring_error(err: keyring::Error) -> Box<dyn std::error::Error + Send + Sync> {
    format!("OS keyring unavailable ({err}); re-run with --key-file to use a key file instead")
        .into()
}

fn home_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir()
}

#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}
