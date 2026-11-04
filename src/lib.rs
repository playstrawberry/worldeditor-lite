//! worldeditor-lite (first-party): selections with a wand or by position, region edits, a
//! clipboard with rotate and flip, undo and redo, `.schem` schematics, brushes and biome edits.
//!
//! Every edit runs in bounded steps over game ticks (`state`), a few milliseconds per tick for
//! all edits together, so a large edit never holds the tick. Nothing is read from or written to
//! a chunk that is not loaded (`chunks`).

pub mod biome;
pub mod chunks;
pub mod commands;
pub mod config;
pub mod events;
pub mod host;
pub mod lang;
pub mod state;

// The pure part lives in the shared library libs/block-edit; kept under the same paths here.
pub use block_edit::{clipboard, geom, gzip, history, job, nbt, pattern, schematic, transform};

use strawberry_plugin_api::logging::{LogLevel, log};
use strawberry_plugin_api::{
    Context, Plugin, PluginMetadata, Result, permissions, register_plugin,
};

use crate::config::{Config, DEFAULT_FILE};
use crate::history::History;

const CONFIG_FILE: &str = "data/config.txt";

fn load_config() -> Config {
    let text = match std::fs::read_to_string(CONFIG_FILE) {
        Ok(text) => text,
        Err(_) => {
            if std::fs::write(CONFIG_FILE, DEFAULT_FILE).is_err() {
                log(
                    LogLevel::Warn,
                    "worldeditor-lite: could not write the default config.txt",
                );
            }
            DEFAULT_FILE.to_owned()
        }
    };
    let (config, bad) = Config::parse(&text);
    for line in bad {
        log(
            LogLevel::Warn,
            &format!("worldeditor-lite: config.txt: kept the default for `{line}`"),
        );
    }
    config
}

struct WorldEditorLite;

impl Plugin for WorldEditorLite {
    fn new() -> Self {
        Self
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "worldeditor-lite".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: vec!["Strawberry Interactive LLC".into()],
            description: "Selections, region edits, clipboard, history, schematics and brushes."
                .into(),
            dependencies: Vec::new(),
            permissions: vec![
                permissions::FS_READ_DATA.into(),
                permissions::FS_WRITE_DATA.into(),
            ],
        }
    }

    fn on_load(&self, context: Context) -> Result<()> {
        let dir = context.get_data_folder();
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create the data folder: {e}"))?;
        let config = load_config();
        let air = host::air_states();
        {
            let mut s = state::state();
            s.history = History::new(
                config.history_bytes_per_player(),
                config.history_bytes_total(),
            );
            s.history.max_steps =
                usize::try_from(config.history_steps_per_player).unwrap_or(usize::MAX);
            s.config = config;
            s.air = air;
        }
        // Players already online (a reload): who may use the wand or a brush.
        let tools: Vec<u128> = context
            .get_server()
            .get_all_players()
            .iter()
            .filter(|p| events::may_use_tools(p))
            .map(state::raw_id)
            .collect();
        state::state().active.extend(tools);
        commands::register(&context)?;
        events::register(&context)?;
        Ok(())
    }

    fn on_unload(&self, _context: Context) -> Result<()> {
        // Edits still running stop here; what they changed stays changed. Chunk tickets go
        // back with the plugin.
        let mut s = state::state();
        s.jobs.clear();
        s.task = None;
        Ok(())
    }
}

register_plugin!(WorldEditorLite);
