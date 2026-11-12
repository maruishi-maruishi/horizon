//! Write plugins for a Horizon server in Rust, compiled to WebAssembly.
//!
//! ```ignore
//! use horizon_plugin_sdk::prelude::*;
//!
//! #[derive(Default)]
//! struct Hello;
//!
//! impl Plugin for Hello {
//!     fn info(&self) -> Info {
//!         Info::new("hello", "1.0").on("player_logged_in").command("hello")
//!     }
//!     fn on_event(&mut self, event: &Event) -> Reply {
//!         match event.name.as_str() {
//!             "player_logged_in" => {
//!                 host::broadcast(&format!("§e{} joined!", event.str("name").unwrap_or("?")));
//!                 Reply::none()
//!             }
//!             "command" => Reply::message("§aHello!"),
//!             _ => Reply::none(),
//!         }
//!     }
//! }
//!
//! register!(Hello::default());
//! ```
//!
//! Build with `cargo build --release --target wasm32-unknown-unknown` (crate-type `cdylib`) and put
//! the `.wasm` into the server's `plugins/` folder: it loads at once, and again whenever the
//! file changes.
//!
//! Every hook of the game is an event named like the hook (`destroy_block`, `player_logged_in`,
//! `living_incoming_damage_v2`...): ask for it in [`Info::on`]; `host::call("hooks", ...)` lists
//! them with their fields. Every helper of the game is a [`host::call`] by its name
//! (`tell`, `teleport_player`, `set_block`...); the common ones have typed wrappers in [`host`].
//! Besides the hooks the host sends `enable`, `disable`, `command`, `scheduled` and
//! `plugin_message`.
use serde::{Deserialize, Serialize, de::DeserializeOwned};
pub use serde_json::{self, Value, json};

pub mod prelude {
    pub use crate::{Command, Event, Info, Player, Plugin, Reply, host, register};
    pub use serde_json::{self, Value, json};
}

/// What a plugin says about itself.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Info {
    pub name: String,
    pub version: String,
    pub events: Vec<String>,
    pub commands: Vec<CommandSpec>,
}

/// A command a plugin adds: `usage` is words, then `<name:kind>` arguments and `[name:kind]`
/// optional ones (kinds: `word`, `text` (the rest of the line), `integer`, `number`, `boolean`,
/// `player`, `item`, `block_pos`, `player_name`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct CommandSpec {
    pub usage: String,
    /// The operator level needed (0: everyone).
    pub level: u8,
    /// A permission node that decides instead of the level (`plugins/permissions.toml`).
    pub permission: Option<String>,
    /// Take over vanilla's command of this name.
    pub replace_vanilla: bool,
}

impl Info {
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            ..Self::default()
        }
    }
    /// Hear a hook or a plugin event.
    pub fn on(mut self, event: &str) -> Self {
        self.events.push(event.into());
        self
    }
    /// A command every player may run.
    pub fn command(mut self, usage: &str) -> Self {
        self.commands.push(CommandSpec {
            usage: usage.into(),
            ..CommandSpec::default()
        });
        self
    }
    /// A command for players with the permission `node` (or operators of `level`).
    pub fn command_with(mut self, usage: &str, level: u8, node: Option<&str>) -> Self {
        self.commands.push(CommandSpec {
            usage: usage.into(),
            level,
            permission: node.map(Into::into),
            replace_vanilla: false,
        });
        self
    }
}

/// An event from the server.
#[derive(Clone, Debug, Deserialize)]
pub struct Event {
    #[serde(rename = "event")]
    pub name: String,
    #[serde(default)]
    pub data: Value,
}

impl Event {
    /// Field `key` of the event's data.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        serde_json::from_value(self.data.get(key)?.clone()).ok()
    }
    pub fn str(&self, key: &str) -> Option<&str> {
        self.data.get(key)?.as_str()
    }
    pub fn int(&self, key: &str) -> Option<i64> {
        self.data.get(key)?.as_i64()
    }
    pub fn pos(&self, key: &str) -> Option<[i32; 3]> {
        self.get(key)
    }
    /// A `command` event's details.
    pub fn command(&self) -> Option<Command> {
        (self.name == "command")
            .then(|| self.get("player"))
            .map(|player| Command {
                player,
                label: self.str("label").unwrap_or_default().to_owned(),
                line: self.str("command").unwrap_or_default().to_owned(),
                args: self.get("args").unwrap_or_default(),
            })
    }
}

/// A command a player (or the console) ran.
#[derive(Clone, Debug)]
pub struct Command {
    /// None on the console.
    pub player: Option<Player>,
    pub label: String,
    pub line: String,
    pub args: Vec<String>,
}

/// A player on the server.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Player {
    pub uuid: String,
    pub name: String,
    /// The operator level, 0 to 4.
    #[serde(default)]
    pub permission: u8,
    #[serde(default)]
    pub dimension: String,
    #[serde(default)]
    pub position: [f64; 3],
    #[serde(default)]
    pub game_mode: i32,
    #[serde(default)]
    pub health: f32,
    #[serde(default)]
    pub yaw: f32,
    #[serde(default)]
    pub pitch: f32,
}

/// A plugin's answer to an event.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Reply {
    /// Stop what the hook is about to do (cancellable hooks).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cancel: bool,
    /// New values for the event's changeable fields.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub set: serde_json::Map<String, Value>,
    /// The replacement value (hooks that return one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// Lines for whoever ran a command (`§` colour codes allowed).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
}

impl Reply {
    pub fn none() -> Self {
        Self::default()
    }
    pub fn cancel() -> Self {
        Self {
            cancel: true,
            ..Self::default()
        }
    }
    pub fn message(line: &str) -> Self {
        Self::default().and_message(line)
    }
    pub fn and_message(mut self, line: &str) -> Self {
        self.messages.push(line.into());
        self
    }
    pub fn set(mut self, field: &str, value: impl Serialize) -> Self {
        self.set.insert(
            field.into(),
            serde_json::to_value(value).unwrap_or(Value::Null),
        );
        self
    }
    pub fn value(mut self, value: impl Serialize) -> Self {
        self.value = Some(serde_json::to_value(value).unwrap_or(Value::Null));
        self
    }
}

/// A plugin.
pub trait Plugin: Send {
    fn info(&self) -> Info;
    fn on_event(&mut self, event: &Event) -> Reply;
}

/// The game's helpers.
pub mod host {
    use super::*;

    /// Call a helper by name: the game's own (`tell`, `teleport_player`, `set_block`, ...; the
    /// `host_functions` call lists them) or the plugin host's (`schedule`, `config`, ...).
    pub fn call(function: &str, args: Value) -> Result<Value, String> {
        let request = serde_json::to_vec(&json!({ "fn": function, "args": args })).unwrap();
        let reply = crate::raw::call(&request);
        let reply: Value = serde_json::from_slice(&reply).map_err(|e| e.to_string())?;
        match reply.get("err") {
            Some(e) => Err(e.as_str().unwrap_or("error").to_owned()),
            None => Ok(reply.get("ok").cloned().unwrap_or(Value::Null)),
        }
    }

    /// [`call`] with its result read as `T`.
    pub fn call_as<T: DeserializeOwned>(function: &str, args: Value) -> Result<T, String> {
        serde_json::from_value(call(function, args)?).map_err(|e| e.to_string())
    }

    pub fn log(text: &str) {
        crate::raw::log(2, text);
    }
    pub fn warn(text: &str) {
        crate::raw::log(1, text);
    }
    pub fn error(text: &str) {
        crate::raw::log(0, text);
    }

    /// A line in one player's chat.
    pub fn tell(uuid: &str, line: &str) -> bool {
        call_as("tell", json!([uuid, line])).unwrap_or(false)
    }
    /// A line for everyone.
    pub fn broadcast(line: &str) {
        let _ = call("broadcast", json!([line]));
    }
    /// The line above the hotbar.
    pub fn action_bar(uuid: &str, line: &str) -> bool {
        call_as("action_bar", json!([uuid, line])).unwrap_or(false)
    }
    /// Run a command as the server.
    pub fn run_command(line: &str) -> bool {
        call_as("run_command", json!([line])).unwrap_or(false)
    }
    pub fn players() -> Vec<Player> {
        call_as("server_players", json!([])).unwrap_or_default()
    }
    pub fn teleport(uuid: &str, dimension: &str, pos: [f64; 3], yaw: f32, pitch: f32) -> bool {
        call_as("teleport_player", json!([uuid, dimension, pos, yaw, pitch])).unwrap_or(false)
    }
    pub fn has_permission(uuid: &str, node: &str, default_level: u8) -> bool {
        call_as("has_permission", json!([uuid, node, default_level])).unwrap_or(false)
    }
    /// `token` comes back in a `scheduled` event after `delay` ticks (then every `repeat` ticks).
    pub fn schedule(delay: u64, token: u64, repeat: u64) {
        let _ = call("schedule", json!([delay, token, repeat]));
    }
    pub fn cancel_task(token: u64) {
        let _ = call("cancel_task", json!([token]));
    }
    /// The plugin's `config.toml` as JSON (`null` when there is none).
    pub fn config() -> Value {
        call("config", json!([])).unwrap_or(Value::Null)
    }
    /// Write `config.toml` unless it exists.
    pub fn save_default_config(text: &str) {
        let _ = call("save_default_config", json!([text]));
    }
    /// A file of the plugin's folder.
    pub fn data_read(path: &str) -> Option<String> {
        call_as("data_read", json!([path])).ok().flatten()
    }
    pub fn data_write(path: &str, text: &str) -> Result<(), String> {
        call("data_write", json!([path, text])).map(|_| ())
    }
    /// Send `data` to another plugin (its `plugin_message` event, next tick).
    pub fn send_plugin(name: &str, data: Value) {
        let _ = call("send_plugin", json!([name, data]));
    }
}

/// The ABI below the SDK.
mod raw {
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "hz")]
    unsafe extern "C" {
        #[link_name = "call"]
        fn hz_call(ptr: *const u8, len: usize) -> i64;
        #[link_name = "log"]
        fn hz_log(level: i32, ptr: *const u8, len: usize);
    }

    pub fn call(request: &[u8]) -> Vec<u8> {
        #[cfg(target_arch = "wasm32")]
        {
            // SAFETY: the host reads `request` and answers a buffer from `hz_alloc`.
            let packed = unsafe { hz_call(request.as_ptr(), request.len()) };
            take(packed)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = request;
            br#"{"err":"not running on a Horizon server"}"#.to_vec()
        }
    }

    pub fn log(level: i32, text: &str) {
        #[cfg(target_arch = "wasm32")]
        // SAFETY: the host only reads the bytes.
        unsafe {
            hz_log(level, text.as_ptr(), text.len())
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (level, text);
        }
    }

    /// Take back a buffer the host filled (from [`alloc`]).
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn take(packed: i64) -> Vec<u8> {
        let (ptr, len) = ((packed as u64 >> 32) as usize, packed as u32 as usize);
        if len == 0 {
            return Vec::new();
        }
        // SAFETY: `ptr` and `len` came from `alloc`.
        unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr as *mut u8, len)).into_vec() }
    }

    pub fn alloc(len: i32) -> i32 {
        let buffer = vec![0u8; len.max(0) as usize].into_boxed_slice();
        Box::into_raw(buffer) as *mut u8 as usize as i32
    }

    pub fn free(ptr: i32, len: i32) {
        if len > 0 {
            // SAFETY: from `alloc` with this length.
            unsafe {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    ptr as usize as *mut u8,
                    len as usize,
                )))
            }
        }
    }

    /// Hand `bytes` to the host (it frees them with `hz_free`).
    pub fn give(bytes: Vec<u8>) -> i64 {
        if bytes.is_empty() {
            return 0;
        }
        let len = bytes.len();
        let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8 as usize;
        ((ptr as u64) << 32 | len as u64) as i64
    }
}

#[doc(hidden)]
pub mod __private {
    use super::*;
    use std::sync::Mutex;

    static PLUGIN: Mutex<Option<Box<dyn Plugin>>> = Mutex::new(None);

    pub fn init(make: impl FnOnce() -> Box<dyn Plugin>) -> i64 {
        let plugin = make();
        let info = serde_json::to_vec(&plugin.info()).unwrap();
        *PLUGIN.lock().unwrap() = Some(plugin);
        raw::give(info)
    }

    pub fn event(ptr: i32, len: i32) -> i64 {
        let input = raw::take(((ptr as u32 as u64) << 32 | len as u32 as u64) as i64);
        let Ok(event) = serde_json::from_slice::<Event>(&input) else {
            return 0;
        };
        let mut plugin = PLUGIN.lock().unwrap();
        let Some(plugin) = plugin.as_mut() else {
            return 0;
        };
        let reply = plugin.on_event(&event);
        raw::give(serde_json::to_vec(&reply).unwrap())
    }

    pub fn alloc(len: i32) -> i32 {
        raw::alloc(len)
    }
    pub fn free(ptr: i32, len: i32) {
        raw::free(ptr, len)
    }
}

/// Export a plugin: `register!(MyPlugin::default());`
#[macro_export]
macro_rules! register {
    ($make:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn hz_init() -> i64 {
            $crate::__private::init(|| Box::new($make))
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn hz_event(ptr: i32, len: i32) -> i64 {
            $crate::__private::event(ptr, len)
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn hz_alloc(len: i32) -> i32 {
            $crate::__private::alloc(len)
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn hz_free(ptr: i32, len: i32) {
            $crate::__private::free(ptr, len)
        }
    };
}
