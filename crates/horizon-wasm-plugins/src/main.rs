//! `horizon-plugin-server`: the Horizon game with the WebAssembly plugin host installed. Run it as
//! the game (`--dedicated` for a server); plugins load from `plugins/` in the working directory, or
//! from the folder `HORIZON_PLUGINS` names.
#![forbid(unsafe_code)]

#[global_allocator]
static ALLOCATOR: horizon_threads::Allocator = horizon_threads::ALLOCATOR;

/// The mod API level this game serves, kept in the program for the launcher to read.
static MOD_API_LEVEL: [u8; horizon_mods::api_level::MARK.len()] = horizon_mods::api_level::MARK;

fn main() {
    std::hint::black_box(&MOD_API_LEVEL);
    if std::env::args().any(|a| a == "--plugin-api") {
        println!("# Hooks (events): name: fields");
        for (hook, fields) in horizon_wasm_plugins::HOOK_NAMES
            .iter()
            .zip(horizon_wasm_plugins::HOOK_FIELDS)
        {
            println!("{hook}: {}", fields.replace(',', ", "));
        }
        println!(
            "
# Plugin events
enable, disable, command, scheduled, plugin_message"
        );
        println!(
            "
# Game helpers (host::call)"
        );
        for helper in horizon_wasm_plugins::HOST_FUNCTIONS {
            println!("{helper}");
        }
        println!("
# Plugin host helpers
plugin_name, tick, now_ms, plugins, hooks, host_functions, schedule, cancel_task, send_plugin, config, save_default_config, data_read, data_write, data_delete");
        return;
    }
    let dir = std::env::var_os("HORIZON_PLUGINS")
        .map_or_else(|| "plugins".into(), std::path::PathBuf::from);
    let host = horizon_wasm_plugins::WasmHost::load(dir);
    horizon_client::mod_api::name_mods(&["wasm-plugins"]);
    let mods: Vec<Box<dyn horizon_client::mod_api::Hooks>> = vec![Box::new(host)];
    horizon_client::entry::main_with(mods.into());
}
