//! The plugin host against real plugins (`plugins/examples`, built to WebAssembly here) and a
//! test double of the game.
use super::*;
use std::process::Command;

/// Build an example plugin and return its `.wasm`.
fn build_example(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = root.join("plugins/examples").join(name).join("Cargo.toml");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
        .join("plugins");
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--manifest-path",
        ])
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target)
        .status()
        .expect("running cargo");
    assert!(status.success(), "building {name}");
    target
        .join("wasm32-unknown-unknown/release")
        .join(format!("{name}.wasm"))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("horizon-plugins-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const STEVE: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;

/// What the plugins did to the "game".
#[derive(Default)]
struct TestGame {
    told: Vec<(u128, String)>,
    feedback: Vec<String>,
    teleports: Vec<(u128, String, [f64; 3])>,
    action_bars: Vec<(u128, String)>,
    refreshed: usize,
}

impl Host for TestGame {
    fn tell(&mut self, uuid: u128, line: Text) -> bool {
        self.told.push((uuid, line.plain()));
        true
    }
    fn broadcast(&self, line: Text) {
        // `&self`: kept through interior mutability in a real game; here, the next best thing.
        BROADCAST.with(|b| b.borrow_mut().push(line.plain()));
    }
    fn feedback(&mut self, line: Text) {
        self.feedback.push(line.plain());
    }
    fn action_bar(&mut self, uuid: u128, line: Text) -> bool {
        self.action_bars.push((uuid, line.plain()));
        true
    }
    fn teleport_player(
        &mut self,
        uuid: u128,
        dimension: &str,
        pos: [f64; 3],
        _yaw: f32,
        _pitch: f32,
    ) -> bool {
        self.teleports.push((uuid, dimension.to_owned(), pos));
        true
    }
    fn refresh_commands(&mut self) {
        self.refreshed += 1;
    }
    fn server_players(&self) -> Vec<NativeServerPlayer> {
        Vec::new()
    }
    fn resend_commands(&mut self, _player: u128) -> bool {
        true
    }
    fn has_permission(&self, _player: u128, _node: &str, _default_level: u8) -> bool {
        false
    }
    fn acting_player(&self) -> Option<NativeServerPlayer> {
        None
    }
}

thread_local! {
    static BROADCAST: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn steve() -> Value {
    json!({
        "uuid": conv::uuid_string(STEVE), "name": "Steve", "permission": 0,
        "dimension": "minecraft:overworld", "position": [10.5, 70.0, -3.5],
        "game_mode": 0, "health": 20.0, "yaw": 90.0, "pitch": 0.0,
    })
}

fn command(host: &WasmHost, game: &mut TestGame, line: &str) -> Vec<String> {
    host.run_command(HostRef::Mut(game), Some(steve()), line)
        .expect("a plugin command")
}

#[test]
fn the_starter_plugin_runs_on_the_hooks() {
    let wasm = build_example("starter");
    let dir = scratch("starter");
    std::fs::copy(&wasm, dir.join("starter.wasm")).unwrap();
    let host = WasmHost::load(&dir);
    assert_eq!(host.plugins(), [("starter".to_owned(), "1.0.0".to_owned())]);
    let mods: Vec<Box<dyn Hooks>> = vec![Box::new(WasmHost(host.0.clone()))];
    let mut game = TestGame::default();

    // The first tick enables it: it writes its default config and schedules its announcement.
    host.tick_start(&mut game);
    assert!(dir.join("starter/config.toml").is_file());
    assert_eq!(host.0.tasks.lock().unwrap().len(), 1);

    // A player joins: the hook reaches the plugin, which tells them the welcome line.
    dispatch_player_logged_in(&mut game, &mods, STEVE, "Steve");
    assert_eq!(game.told, [(STEVE, "ようこそ、Steveさん！".to_owned())]);

    // Its commands register with the server's tree.
    let mut commands = Vec::new();
    dispatch_register_commands(&mut game, &mods, &mut commands);
    let usages: Vec<_> = commands.iter().map(|c| c.usage.as_str()).collect();
    assert!(usages.contains(&"sethome [name:word]"), "{usages:?}");
    assert!(usages.contains(&"plugins"));
    assert_eq!(
        commands
            .iter()
            .find(|c| c.usage == "homes")
            .unwrap()
            .permission
            .as_deref(),
        Some("starter.home")
    );

    // Homes: saved to its data folder, and back.
    assert_eq!(
        command(&host, &mut game, "sethome base"),
        ["§aホーム「base」を保存しました。"]
    );
    let saved = std::fs::read_to_string(dir.join("starter/homes.json")).unwrap();
    assert!(saved.contains("\"base\""), "{saved}");
    assert_eq!(command(&host, &mut game, "homes"), ["§eホーム: §fbase"]);
    assert_eq!(
        command(&host, &mut game, "home base"),
        ["§aホーム「base」へ移動しました。"]
    );
    assert_eq!(
        game.teleports,
        [(STEVE, "minecraft:overworld".to_owned(), [10.5, 70.0, -3.5])]
    );
    assert_eq!(
        command(&host, &mut game, "home nowhere")[0],
        "§cホーム「nowhere」はありません。/homes で一覧を見られます。"
    );

    // A cancellable hook: no acting player in the test game, so nothing is protected.
    let mut canceled = false;
    dispatch_destroy_block(&mut game, &mods, [3, 64, 3], BlockStateId(1), &mut canceled);
    assert!(!canceled);

    // Scheduled task: the announcement fires after its delay (300 s = 6000 ticks).
    for _ in 0..6000 {
        host.tick_start(&mut game);
    }
    BROADCAST.with(|b| assert_eq!(*b.borrow(), ["/sethome でホームを保存、/home で戻れます"]));

    // The host's own command.
    let listed = host
        .run_command(HostRef::Mut(&mut game), None, "plugins")
        .unwrap();
    assert_eq!(listed, ["§ePlugins (1):", "§f- starter §71.0.0"]);

    // A reload keeps its data: a fresh instance reads homes.json again.
    host.run_command(HostRef::Mut(&mut game), None, "plugins reload starter")
        .unwrap();
    host.tick_start(&mut game);
    assert_eq!(command(&host, &mut game, "homes"), ["§eホーム: §fbase"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plugin_that_never_returns_is_stopped_and_the_server_goes_on() {
    let wasm = build_example("runaway");
    let dir = scratch("runaway");
    std::fs::copy(&wasm, dir.join("runaway.wasm")).unwrap();
    let host = WasmHost::load(&dir);
    let mut game = TestGame::default();
    let started = std::time::Instant::now();
    let lines = host
        .run_command(HostRef::Mut(&mut game), None, "spin")
        .unwrap();
    assert!(lines.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "stopped after {:?}",
        started.elapsed()
    );
    let plugin = host.0.plugins.read().unwrap()[0].clone();
    assert_eq!(plugin.failures.load(Ordering::Relaxed), 1);
    // A panic is a trap too; after enough in a row the plugin is turned off.
    for _ in 0..MAX_FAILURES {
        host.run_command(HostRef::Mut(&mut game), None, "boom");
    }
    assert!(plugin.off.load(Ordering::Relaxed));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plugins_hear_every_bridged_hook_by_name() {
    // Hooks with and without helpers, listening, cancelling and replacing ones.
    for hook in [
        "destroy_block",
        "player_logged_in",
        "player_chat",
        "permission_check",
        "living_incoming_damage_v2",
    ] {
        assert!(HOOK_NAMES.contains(&hook), "{hook} is not bridged");
    }
    for helper in [
        "tell",
        "teleport_player",
        "set_block",
        "run_command",
        "kick",
        "has_permission",
    ] {
        assert!(HOST_FUNCTIONS.contains(&helper), "{helper} is not callable");
    }
}
