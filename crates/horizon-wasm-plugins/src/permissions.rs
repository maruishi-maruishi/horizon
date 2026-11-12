//! The server's permissions, from `plugins/permissions.toml` (reread when it changes):
//!
//! ```toml
//! [groups.default]            # every player is in it
//! permissions = ["homes.use"]
//!
//! [groups.admin]
//! inherits = ["default"]
//! permissions = ["*"]
//!
//! [players.Steve]             # a name, or a UUID
//! groups = ["admin"]
//! permissions = ["-homes.limit.bypass"]
//! ```
//!
//! A node is granted by itself, by `a.b.*` for any node under `a.b`, or by `*`; a `-` in front
//! denies it. The player's own nodes come first, then their groups in order (each before the
//! groups it inherits), then `default`. The first match decides; with none, the game's own rule
//! does (the command's operator level).
use crate::conv::{parse_uuid, uuid_string};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    groups: BTreeMap<String, Group>,
    #[serde(default)]
    players: BTreeMap<String, Player>,
}

#[derive(Debug, Default, Deserialize)]
struct Group {
    #[serde(default)]
    inherits: Vec<String>,
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Player {
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Permissions {
    path: PathBuf,
    modified: Option<SystemTime>,
    file: File,
}

/// Whether `pattern` (without its `-`) covers `node`.
fn covers(pattern: &str, node: &str) -> bool {
    pattern == "*"
        || pattern == node
        || pattern
            .strip_suffix(".*")
            .is_some_and(|base| node.starts_with(base) && node[base.len()..].starts_with('.'))
}

impl Permissions {
    pub fn load(path: &Path) -> Self {
        let mut p = Self {
            path: path.to_owned(),
            ..Self::default()
        };
        p.reload_if_changed();
        p
    }

    /// Reread the file if it changed; whether it did.
    pub fn reload_if_changed(&mut self) -> bool {
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if modified == self.modified {
            return false;
        }
        self.modified = modified;
        self.file = match std::fs::read_to_string(&self.path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(file) => {
                    log::info!("plugins: read permissions from {}", self.path.display());
                    file
                }
                Err(e) => {
                    log::error!(
                        "plugins: {}: {e} (keeping no permissions)",
                        self.path.display()
                    );
                    File::default()
                }
            },
            Err(_) => File::default(),
        };
        true
    }

    #[cfg(test)]
    fn from_text(text: &str) -> Self {
        Self {
            file: toml::from_str(text).unwrap(),
            ..Self::default()
        }
    }

    fn player(&self, uuid: u128, name: Option<&str>) -> Option<&Player> {
        let id = uuid_string(uuid);
        self.file.players.iter().find_map(|(key, p)| {
            let by_uuid =
                parse_uuid(key).is_some_and(|k| k == uuid) || key.eq_ignore_ascii_case(&id);
            let by_name = name.is_some_and(|n| key.eq_ignore_ascii_case(n));
            (by_uuid || by_name).then_some(p)
        })
    }

    /// The player's groups in priority order, inherited ones after their child, `default` last.
    pub fn groups_of(&self, uuid: u128, name: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack: Vec<String> = self
            .player(uuid, name)
            .map(|p| p.groups.clone())
            .unwrap_or_default();
        stack.reverse();
        while let Some(g) = stack.pop() {
            if out.contains(&g) {
                continue;
            }
            if let Some(group) = self.file.groups.get(&g) {
                for parent in group.inherits.iter().rev() {
                    stack.push(parent.clone());
                }
            }
            out.push(g);
        }
        if !out.iter().any(|g| g == "default") && self.file.groups.contains_key("default") {
            out.push("default".into());
        }
        out
    }

    /// Granted, denied, or not decided here.
    pub fn check(&self, uuid: u128, name: Option<&str>, node: &str) -> Option<bool> {
        let decide = |nodes: &[String]| {
            nodes.iter().find_map(|n| match n.strip_prefix('-') {
                Some(denied) if covers(denied, node) => Some(false),
                None if covers(n, node) => Some(true),
                _ => None,
            })
        };
        if let Some(p) = self.player(uuid, name)
            && let Some(d) = decide(&p.permissions)
        {
            return Some(d);
        }
        for group in self.groups_of(uuid, name) {
            if let Some(g) = self.file.groups.get(&group)
                && let Some(d) = decide(&g.permissions)
            {
                return Some(d);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEVE: u128 = 0x1111_2222_3333_4444_5555_6666_7777_8888;

    #[test]
    fn players_groups_wildcards_and_denials() {
        let p = Permissions::from_text(
            r#"
            [groups.default]
            permissions = ["homes.use", "-homes.admin"]
            [groups.mod]
            inherits = ["default"]
            permissions = ["kick", "homes.*"]
            [players.Steve]
            groups = ["mod"]
            permissions = ["-homes.limit.bypass"]
            [players."2222-x"]
            groups = []
            "#,
        );
        assert_eq!(p.groups_of(STEVE, Some("steve")), ["mod", "default"]);
        assert_eq!(p.check(STEVE, Some("Steve"), "kick"), Some(true));
        assert_eq!(p.check(STEVE, Some("Steve"), "homes.admin"), Some(true)); // mod's homes.* first
        assert_eq!(
            p.check(STEVE, Some("Steve"), "homes.limit.bypass"),
            Some(false)
        );
        assert_eq!(p.check(STEVE, Some("Steve"), "ban"), None);
        // Someone else: only default.
        assert_eq!(p.check(7, Some("Alex"), "homes.use"), Some(true));
        assert_eq!(p.check(7, Some("Alex"), "homes.admin"), Some(false));
        assert_eq!(p.check(7, Some("Alex"), "kick"), None);
        assert!(!covers("homes.*", "homesick"));
    }

    #[test]
    fn players_by_uuid() {
        let p = Permissions::from_text(&format!(
            "[groups.admin]\npermissions = [\"*\"]\n[players.\"{}\"]\ngroups = [\"admin\"]\n",
            uuid_string(STEVE)
        ));
        assert_eq!(p.check(STEVE, None, "anything.at.all"), Some(true));
        assert_eq!(p.check(STEVE + 1, None, "anything"), None);
    }
}
