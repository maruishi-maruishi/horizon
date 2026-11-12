//! WebAssembly plugins for a Horizon server.
//!
//! Each `plugins/*.wasm` file is a plugin: loaded at start, reloaded when its file changes,
//! sandboxed (its own memory, a time limit on every call, no file or network access but its own
//! folder). The plugin host is one trait mod (`Hooks`): it hears every hook of the game and hands
//! those a plugin asked for to it as JSON, and plugins call the game's `Host` helpers by name.
//! Both sides are generated from the game's hook catalog (build.rs), so every hook and helper the
//! game has reaches plugins.
//!
//! On top of the hooks the host gives plugins commands (registered with the server's command
//! tree, permission nodes included), a scheduler, a config file, a data folder, messages between
//! plugins, and a permission system (`plugins/permissions.toml`) the whole server uses.
//!
//! The plugin ABI (all JSON, UTF-8; `(ptr, len)` packed in an i64 as `ptr << 32 | len`):
//!
//! - the plugin exports `memory`, `hz_alloc(len) -> ptr`, `hz_free(ptr, len)`,
//!   `hz_init() -> packed` (its [`PluginInfo`]) and `hz_event(ptr, len) -> packed`
//!   (`{"event", "data"}` in, a [`Reply`] out; length 0 means "nothing");
//! - the host gives `hz.call(ptr, len) -> packed` (`{"fn", "args"}` in, `{"ok"}` or `{"err"}`
//!   out) and `hz.log(level, ptr, len)`.
//!
//! The SDK for writing plugins is `plugins/sdk` (`horizon-plugin-sdk`).

mod conv;
mod permissions;
mod skins;

use conv::{FromJson, ToJson, arg};
use horizon_client::mod_api::*;
use permissions::Permissions;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use wasmtime::{
    AsContextMut, Caller, Config, Engine, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc,
};

include!(concat!(env!("OUT_DIR"), "/bridge_hooks.rs"));
include!(concat!(env!("OUT_DIR"), "/bridge_host.rs"));

/// A plugin call may run this many epochs (10 ms each) before it is stopped.
const CALL_EPOCHS: u64 = 100;
/// A plugin's memory limit.
const MEMORY_LIMIT: usize = 128 * 1024 * 1024;
/// Traps in a row that turn a plugin off until its file changes.
const MAX_FAILURES: u32 = 5;
/// How often (in server ticks) the plugin folder and the permissions file are checked.
const RESCAN_TICKS: u64 = 20;
/// What a helper call answers inside a hook without helpers.
const NO_HELPERS: &str =
    "this hook gives no helpers: answer through the reply, or schedule the work for the next tick";
/// Events that are not hooks, sent by the host itself.
const PLUGIN_EVENTS: [&str; 5] = [
    "enable",
    "disable",
    "command",
    "scheduled",
    "plugin_message",
];

/// The game's helpers for the plugin call being made.
pub enum HostRef<'a> {
    Mut(&'a mut dyn Host),
    Shared(&'a dyn Host),
    /// A hook the game gives no helpers in (`player_chat`, say).
    None,
}

impl HostRef<'_> {
    fn mutable(&mut self) -> Result<&mut dyn Host, String> {
        match self {
            HostRef::Mut(h) => Ok(&mut **h),
            HostRef::Shared(_) => {
                Err("this hook only reads the game: the helper changes it".into())
            }
            HostRef::None => Err(NO_HELPERS.into()),
        }
    }
    fn shared(&self) -> Result<&dyn Host, String> {
        match self {
            HostRef::Mut(h) => Ok(&**h),
            HostRef::Shared(h) => Ok(*h),
            HostRef::None => Err(NO_HELPERS.into()),
        }
    }
}

thread_local! {
    /// The host of the dispatch running on this thread, for the plugins' `hz.call`.
    static CURRENT: Cell<*mut HostRef<'static>> = const { Cell::new(std::ptr::null_mut()) };
}

/// Makes `host` the current host until dropped; restores the one before (dispatches nest when a
/// plugin's helper call fires another hook).
struct CurrentHost(*mut HostRef<'static>);

impl CurrentHost {
    fn set(host: &mut HostRef<'_>) -> Self {
        // SAFETY: the pointer is only used while this guard lives, on this thread, and the guard
        // lives inside the borrow of `host`; the lifetime is erased only to store it.
        let ptr = std::ptr::from_mut(host).cast::<HostRef<'static>>();
        Self(CURRENT.with(|c| c.replace(ptr)))
    }
}

impl Drop for CurrentHost {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.0));
    }
}

fn with_current<R>(f: impl FnOnce(&mut HostRef<'_>) -> Result<R, String>) -> Result<R, String> {
    let ptr = CURRENT.with(Cell::get);
    if ptr.is_null() {
        return Err("no game here: call helpers from an event (enable, a hook, a command)".into());
    }
    // SAFETY: set by a live `CurrentHost` on this thread (see there).
    f(unsafe { &mut *ptr })
}

/// What a plugin's `hz_init` says about itself.
#[derive(Debug, Default, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// Hooks (`destroy_block`...) and plugin events (`scheduled`...) it wants.
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PluginCommand {
    /// `home set <name:word>`: words, then `<name:kind>` and `[name:kind]` (see `NativeCommand`).
    pub usage: String,
    #[serde(default)]
    pub level: u8,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub replace_vanilla: bool,
}

impl PluginCommand {
    fn root(&self) -> &str {
        self.usage.split_whitespace().next().unwrap_or("")
    }
}

/// A plugin's answer to an event.
#[derive(Debug, Default, Deserialize)]
pub struct Reply {
    /// Stop what the hook is about to do (a cancellable hook).
    #[serde(default)]
    pub cancel: bool,
    /// New values for the event's `&mut` fields.
    #[serde(default)]
    pub set: serde_json::Map<String, Value>,
    /// The replacement value (a hook that returns one).
    #[serde(default)]
    pub value: Option<Value>,
    /// Lines for whoever ran a command.
    #[serde(default)]
    pub messages: Vec<String>,
}

struct Instance {
    store: Store<PluginState>,
    alloc: TypedFunc<i32, i32>,
    free: Option<TypedFunc<(i32, i32), ()>>,
    event: TypedFunc<(i32, i32), i64>,
    memory: Memory,
}

struct PluginState {
    name: String,
    dir: PathBuf,
    limits: StoreLimits,
    shared: Arc<Shared>,
}

struct Plugin {
    info: PluginInfo,
    file: PathBuf,
    modified: Option<SystemTime>,
    /// Per hook index: wanted.
    hooks: Vec<bool>,
    plugin_events: HashSet<String>,
    instance: Mutex<Instance>,
    failures: AtomicU32,
    off: AtomicBool,
    enabled: AtomicBool,
}

struct Task {
    plugin: String,
    due: u64,
    token: u64,
    repeat: u64,
}

struct Shared {
    dir: PathBuf,
    engine: Engine,
    linker: Linker<PluginState>,
    plugins: RwLock<Vec<Arc<Plugin>>>,
    wanted: Vec<AtomicBool>,
    tick: AtomicU64,
    tasks: Mutex<Vec<Task>>,
    messages: Mutex<Vec<(String, String, Value)>>,
    permissions: RwLock<Permissions>,
    names: RwLock<HashMap<u128, String>>,
    /// The command roots registered with the server's tree (others route through
    /// `player_command`: plugins loaded after the start).
    registered: RwLock<HashSet<String>>,
    /// The plugins' commands changed: the server's tree is rebuilt at the next tick.
    commands_dirty: AtomicBool,
    /// The permissions changed: every player is sent their commands again.
    permissions_dirty: AtomicBool,
    /// Offline-mode skins (`plugins/skins.toml`).
    skins: skins::Skins,
}

/// The plugin host: one trait mod for all plugins.
pub struct WasmHost(Arc<Shared>);

impl WasmHost {
    /// Load the plugins in `dir` (created if missing).
    pub fn load(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::error!("plugins: cannot create {}: {e}", dir.display());
        }
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).expect("starting the WebAssembly engine");
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("plugin-epochs".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                    ticker.increment_epoch();
                }
            })
            .expect("starting the plugin clock");
        let mut linker = Linker::new(&engine);
        define_imports(&mut linker).expect("defining the plugin imports");
        let shared = Arc::new(Shared {
            permissions: RwLock::new(Permissions::load(&dir.join("permissions.toml"))),
            skins: skins::Skins::load(&dir.join("skins.toml")),
            dir,
            engine,
            linker,
            plugins: RwLock::new(Vec::new()),
            wanted: HOOK_NAMES.iter().map(|_| AtomicBool::new(false)).collect(),
            tick: AtomicU64::new(0),
            tasks: Mutex::new(Vec::new()),
            messages: Mutex::new(Vec::new()),
            names: RwLock::new(HashMap::new()),
            registered: RwLock::new(HashSet::new()),
            commands_dirty: AtomicBool::new(false),
            permissions_dirty: AtomicBool::new(false),
        });
        let host = Self(shared);
        host.rescan();
        host
    }

    /// The loaded plugins' names and versions.
    pub fn plugins(&self) -> Vec<(String, String)> {
        self.0
            .plugins
            .read()
            .unwrap()
            .iter()
            .map(|p| (p.info.name.clone(), p.info.version.clone()))
            .collect()
    }

    fn wants(&self, index: usize) -> bool {
        self.0.wanted[index].load(Ordering::Relaxed)
    }

    fn refresh_wanted(&self) {
        let plugins = self.0.plugins.read().unwrap();
        for (i, flag) in self.0.wanted.iter().enumerate() {
            flag.store(plugins.iter().any(|p| p.hooks[i]), Ordering::Relaxed);
        }
    }

    /// Load new and changed plugin files, drop removed ones, and reread the permissions.
    fn rescan(&self) {
        let shared = &self.0;
        if shared.permissions.write().unwrap().reload_if_changed() {
            shared.permissions_dirty.store(true, Ordering::Relaxed);
        }
        let mut files: Vec<PathBuf> = match std::fs::read_dir(&shared.dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "wasm"))
                .collect(),
            Err(_) => return,
        };
        files.sort();
        let current: Vec<Arc<Plugin>> = shared.plugins.read().unwrap().clone();
        let mut next = Vec::new();
        let mut changed = false;
        for file in &files {
            let modified = std::fs::metadata(file).and_then(|m| m.modified()).ok();
            if let Some(p) = current.iter().find(|p| &p.file == file) {
                if p.modified == modified {
                    next.push(p.clone());
                    continue;
                }
                log::info!("plugins: {} changed, reloading", file.display());
                self.disable(p);
            }
            changed = true;
            match self.instantiate(file, modified) {
                Ok(plugin) => {
                    if next
                        .iter()
                        .any(|p: &Arc<Plugin>| p.info.name == plugin.info.name)
                    {
                        log::error!(
                            "plugins: {} is named `{}` like another plugin; skipped",
                            file.display(),
                            plugin.info.name
                        );
                        continue;
                    }
                    log::info!(
                        "plugins: loaded {} {} ({} events, {} commands)",
                        plugin.info.name,
                        plugin.info.version,
                        plugin.info.events.len(),
                        plugin.info.commands.len()
                    );
                    next.push(Arc::new(plugin));
                }
                Err(e) => log::error!("plugins: cannot load {}: {e:#}", file.display()),
            }
        }
        for p in &current {
            if !files.contains(&p.file) {
                log::info!(
                    "plugins: {} removed, unloading {}",
                    p.file.display(),
                    p.info.name
                );
                self.disable(p);
                changed = true;
            }
        }
        if changed {
            let names: HashSet<&str> = next.iter().map(|p| p.info.name.as_str()).collect();
            shared
                .tasks
                .lock()
                .unwrap()
                .retain(|t| names.contains(t.plugin.as_str()));
            *shared.plugins.write().unwrap() = next;
            self.refresh_wanted();
            shared.commands_dirty.store(true, Ordering::Relaxed);
        }
    }

    fn instantiate(
        &self,
        file: &Path,
        modified: Option<SystemTime>,
    ) -> anyhow_like::Result<Plugin> {
        let shared = &self.0;
        let module = Module::from_file(&shared.engine, file)?;
        let stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_owned();
        let state = PluginState {
            name: stem.clone(),
            dir: shared.dir.join(&stem),
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_LIMIT)
                .instances(1)
                .build(),
            shared: shared.clone(),
        };
        let mut store = Store::new(&shared.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(CALL_EPOCHS * 5);
        let instance = shared.linker.instantiate(&mut store, &module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow_like::msg("the plugin exports no `memory`"))?;
        let alloc = instance.get_typed_func::<i32, i32>(&mut store, "hz_alloc")?;
        let free = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "hz_free")
            .ok();
        let init = instance.get_typed_func::<(), i64>(&mut store, "hz_init")?;
        let event = instance.get_typed_func::<(i32, i32), i64>(&mut store, "hz_event")?;
        let packed = init.call(&mut store, ())?;
        let mut instance = Instance {
            store,
            alloc,
            free,
            event,
            memory,
        };
        let text = instance.take(packed)?;
        let mut info: PluginInfo = serde_json::from_slice(&text)
            .map_err(|e| anyhow_like::msg(format!("hz_init gave no plugin info: {e}")))?;
        if info.name.is_empty() {
            info.name = stem;
        }
        // Its folder takes its own name.
        instance.store.data_mut().name = info.name.clone();
        instance.store.data_mut().dir = shared.dir.join(&info.name);
        let mut hooks = vec![false; HOOK_NAMES.len()];
        let mut plugin_events = HashSet::new();
        for e in &info.events {
            if let Some(i) = HOOK_NAMES.iter().position(|h| h == e) {
                hooks[i] = true;
            } else if PLUGIN_EVENTS.contains(&e.as_str()) {
                plugin_events.insert(e.clone());
            } else {
                log::warn!(
                    "plugins: {} asks for `{e}`, which no hook or plugin event is called",
                    info.name
                );
            }
        }
        for c in &info.commands {
            let root = c.root();
            if !shared.registered.read().unwrap().contains(root)
                && shared.tick.load(Ordering::Relaxed) > 0
            {
                log::info!(
                    "plugins: /{root} of {} works now; its tab completion comes with the next restart",
                    info.name
                );
            }
        }
        Ok(Plugin {
            info,
            file: file.to_owned(),
            modified,
            hooks,
            plugin_events,
            instance: Mutex::new(instance),
            failures: AtomicU32::new(0),
            off: AtomicBool::new(false),
            enabled: AtomicBool::new(false),
        })
    }

    fn disable(&self, plugin: &Plugin) {
        if plugin.enabled.load(Ordering::Relaxed) && plugin.plugin_events.contains("disable") {
            // Outside a dispatch there is no game: the plugin may still save its own data.
            let _ = self.call(plugin, "disable", Value::Null);
        }
    }

    /// Send `event` to one plugin and read its reply. None when it is busy (this event fired from
    /// inside its own call), off, or failed.
    fn call(&self, plugin: &Plugin, event: &str, data: Value) -> Option<Reply> {
        if plugin.off.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(mut instance) = plugin.instance.try_lock() else {
            log::debug!("plugins: {} is busy, skipping {event}", plugin.info.name);
            return None;
        };
        let input = serde_json::to_vec(&json!({ "event": event, "data": data })).ok()?;
        let result = instance.event_call(&input);
        match result {
            Ok(bytes) => {
                plugin.failures.store(0, Ordering::Relaxed);
                if bytes.is_empty() {
                    return Some(Reply::default());
                }
                match serde_json::from_slice::<Reply>(&bytes) {
                    Ok(reply) => Some(reply),
                    Err(e) => {
                        log::warn!(
                            "plugins: {} answered {event} with no reply: {e}",
                            plugin.info.name
                        );
                        None
                    }
                }
            }
            Err(e) => {
                let failures = plugin.failures.fetch_add(1, Ordering::Relaxed) + 1;
                log::error!("plugins: {} failed in {event}: {e:#}", plugin.info.name);
                if failures >= MAX_FAILURES {
                    log::error!(
                        "plugins: {} failed {failures} times in a row; turned off until its file changes",
                        plugin.info.name
                    );
                    plugin.off.store(true, Ordering::Relaxed);
                }
                None
            }
        }
    }

    /// Send a hook's event to every plugin that wants it, in load order. The first plugin that
    /// cancels or answers a value ends it; values it set reach the plugins after it.
    fn dispatch(&self, mut host: HostRef<'_>, index: usize, mut data: Value) -> Reply {
        let plugins = self.0.plugins.read().unwrap().clone();
        let _current = CurrentHost::set(&mut host);
        let mut merged = Reply::default();
        for plugin in plugins.iter().filter(|p| p.hooks[index]) {
            let Some(reply) = self.call(plugin, HOOK_NAMES[index], data.clone()) else {
                continue;
            };
            for (k, v) in reply.set {
                if let Value::Object(m) = &mut data {
                    m.insert(k.clone(), v.clone());
                }
                merged.set.insert(k, v);
            }
            merged.messages.extend(reply.messages);
            if reply.value.is_some() {
                merged.value = reply.value;
                break;
            }
            if reply.cancel {
                merged.cancel = true;
                break;
            }
        }
        merged
    }

    /// Send a plugin event (not a hook) to the plugin named `name`.
    fn send_to(
        &self,
        mut host: HostRef<'_>,
        name: &str,
        event: &str,
        data: Value,
    ) -> Option<Reply> {
        let plugin = self
            .0
            .plugins
            .read()
            .unwrap()
            .iter()
            .find(|p| p.info.name == name)
            .cloned()?;
        if event != "command" && !plugin.plugin_events.contains(event) {
            return None;
        }
        let _current = CurrentHost::set(&mut host);
        self.call(&plugin, event, data)
    }

    fn command_owner(&self, root: &str) -> Option<(String, Vec<PluginCommand>)> {
        self.0.plugins.read().unwrap().iter().find_map(|p| {
            let commands: Vec<_> = p
                .info
                .commands
                .iter()
                .filter(|c| c.root() == root)
                .cloned()
                .collect();
            (!commands.is_empty()).then(|| (p.info.name.clone(), commands))
        })
    }

    /// Run a plugin command, or the host's own `/plugins`. The lines it answers.
    fn run_command(
        &self,
        host: HostRef<'_>,
        player: Option<Value>,
        line: &str,
    ) -> Option<Vec<String>> {
        let words: Vec<&str> = line.split_whitespace().collect();
        let root = *words.first()?;
        if root == "plugins" {
            return Some(self.plugins_command(&words[1..]));
        }
        let (owner, _) = self.command_owner(root)?;
        let data = json!({
            "player": player,
            "command": line,
            "label": root,
            "args": words[1..],
        });
        let reply = self
            .send_to(host, &owner, "command", data)
            .unwrap_or_default();
        Some(reply.messages)
    }

    fn plugins_command(&self, args: &[&str]) -> Vec<String> {
        match args {
            ["reload", ..] => {
                // Mark every plugin (or the one named) changed, then rescan.
                let names: Vec<String> =
                    args.get(1).map(|n| vec![n.to_string()]).unwrap_or_default();
                let plugins = self.0.plugins.read().unwrap().clone();
                let mut kept = Vec::new();
                for p in plugins {
                    if names.is_empty() || names.contains(&p.info.name) {
                        self.disable(&p);
                    } else {
                        kept.push(p);
                    }
                }
                *self.0.plugins.write().unwrap() = kept;
                self.rescan();
                vec![format!(
                    "§aReloaded. {} plugins loaded.",
                    self.0.plugins.read().unwrap().len()
                )]
            }
            _ => {
                let plugins = self.0.plugins.read().unwrap();
                let mut lines = vec![format!("§ePlugins ({}):", plugins.len())];
                for p in plugins.iter() {
                    let state = if p.off.load(Ordering::Relaxed) {
                        " §c(off: failed)"
                    } else {
                        ""
                    };
                    lines.push(format!("§f- {} §7{}{state}", p.info.name, p.info.version));
                }
                lines
            }
        }
    }

    // ---- Hooks the host answers itself (build.rs `CUSTOM`). ----

    fn custom_register_commands(&self, host: &mut dyn Host, event: &mut RegisterCommandsEvent<'_>) {
        let _ = host;
        let mut registered = self.0.registered.write().unwrap();
        registered.clear();
        event.commands.push(NativeCommand::op("plugins", 3));
        event
            .commands
            .push(NativeCommand::op("plugins reload [plugin:word]", 3));
        registered.insert("plugins".into());
        for plugin in self.0.plugins.read().unwrap().iter() {
            for c in &plugin.info.commands {
                let mut command =
                    NativeCommand::op(&c.usage, c.level).replace_vanilla(c.replace_vanilla);
                if let Some(node) = &c.permission {
                    command = command.permission(node);
                }
                event.commands.push(command);
                registered.insert(c.root().to_owned());
            }
        }
    }

    fn custom_perform_command(
        &self,
        host: &mut dyn Host,
        event: &mut PerformCommandEvent<'_>,
    ) -> Flow {
        let player = event.player.to_json();
        if let Some(lines) = self.run_command(HostRef::Mut(&mut *host), Some(player), event.command)
        {
            for line in lines {
                host.feedback(Text::from_json(&Value::String(line)).unwrap());
            }
            return Flow::Cancel;
        }
        self.forward_perform_command(host, event)
    }

    fn custom_perform_console_command(
        &self,
        host: &mut dyn Host,
        event: &mut PerformConsoleCommandEvent<'_>,
    ) -> Flow {
        if let Some(lines) = self.run_command(HostRef::Mut(&mut *host), None, event.command) {
            for line in lines {
                host.feedback(Text::from_json(&Value::String(line)).unwrap());
            }
            return Flow::Cancel;
        }
        self.forward_perform_console_command(host, event)
    }

    /// Commands of plugins loaded after the start aren't in the server's tree: they arrive here.
    fn custom_player_command(
        &self,
        host: &mut dyn Host,
        event: &mut PlayerCommandEvent<'_>,
    ) -> Flow {
        let root = event.command.split_whitespace().next().unwrap_or("");
        let unregistered = !self.0.registered.read().unwrap().contains(root);
        if unregistered && let Some((_, commands)) = self.command_owner(root) {
            let allowed = commands.iter().any(|c| match &c.permission {
                Some(node) => host.has_permission(event.player, node, c.level),
                None => host.player_permission(event.player).unwrap_or(0) >= c.level,
            });
            if !allowed {
                event.replies.push(Text::legacy(
                    "§cYou do not have permission to use this command.",
                ));
                return Flow::Cancel;
            }
            let player = host
                .server_players()
                .into_iter()
                .find(|p| p.uuid == event.player)
                .map(|p| p.to_json());
            if let Some(lines) = self.run_command(HostRef::Mut(&mut *host), player, event.command) {
                for line in lines {
                    event
                        .replies
                        .push(Text::from_json(&Value::String(line)).unwrap());
                }
            }
            return Flow::Cancel;
        }
        self.forward_player_command(host, event)
    }

    fn custom_permission_check(
        &self,
        host: &dyn Host,
        event: &mut PermissionCheckEvent<'_>,
    ) -> Option<bool> {
        let name = self.0.names.read().unwrap().get(&event.player).cloned();
        let decided =
            self.0
                .permissions
                .read()
                .unwrap()
                .check(event.player, name.as_deref(), event.node);
        decided.or_else(|| self.forward_permission_check(host, event))
    }

    fn custom_permission_groups(&self, host: &dyn Host, event: &mut PermissionGroupsEvent<'_>) {
        let name = self.0.names.read().unwrap().get(&event.player).cloned();
        for group in self
            .0
            .permissions
            .read()
            .unwrap()
            .groups_of(event.player, name.as_deref())
        {
            if !event.groups.contains(&group) {
                event.groups.push(group);
            }
        }
        self.forward_permission_groups(host, event);
    }

    fn custom_player_logged_in(&self, host: &mut dyn Host, event: &mut PlayerLoggedInEvent<'_>) {
        self.0
            .names
            .write()
            .unwrap()
            .insert(event.player, event.name.to_owned());
        self.forward_player_logged_in(host, event);
    }

    /// On the login's own thread: an offline-mode player gets their Mojang skin and cape.
    fn custom_player_profile(&self, event: &mut PlayerProfileEvent<'_>) {
        self.0.skins.fill(event.name, event.properties);
        self.forward_player_profile(event);
    }

    fn custom_server_tick(&self, host: &mut dyn Host, event: &mut ServerTickEvent<'_>) {
        if matches!(event.phase, ServerTickPhase::ServerTickStart { .. }) {
            self.tick_start(host);
        }
        self.forward_server_tick(host, event);
    }

    /// The start of a server tick: reloads, enables, messages, scheduled tasks.
    fn tick_start(&self, host: &mut dyn Host) {
        {
            let tick = self.0.tick.fetch_add(1, Ordering::Relaxed) + 1;
            if tick == 1 {
                // The game's log is up by now (not yet when the plugins first loaded).
                log::info!(
                    "plugins: {} loaded from {} ({} hooks and {} helpers available)",
                    self.0.plugins.read().unwrap().len(),
                    self.0.dir.display(),
                    HOOK_NAMES.len(),
                    HOST_FUNCTIONS.len()
                );
            }
            if tick.is_multiple_of(RESCAN_TICKS) {
                self.rescan();
            }
            if self.0.commands_dirty.swap(false, Ordering::Relaxed) && tick > 1 {
                host.refresh_commands();
            }
            if self.0.permissions_dirty.swap(false, Ordering::Relaxed) {
                for player in host.server_players() {
                    host.resend_commands(player.uuid);
                }
            }
            // Plugins not yet enabled (loaded at start, or since).
            let plugins = self.0.plugins.read().unwrap().clone();
            for p in &plugins {
                if !p.enabled.swap(true, Ordering::Relaxed) && p.plugin_events.contains("enable") {
                    self.send_to(
                        HostRef::Mut(&mut *host),
                        &p.info.name,
                        "enable",
                        Value::Null,
                    );
                }
            }
            // Messages between plugins, from the tick before.
            let messages = std::mem::take(&mut *self.0.messages.lock().unwrap());
            for (from, to, data) in messages {
                self.send_to(
                    HostRef::Mut(&mut *host),
                    &to,
                    "plugin_message",
                    json!({ "from": from, "data": data }),
                );
            }
            // Scheduled tasks.
            let due: Vec<(String, u64)> = {
                let mut tasks = self.0.tasks.lock().unwrap();
                let mut due = Vec::new();
                tasks.retain_mut(|t| {
                    if t.due > tick {
                        return true;
                    }
                    due.push((t.plugin.clone(), t.token));
                    if t.repeat > 0 {
                        t.due = tick + t.repeat;
                        true
                    } else {
                        false
                    }
                });
                due
            };
            for (plugin, token) in due {
                self.send_to(
                    HostRef::Mut(&mut *host),
                    &plugin,
                    "scheduled",
                    json!({ "token": token }),
                );
            }
        }
    }
}

impl Instance {
    /// Copy `bytes` into the plugin's memory.
    fn put(&mut self, bytes: &[u8]) -> wasmtime::Result<(i32, i32)> {
        let len = i32::try_from(bytes.len())?;
        let ptr = self.alloc.call(&mut self.store, len)?;
        self.memory
            .write(&mut self.store, ptr as u32 as usize, bytes)?;
        Ok((ptr, len))
    }

    /// Read (and free) the buffer `packed` names.
    fn take(&mut self, packed: i64) -> wasmtime::Result<Vec<u8>> {
        let (ptr, len) = unpack(packed);
        if len == 0 {
            return Ok(Vec::new());
        }
        let data = self.memory.data(&self.store);
        let bytes = data
            .get(ptr as usize..ptr as usize + len as usize)
            .ok_or_else(|| wasmtime::Error::msg("the plugin answered outside its memory"))?
            .to_vec();
        if let Some(free) = &self.free {
            free.call(&mut self.store, (ptr as i32, len as i32))?;
        }
        Ok(bytes)
    }

    fn event_call(&mut self, input: &[u8]) -> wasmtime::Result<Vec<u8>> {
        self.store.set_epoch_deadline(CALL_EPOCHS);
        let (ptr, len) = self.put(input)?;
        let packed = self.event.call(&mut self.store, (ptr, len))?;
        self.take(packed)
    }
}

fn unpack(packed: i64) -> (u32, u32) {
    let packed = packed as u64;
    ((packed >> 32) as u32, packed as u32)
}

fn pack(ptr: i32, len: i32) -> i64 {
    ((u64::from(ptr as u32) << 32) | u64::from(len as u32)) as i64
}

/// Read `len` bytes at `ptr` of the calling plugin's memory.
fn read_guest(
    caller: &mut Caller<'_, PluginState>,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<Vec<u8>> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("no memory"))?;
    let data = memory.data(&caller);
    let start = ptr as u32 as usize;
    Ok(data
        .get(start..start + len as u32 as usize)
        .ok_or_else(|| wasmtime::Error::msg("a buffer outside the plugin's memory"))?
        .to_vec())
}

/// Copy `bytes` into the calling plugin's memory, through its `hz_alloc`.
fn write_guest(caller: &mut Caller<'_, PluginState>, bytes: &[u8]) -> wasmtime::Result<i64> {
    let alloc = caller
        .get_export("hz_alloc")
        .and_then(|e| e.into_func())
        .ok_or_else(|| wasmtime::Error::msg("no hz_alloc"))?
        .typed::<i32, i32>(&caller)?;
    let len = i32::try_from(bytes.len())?;
    let ptr = alloc.call(&mut *caller, len)?;
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("no memory"))?;
    memory.write(&mut *caller, ptr as u32 as usize, bytes)?;
    Ok(pack(ptr, len))
}

fn define_imports(linker: &mut Linker<PluginState>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "hz",
        "log",
        |mut caller: Caller<'_, PluginState>,
         level: i32,
         ptr: i32,
         len: i32|
         -> wasmtime::Result<()> {
            let bytes = read_guest(&mut caller, ptr, len)?;
            let text = String::from_utf8_lossy(&bytes);
            let name = &caller.data().name;
            match level {
                0 => log::error!("[{name}] {text}"),
                1 => log::warn!("[{name}] {text}"),
                2 => log::info!("[{name}] {text}"),
                _ => log::debug!("[{name}] {text}"),
            }
            Ok(())
        },
    )?;
    linker.func_wrap(
        "hz",
        "call",
        |mut caller: Caller<'_, PluginState>, ptr: i32, len: i32| -> wasmtime::Result<i64> {
            let bytes = read_guest(&mut caller, ptr, len)?;
            let request: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let name = request["fn"].as_str().unwrap_or("").to_owned();
            let args = request["args"].as_array().cloned().unwrap_or_default();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                builtin(caller.data(), &name, &args)
                    .unwrap_or_else(|| with_current(|host| call_host(host, &name, &args)))
            }))
            .unwrap_or_else(|_| Err(format!("the game failed in `{name}`")));
            // Time spent in the game doesn't count against the plugin.
            caller.as_context_mut().set_epoch_deadline(CALL_EPOCHS);
            let reply = match result {
                Ok(v) => json!({ "ok": v }),
                Err(e) => json!({ "err": e }),
            };
            write_guest(&mut caller, &serde_json::to_vec(&reply)?)
        },
    )?;
    Ok(())
}

/// A plugin's own path under its folder, refusing anything that leaves it.
fn data_path(state: &PluginState, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "`{relative}` is not a path inside the plugin's folder"
        ));
    }
    Ok(state.dir.join(path))
}

/// The helpers the plugin host itself gives (not the game's `Host`). None for another name.
fn builtin(state: &PluginState, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    let shared = &state.shared;
    let result = match name {
        "plugin_name" => Ok(json!(state.name)),
        "tick" => Ok(json!(shared.tick.load(Ordering::Relaxed))),
        "now_ms" => Ok(json!(
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
        )),
        "plugins" => Ok(json!(
            shared.plugins.read().unwrap().iter().map(|p| json!({"name": p.info.name, "version": p.info.version})).collect::<Vec<_>>()
        )),
        "hooks" => Ok(json!(
            HOOK_NAMES.iter().zip(HOOK_FIELDS).map(|(h, f)| json!({"name": h, "fields": f.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>()})).collect::<Vec<_>>()
        )),
        "host_functions" => Ok(json!(HOST_FUNCTIONS.to_vec())),
        "schedule" => (|| {
            let delay: u64 = arg(args, 0)?;
            let token: u64 = arg(args, 1)?;
            let repeat: u64 = Option::<u64>::from_json(args.get(2).unwrap_or(&Value::Null))?.unwrap_or(0);
            let due = shared.tick.load(Ordering::Relaxed) + delay.max(1);
            shared.tasks.lock().unwrap().push(Task { plugin: state.name.clone(), due, token, repeat });
            Ok(Value::Null)
        })(),
        "cancel_task" => (|| {
            let token: u64 = arg(args, 0)?;
            shared.tasks.lock().unwrap().retain(|t| !(t.plugin == state.name && t.token == token));
            Ok(Value::Null)
        })(),
        "send_plugin" => (|| {
            let to: String = arg(args, 0)?;
            let data = args.get(1).cloned().unwrap_or(Value::Null);
            shared.messages.lock().unwrap().push((state.name.clone(), to, data));
            Ok(Value::Null)
        })(),
        "config" => {
            let path = state.dir.join("config.toml");
            match std::fs::read_to_string(&path) {
                Ok(text) => toml::from_str::<toml::Value>(&text)
                    .map_err(|e| format!("{}: {e}", path.display()))
                    .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string())),
                Err(_) => Ok(Value::Null),
            }
        }
        "save_default_config" => (|| {
            let text: String = arg(args, 0)?;
            let path = state.dir.join("config.toml");
            if !path.exists() {
                std::fs::create_dir_all(&state.dir).map_err(|e| e.to_string())?;
                std::fs::write(&path, text).map_err(|e| e.to_string())?;
            }
            Ok(Value::Null)
        })(),
        "data_read" => (|| {
            let path = data_path(state, &arg::<String>(args, 0)?)?;
            Ok(std::fs::read_to_string(path).map_or(Value::Null, Value::String))
        })(),
        "data_write" => (|| {
            let path = data_path(state, &arg::<String>(args, 0)?)?;
            let text: String = arg(args, 1)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            // Written whole, then renamed, so a crash never leaves half a file.
            let temporary = path.with_extension("tmp-write");
            std::fs::write(&temporary, text).map_err(|e| e.to_string())?;
            std::fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
            Ok(Value::Null)
        })(),
        "data_delete" => (|| {
            let path = data_path(state, &arg::<String>(args, 0)?)?;
            Ok(json!(std::fs::remove_file(path).is_ok()))
        })(),
        _ => return None,
    };
    Some(result)
}

/// A small error type for loading (wasmtime's error, or a message).
mod anyhow_like {
    pub type Result<T> = std::result::Result<T, wasmtime::Error>;
    pub fn msg(m: impl std::fmt::Display + Send + Sync + 'static) -> wasmtime::Error {
        wasmtime::Error::msg(m.to_string())
    }
}

#[cfg(test)]
mod tests;
