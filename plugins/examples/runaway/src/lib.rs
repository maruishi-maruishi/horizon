//! A plugin that never returns from `/spin`, and panics on `/boom`: the host's tests check that it
//! is stopped, and that the server goes on.
use horizon_plugin_sdk::prelude::*;

#[derive(Default)]
struct Runaway;

impl Plugin for Runaway {
    fn info(&self) -> Info {
        Info::new("runaway", "0.1.0")
            .command("spin")
            .command("boom")
    }
    fn on_event(&mut self, event: &Event) -> Reply {
        match event.command().map(|c| c.label) {
            Some(label) if label == "spin" => {
                let mut n: u64 = 0;
                loop {
                    n = std::hint::black_box(n.wrapping_add(1));
                }
            }
            Some(label) if label == "boom" => panic!("boom"),
            _ => Reply::none(),
        }
    }
}

register!(Runaway::default());
