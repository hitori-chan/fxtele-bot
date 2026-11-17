//! Who may use the bot: the owner, `bot.toml` entries, and the state file.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankenstein::types::{Chat, ChatType, User};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::config::Config;

/// What the state file holds.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// People added from the menu.
    users: BTreeSet<i64>,
    /// Groups added from the menu or approved by the owner.
    chats: BTreeSet<i64>,
    /// Names last seen for allowed people and groups, including `bot.toml` entries.
    names: BTreeMap<i64, Name>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Name {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl Name {
    /// "First Last", or the first name alone.
    pub fn new(first: &str, last: Option<&str>, username: Option<String>) -> Self {
        let name = match last {
            Some(last) => format!("{first} {last}"),
            None => first.to_string(),
        };
        Self { name, username }
    }

    pub fn of_user(user: &User) -> Self {
        Self::new(&user.first_name, user.last_name.as_deref(), user.username.clone())
    }

    pub fn of_chat(chat: &Chat) -> Option<Self> {
        let name = chat.title.clone().or_else(|| chat.first_name.clone())?;
        Some(Self { name, username: chat.username.clone() })
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.username {
            Some(username) => write!(f, "{} (@{username})", self.name),
            None => f.write_str(&self.name),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    New,
    Already,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Removed {
    Done,
    /// Listed in `bot.toml`; only editing the file removes it.
    Permanent,
    NotFound,
}

/// One row of the owner's list.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub name: Option<Name>,
    pub permanent: bool,
}

pub struct Access {
    owner: i64,
    config_users: BTreeSet<i64>,
    config_chats: BTreeSet<i64>,
    state: State,
    path: PathBuf,
}

impl Access {
    /// Reads the state file, and writes it back once: one that can't be saved stops the bot here,
    /// rather than losing the menu's changes at the next restart.
    pub fn load(config: &Config) -> Result<Self> {
        let path = config.state_path.clone();
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("the state file {} is damaged; fix or delete it", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e).with_context(|| format!("can't read the state file {}", path.display())),
        };
        write_atomic(&path, &state).with_context(|| {
            format!("can't save the state file {}; create its folder or set state_path in bot.toml", path.display())
        })?;
        Ok(Self {
            owner: config.owner_id,
            config_users: config.allowed_users.iter().copied().collect(),
            config_chats: config.allowed_chats.iter().copied().collect(),
            state,
            path,
        })
    }

    pub const fn owner(&self) -> i64 {
        self.owner
    }

    pub fn is_owner(&self, user: &User) -> bool {
        i64::try_from(user.id).is_ok_and(|id| id == self.owner)
    }

    /// Whether the bot works in this chat.
    pub fn allows(&self, chat: &Chat) -> bool {
        match chat.type_field {
            ChatType::Private => self.allows_id(chat.id),
            ChatType::Group | ChatType::Supergroup => chat.id < 0 && self.allows_id(chat.id),
            ChatType::Channel => false,
        }
    }

    pub(crate) fn allows_id(&self, id: i64) -> bool {
        id == self.owner || self.is_permanent(id) || self.state.users.contains(&id) || self.state.chats.contains(&id)
    }

    /// Whether `id` is in `bot.toml`, so the menu can't remove it.
    pub(crate) fn is_permanent(&self, id: i64) -> bool {
        self.config_users.contains(&id) || self.config_chats.contains(&id)
    }

    /// Adds a person (positive ID) or a group (negative ID).
    pub fn add(&mut self, id: i64, name: Option<Name>) -> Added {
        let added = if id == self.owner || self.is_permanent(id) {
            false
        } else if id > 0 {
            self.state.users.insert(id)
        } else {
            self.state.chats.insert(id)
        };
        let renamed = id != self.owner && name.is_some_and(|name| self.set_name(id, name));
        if added || renamed {
            self.save();
        }
        if added { Added::New } else { Added::Already }
    }

    pub fn remove(&mut self, id: i64) -> Removed {
        if self.is_permanent(id) || id == self.owner {
            return Removed::Permanent;
        }
        let removed = if id > 0 { self.state.users.remove(&id) } else { self.state.chats.remove(&id) };
        if !removed {
            return Removed::NotFound;
        }
        self.state.names.remove(&id);
        self.save();
        Removed::Done
    }

    /// A group became a supergroup and got a new ID. Returns whether the approval moved;
    /// `false` with a `bot.toml` entry means the config needs the new ID.
    pub fn migrate(&mut self, from: i64, to: i64) -> bool {
        if !self.state.chats.remove(&from) {
            return false;
        }
        self.state.chats.insert(to);
        if let Some(name) = self.state.names.remove(&from) {
            self.state.names.insert(to, name);
        }
        self.save();
        true
    }

    /// Remembers the name of an allowed person or group; others are ignored.
    pub fn remember(&mut self, id: i64, name: Name) {
        if id != self.owner && self.allows_id(id) && self.set_name(id, name) {
            self.save();
        }
    }

    pub fn name(&self, id: i64) -> Option<&Name> {
        self.state.names.get(&id)
    }

    fn set_name(&mut self, id: i64, name: Name) -> bool {
        if self.state.names.get(&id) == Some(&name) {
            return false;
        }
        self.state.names.insert(id, name);
        true
    }

    /// Every allowed person, then every group, besides the owner.
    pub fn entries(&self) -> Vec<Entry> {
        let users = self.config_users.union(&self.state.users);
        let chats = self.config_chats.union(&self.state.chats);
        users
            .chain(chats)
            .filter(|&&id| id != self.owner)
            .map(|&id| Entry { id, name: self.state.names.get(&id).cloned(), permanent: self.is_permanent(id) })
            .collect()
    }

    fn save(&self) {
        if let Err(e) = write_atomic(&self.path, &self.state) {
            error!(path = %self.path.display(), "can't save the state file: {e:#}");
        }
    }
}

/// Writes a temp file next to the target, then renames it over the target.
fn write_atomic(path: &Path, state: &State) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn config(dir: &Path) -> Config {
        let mut config = Config::parse("owner_id = 1\nallowed_users = [10]\nallowed_chats = [-100]").unwrap();
        config.state_path = dir.join("state.json");
        config
    }

    pub fn chat(id: i64, kind: ChatType) -> Chat {
        Chat::builder().id(id).type_field(kind).build()
    }

    fn name(n: &str) -> Name {
        Name { name: n.into(), username: None }
    }

    #[test]
    fn allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let mut access = Access::load(&config(dir.path())).unwrap();
        assert!(access.allows(&chat(1, ChatType::Private)));
        assert!(access.allows(&chat(10, ChatType::Private)));
        assert!(!access.allows(&chat(11, ChatType::Private)));
        assert!(access.allows(&chat(-100, ChatType::Supergroup)));
        assert!(!access.allows(&chat(-101, ChatType::Group)));
        assert!(!access.allows(&chat(-100, ChatType::Channel)));

        assert_eq!(access.add(11, None), Added::New);
        assert_eq!(access.add(-101, Some(name("G"))), Added::New);
        assert!(access.allows(&chat(11, ChatType::Private)));
        assert!(access.allows(&chat(-101, ChatType::Group)));
        assert_eq!(access.add(11, None), Added::Already);
        assert_eq!(access.add(10, None), Added::Already);
    }

    #[test]
    fn state_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut access = Access::load(&config(dir.path())).unwrap();
        access.add(11, Some(Name { name: "Ann".into(), username: Some("ann".into()) }));
        access.add(-101, None);
        access.remember(-100, name("Config group"));
        access.remember(-999, name("Stranger group"));

        let reloaded = Access::load(&config(dir.path())).unwrap();
        assert_eq!(reloaded.state, access.state);
        assert!(reloaded.state.names.contains_key(&-100));
        assert!(!reloaded.state.names.contains_key(&-999));
        assert!(!dir.path().join("state.json.tmp").exists());
    }

    #[test]
    fn damaged_state_file_stops_startup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), "{not json").unwrap();
        let err = Access::load(&config(dir.path())).err().unwrap();
        assert!(format!("{err:#}").contains("damaged"));
    }

    #[test]
    fn removing() {
        let dir = tempfile::tempdir().unwrap();
        let mut access = Access::load(&config(dir.path())).unwrap();
        access.add(-101, Some(name("G")));
        assert_eq!(access.remove(-101), Removed::Done);
        assert_eq!(access.name(-101), None);
        assert_eq!(access.remove(-101), Removed::NotFound);
        assert_eq!(access.remove(-100), Removed::Permanent);
        assert_eq!(access.remove(10), Removed::Permanent);
        assert_eq!(access.remove(1), Removed::Permanent);
    }

    #[test]
    fn migration_moves_approval_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut access = Access::load(&config(dir.path())).unwrap();
        access.add(-101, Some(name("G")));
        assert!(access.migrate(-101, -1_000_000_000_101));
        assert!(access.allows(&chat(-1_000_000_000_101, ChatType::Supergroup)));
        assert!(!access.allows(&chat(-101, ChatType::Group)));
        assert_eq!(access.name(-1_000_000_000_101), Some(&name("G")));
        // bot.toml entries don't move.
        assert!(!access.migrate(-100, -1_000_000_000_100));
        assert!(access.is_permanent(-100));
    }

    #[test]
    fn entries_list_people_then_groups() {
        let dir = tempfile::tempdir().unwrap();
        let mut access = Access::load(&config(dir.path())).unwrap();
        access.add(-101, Some(name("G")));
        access.add(5, None);
        let ids: Vec<(i64, bool)> = access.entries().iter().map(|e| (e.id, e.permanent)).collect();
        assert_eq!(ids, [(5, false), (10, true), (-101, false), (-100, true)]);
    }
}
