//! `config.txt` in the plugin's data folder: `key = value` lines, `#` comments. A missing file
//! is written with the defaults; an unknown key or a bad value keeps the default and is
//! reported once in the log.

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The most blocks one edit may cover.
    pub max_blocks_per_edit: u64,
    /// Undo memory per player and for the whole server, in MiB.
    pub history_mib_per_player: u64,
    pub history_mib_total: u64,
    /// The most edits each player keeps to undo; 0 is no step limit.
    pub history_steps_per_player: u64,
    /// Minutes a player's selection, clipboard and brush are kept after leaving; 0 drops them
    /// at once.
    pub session_keep_minutes: u64,
    /// One console line per finished edit: who, what, where, how many blocks.
    pub edit_log: bool,
    /// The most blocks one clipboard may hold, and all clipboards together.
    pub clipboard_max_blocks: u64,
    pub clipboard_total_blocks: u64,
    /// Milliseconds of each game tick all running edits may use together.
    pub tick_budget_ms: f64,
    /// The most blocks all edits may touch in one tick.
    pub max_blocks_per_tick: u64,
    /// Ticks between two progress messages.
    pub progress_ticks: u64,
    /// Seconds an edit waits for a chunk that is not loaded before it stops.
    pub chunk_wait_seconds: u64,
    /// Whether edits wake neighbouring blocks (sand falls, water flows), as a placed block does.
    pub neighbor_updates: bool,
    pub brush_max_radius: u32,
    /// The largest schematic file read, in KiB.
    pub schematic_max_kib: u64,
    pub wand_item: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_blocks_per_edit: 500_000,
            history_mib_per_player: 8,
            history_mib_total: 24,
            history_steps_per_player: 0,
            session_keep_minutes: 0,
            edit_log: false,
            clipboard_max_blocks: 1_000_000,
            clipboard_total_blocks: 4_000_000,
            tick_budget_ms: 3.0,
            max_blocks_per_tick: 20_000,
            progress_ticks: 20,
            chunk_wait_seconds: 30,
            neighbor_updates: true,
            brush_max_radius: 8,
            schematic_max_kib: 8192,
            wand_item: "minecraft:wooden_axe".to_owned(),
        }
    }
}

pub const DEFAULT_FILE: &str = "\
# worldeditor-lite settings. Delete this file to get the defaults back.
# The most blocks one edit may cover.
max-blocks-per-edit = 500000
# Undo memory in MiB: per player, and for every player together.
history-mib-per-player = 8
history-mib-total = 24
# The most edits each player can undo; 0 is no step limit (the memory above still bounds it).
history-steps-per-player = 0
# Minutes a player's selection, clipboard and brush are kept after leaving; 0 drops them at once.
session-keep-minutes = 0
# One console line per finished edit (player name, command, world, box, blocks changed).
edit-log = false
# The most blocks one clipboard may hold, and every clipboard together.
clipboard-max-blocks = 1000000
clipboard-total-blocks = 4000000
# Milliseconds of each game tick (50 ms) that all running edits may use together.
tick-budget-ms = 3
# The most blocks all edits may touch in one tick.
max-blocks-per-tick = 20000
# Ticks between two progress messages (20 ticks = 1 second).
progress-ticks = 20
# Seconds an edit waits for an unloaded chunk before it stops.
chunk-wait-seconds = 30
# Whether edited blocks update their neighbours (sand falls, water flows), as placed blocks do.
neighbor-updates = true
# The largest brush radius.
brush-max-radius = 8
# The largest schematic file read, in KiB.
schematic-max-kib = 8192
# The selection wand.
wand-item = minecraft:wooden_axe
";

fn flag(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn number(value: &str, min: u64, max: u64) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|v| (min..=max).contains(v))
}

impl Config {
    /// Reads the file text; returns the settings and the lines that were not taken.
    #[must_use]
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut c = Self::default();
        let mut bad = Vec::new();
        for line in text.lines().take(1000) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bad.push(line.chars().take(80).collect());
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            let ok = match key {
                "max-blocks-per-edit" => {
                    number(value, 1, 50_000_000).map(|v| c.max_blocks_per_edit = v)
                }
                "history-mib-per-player" => {
                    number(value, 0, 1024).map(|v| c.history_mib_per_player = v)
                }
                "history-mib-total" => number(value, 0, 1024).map(|v| c.history_mib_total = v),
                "clipboard-max-blocks" => {
                    number(value, 1, 50_000_000).map(|v| c.clipboard_max_blocks = v)
                }
                "clipboard-total-blocks" => {
                    number(value, 1, 200_000_000).map(|v| c.clipboard_total_blocks = v)
                }
                "tick-budget-ms" => value
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && (0.1..=40.0).contains(v))
                    .map(|v| c.tick_budget_ms = v),
                "max-blocks-per-tick" => {
                    number(value, 1, 1_000_000).map(|v| c.max_blocks_per_tick = v)
                }
                "progress-ticks" => number(value, 1, 12_000).map(|v| c.progress_ticks = v),
                "chunk-wait-seconds" => number(value, 0, 3600).map(|v| c.chunk_wait_seconds = v),
                "neighbor-updates" => flag(value).map(|v| c.neighbor_updates = v),
                "edit-log" => flag(value).map(|v| c.edit_log = v),
                "history-steps-per-player" => {
                    number(value, 0, 100_000).map(|v| c.history_steps_per_player = v)
                }
                "session-keep-minutes" => {
                    number(value, 0, 1440).map(|v| c.session_keep_minutes = v)
                }
                "brush-max-radius" => number(value, 1, 64).map(|v| c.brush_max_radius = v as u32),
                "schematic-max-kib" => number(value, 1, 262_144).map(|v| c.schematic_max_kib = v),
                "wand-item" => {
                    let v = value.to_ascii_lowercase();
                    (!v.is_empty()
                        && v.len() <= 100
                        && v.chars().all(|ch| {
                            ch.is_ascii_lowercase()
                                || ch.is_ascii_digit()
                                || matches!(ch, '_' | ':' | '.' | '-' | '/')
                        }))
                    .then(|| {
                        c.wand_item = if v.contains(':') {
                            v
                        } else {
                            format!("minecraft:{v}")
                        }
                    })
                }
                _ => None,
            };
            if ok.is_none() {
                bad.push(line.chars().take(80).collect());
            }
        }
        (c, bad)
    }

    #[must_use]
    pub const fn history_bytes_per_player(&self) -> usize {
        (self.history_mib_per_player as usize) << 20
    }

    #[must_use]
    pub const fn history_bytes_total(&self) -> usize {
        (self.history_mib_total as usize) << 20
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_file_gives_the_defaults() {
        let (c, bad) = Config::parse(DEFAULT_FILE);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn bad_lines_keep_the_default() {
        let (c, bad) = Config::parse(
            "max-blocks-per-edit = -5\ntick-budget-ms = NaN\nwand-item = stick\nnope = 1\nneighbor-updates = false\nbroken line\nhistory-steps-per-player = 15\nsession-keep-minutes = 10\nedit-log = true\nedit-log = maybe",
        );
        assert_eq!(c.max_blocks_per_edit, Config::default().max_blocks_per_edit);
        assert!((c.tick_budget_ms - 3.0).abs() < f64::EPSILON);
        assert_eq!(c.wand_item, "minecraft:stick");
        assert!(!c.neighbor_updates);
        assert_eq!(c.history_steps_per_player, 15);
        assert_eq!(c.session_keep_minutes, 10);
        assert!(c.edit_log);
        assert_eq!(bad.len(), 5);
    }

    #[test]
    fn the_largest_edit_can_be_fifty_million_blocks() {
        let (c, bad) = Config::parse("max-blocks-per-edit = 50000000");
        assert!(bad.is_empty());
        assert_eq!(c.max_blocks_per_edit, 50_000_000);
    }
}
