//! An example plugin: a welcome line, homes, spawn protection and timed announcements.
//!
//! Build: `cargo build --release --target wasm32-unknown-unknown`, then copy
//! `target/wasm32-unknown-unknown/release/starter.wasm` into the server's `plugins/`.
use horizon_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const DEFAULT_CONFIG: &str = r#"# starter plugin settings. Saved changes apply after /plugins reload starter.
welcome = "§eようこそ、{name}さん！"
announcements = ["§b/sethome でホームを保存、/home で戻れます"]
announce_every_seconds = 300
# Blocks within this many blocks of spawn can only be broken with the permission starter.build.
spawn = [0, 0]
spawn_protection_radius = 16
max_homes = 5
"#;

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    welcome: String,
    #[serde(default)]
    announcements: Vec<String>,
    #[serde(default = "five_minutes")]
    announce_every_seconds: u64,
    #[serde(default)]
    spawn: [i32; 2],
    #[serde(default)]
    spawn_protection_radius: i32,
    #[serde(default = "five")]
    max_homes: usize,
}

fn five_minutes() -> u64 {
    300
}
fn five() -> usize {
    5
}

impl Default for Config {
    fn default() -> Self {
        Self {
            welcome: String::new(),
            announcements: Vec::new(),
            announce_every_seconds: 300,
            spawn: [0, 0],
            spawn_protection_radius: 0,
            max_homes: 5,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Home {
    dimension: String,
    position: [f64; 3],
    yaw: f32,
    pitch: f32,
}

/// Homes by player UUID, then by name.
type Homes = BTreeMap<String, BTreeMap<String, Home>>;

const ANNOUNCE: u64 = 1;

#[derive(Default)]
struct Starter {
    config: Config,
    homes: Homes,
    next_announcement: usize,
}

impl Starter {
    fn load(&mut self) {
        host::save_default_config(DEFAULT_CONFIG);
        self.config = serde_json::from_value(host::config()).unwrap_or_default();
        self.homes = host::data_read("homes.json")
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        host::cancel_task(ANNOUNCE);
        if !self.config.announcements.is_empty() {
            let ticks = self.config.announce_every_seconds.max(10) * 20;
            host::schedule(ticks, ANNOUNCE, ticks);
        }
        host::log(&format!("ready: {} players have homes", self.homes.len()));
    }

    fn save_homes(&self) {
        if let Err(e) = host::data_write(
            "homes.json",
            &serde_json::to_string_pretty(&self.homes).unwrap(),
        ) {
            host::error(&format!("saving homes: {e}"));
        }
    }

    fn command(&mut self, command: Command) -> Reply {
        let Some(player) = command.player else {
            return Reply::message("Only players have homes.");
        };
        let name = command
            .args
            .first()
            .cloned()
            .unwrap_or_else(|| "home".into());
        let homes = self.homes.entry(player.uuid.clone()).or_default();
        match command.label.as_str() {
            "sethome" => {
                if !homes.contains_key(&name) && homes.len() >= self.config.max_homes {
                    return Reply::message(&format!(
                        "§cホームは {} 個までです。",
                        self.config.max_homes
                    ));
                }
                homes.insert(
                    name.clone(),
                    Home {
                        dimension: player.dimension,
                        position: player.position,
                        yaw: player.yaw,
                        pitch: player.pitch,
                    },
                );
                self.save_homes();
                Reply::message(&format!("§aホーム「{name}」を保存しました。"))
            }
            "home" => match homes.get(&name) {
                Some(h) => {
                    if host::teleport(&player.uuid, &h.dimension, h.position, h.yaw, h.pitch) {
                        Reply::message(&format!("§aホーム「{name}」へ移動しました。"))
                    } else {
                        Reply::message("§c移動できませんでした。")
                    }
                }
                None => Reply::message(&format!(
                    "§cホーム「{name}」はありません。/homes で一覧を見られます。"
                )),
            },
            "delhome" => {
                if homes.remove(&name).is_some() {
                    self.save_homes();
                    Reply::message(&format!("§eホーム「{name}」を削除しました。"))
                } else {
                    Reply::message(&format!("§cホーム「{name}」はありません。"))
                }
            }
            "homes" => {
                if homes.is_empty() {
                    Reply::message("§7ホームはまだありません。/sethome で保存できます。")
                } else {
                    let names: Vec<_> = homes.keys().cloned().collect();
                    Reply::message(&format!("§eホーム: §f{}", names.join(", ")))
                }
            }
            _ => Reply::none(),
        }
    }

    fn protected(&self, pos: [i32; 3]) -> bool {
        let r = self.config.spawn_protection_radius;
        r > 0
            && (pos[0] - self.config.spawn[0]).abs() <= r
            && (pos[2] - self.config.spawn[1]).abs() <= r
    }
}

impl Plugin for Starter {
    fn info(&self) -> Info {
        Info::new("starter", "1.0.0")
            .on("enable")
            .on("scheduled")
            .on("player_logged_in")
            .on("destroy_block")
            .command_with("sethome [name:word]", 0, Some("starter.home"))
            .command_with("home [name:word]", 0, Some("starter.home"))
            .command_with("delhome <name:word>", 0, Some("starter.home"))
            .command_with("homes", 0, Some("starter.home"))
    }

    fn on_event(&mut self, event: &Event) -> Reply {
        match event.name.as_str() {
            "enable" => {
                self.load();
                Reply::none()
            }
            "scheduled" => {
                if !self.config.announcements.is_empty() {
                    let line = &self.config.announcements
                        [self.next_announcement % self.config.announcements.len()];
                    host::broadcast(line);
                    self.next_announcement += 1;
                }
                Reply::none()
            }
            "player_logged_in" => {
                if let (Some(uuid), Some(name)) = (event.str("player"), event.str("name"))
                    && !self.config.welcome.is_empty()
                {
                    host::tell(uuid, &self.config.welcome.replace("{name}", name));
                }
                Reply::none()
            }
            "destroy_block" => {
                let Some(pos) = event.pos("pos") else {
                    return Reply::none();
                };
                if !self.protected(pos) {
                    return Reply::none();
                }
                let player: Option<Player> =
                    host::call_as("acting_player", json!([])).ok().flatten();
                match player {
                    Some(p) if !host::has_permission(&p.uuid, "starter.build", 2) => {
                        host::action_bar(&p.uuid, "§cスポーン付近は保護されています");
                        Reply::cancel()
                    }
                    _ => Reply::none(),
                }
            }
            "command" => match event.command() {
                Some(c) => self.command(c),
                None => Reply::none(),
            },
            _ => Reply::none(),
        }
    }
}

register!(Starter::default());
