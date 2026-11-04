//! The plugin's state and the runner that moves edits forward a few milliseconds per tick.
//!
//! - One repeating sync task exists only while an edit runs; with no edit there is no
//!   per-tick work.
//! - The state lock is never held across a host call: the runner takes the edits out, steps
//!   them, and puts them back.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use strawberry_plugin_api::text::TextComponent;
use strawberry_plugin_api::{Player, Server, scheduler};

use crate::chunks::{self, Tickets};
use crate::clipboard::Clipboard;
use crate::config::Config;
use crate::geom::{Cuboid, Pos};
use crate::history::History;
use crate::host::{HostBlocks, HostWorld};
use crate::job::{Budget, Job, Step};
use crate::lang;
use crate::pattern::Pattern;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrushShape {
    Sphere { radius: u32 },
    Cylinder { radius: u32, height: u32 },
}

pub struct Brush {
    pub item: String,
    pub shape: BrushShape,
    pub pattern: Pattern,
}

#[derive(Default)]
pub struct Session {
    /// The two positions, with the world each is in.
    pub pos: [Option<(String, Pos)>; 2],
    pub clipboard: Option<Arc<Clipboard>>,
    pub brush: Option<Brush>,
    /// The last wand click (which position, where, when), so one click is not counted twice.
    pub last_click: Option<(usize, Pos, Instant)>,
    /// When the player left, for a session kept after leaving (session-keep-minutes).
    pub left: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Edit,
    Copy,
    Cut,
    Undo,
    Redo,
    Biome,
}

pub struct Running {
    pub owner: String,
    pub kind: Kind,
    pub job: Job,
    pub started: Instant,
    pub blocked_since: Option<Instant>,
    pub last_message: u64,
    pub tickets: Tickets,
    pub cancel: bool,
    /// The world was unloaded while the edit ran: it stops, it never writes into a world loaded
    /// later under the same name.
    pub world_gone: bool,
    /// For dit-log: who and what (empty when the log is off), and the box the edit covers.
    pub actor: String,
    pub label: String,
    pub area: Option<Cuboid>,
}

impl Running {
    #[must_use]
    pub fn new(owner: &str, kind: Kind, job: Job) -> Self {
        Self {
            owner: owner.to_owned(),
            kind,
            job,
            started: Instant::now(),
            blocked_since: None,
            last_message: 0,
            tickets: Tickets::default(),
            cancel: false,
            world_gone: false,
            actor: String::new(),
            label: String::new(),
            area: None,
        }
    }

    #[must_use]
    pub fn percent(&self) -> u64 {
        (self.job.done * 100)
            .checked_div(self.job.total)
            .unwrap_or(100)
            .min(100)
    }
}

pub struct State {
    pub config: Config,
    pub sessions: HashMap<String, Session>,
    pub history: History,
    pub jobs: Vec<Running>,
    pub task: Option<u32>,
    pub tick: u64,
    pub air: Vec<u16>,
    pub entity_states: HashMap<u16, bool>,
    /// Online players who may use the wand or a brush, by raw UUID: filled at join from their
    /// permissions and whenever they run one of this plugin's commands. The click handlers stop
    /// at this set for everyone else, after one host call.
    pub active: HashSet<u128>,
    /// For dit-log: who runs the command now, and its text.
    pub current: Option<(String, String)>,
}

impl State {
    #[must_use]
    pub fn busy(&self, owner: &str) -> Option<u64> {
        self.jobs
            .iter()
            .find(|r| r.owner == owner)
            .map(Running::percent)
    }

    #[must_use]
    pub fn clipboard_blocks(&self) -> u64 {
        self.sessions
            .values()
            .filter_map(|s| s.clipboard.as_ref())
            .map(|c| c.volume())
            .sum()
    }

    pub fn session(&mut self, owner: &str) -> &mut Session {
        self.sessions.entry(owner.to_owned()).or_default()
    }

    /// Drops the sessions of players who left longer ago than session-keep-minutes.
    pub fn prune_sessions(&mut self) {
        let keep = std::time::Duration::from_secs(self.config.session_keep_minutes * 60);
        self.sessions
            .retain(|_, s| s.left.is_none_or(|left| left.elapsed() < keep));
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    let config = Config::default();
    Mutex::new(State {
        history: History::new(
            config.history_bytes_per_player(),
            config.history_bytes_total(),
        ),
        config,
        sessions: HashMap::new(),
        jobs: Vec::new(),
        task: None,
        tick: 0,
        air: Vec::new(),
        entity_states: HashMap::new(),
        active: HashSet::new(),
        current: None,
    })
});

pub fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The player's UUID as one number: one host call, no text.
#[must_use]
pub fn raw_id(player: &Player) -> u128 {
    let id = player.get_id();
    (u128::from(id.high) << 64) | u128::from(id.low)
}

#[must_use]
pub fn uuid_of(player: &Player) -> String {
    strawberry_plugin_api::uuid::to_string(player.get_id())
}

#[must_use]
pub fn text_for(player: &Player, key: &str, args: &[&str]) -> String {
    lang::text(lang::language(&player.get_locale()), key, args)
}

pub fn say(player: &Player, key: &str, args: &[&str]) {
    player.send_system_message(TextComponent::text(&text_for(player, key, args)), false);
}

fn bar(player: &Player, key: &str, args: &[&str]) {
    player.send_system_message(TextComponent::text(&text_for(player, key, args)), true);
}

fn player_by(server: &Server, owner: &str) -> Option<Player> {
    strawberry_plugin_api::uuid::parse(owner).and_then(|id| server.get_player_by_uuid(id))
}

/// Queues an edit. The caller has checked that the owner has none running.
pub fn start(mut running: Running) {
    let need_task = {
        let mut s = state();
        if let Some((actor, label)) = s.current.take() {
            running.actor = actor;
            running.label = label;
        }
        s.jobs.push(running);
        s.task.is_none()
    };
    if need_task {
        let id = scheduler::schedule_repeating_task(1, 1, |server| tick(&server));
        state().task = Some(id);
    }
}

/// The `edit-log` line of a finished edit: who, what, where, how many blocks. Never an address.
#[must_use]
pub fn edit_log_line(
    actor: &str,
    label: &str,
    world: &str,
    area: Option<Cuboid>,
    changed: u64,
    outcome: &str,
    seconds: f64,
) -> String {
    let clean =
        |s: &str, n: usize| -> String { s.chars().filter(|c| !c.is_control()).take(n).collect() };
    let place = area.map_or_else(String::new, |a| {
        format!(
            " from {},{},{} to {},{},{}",
            a.min.x, a.min.y, a.min.z, a.max.x, a.max.y, a.max.z
        )
    });
    format!(
        "worldeditor-lite: {} ran {} in {}{place}: {changed} blocks changed, {outcome}, {seconds:.1} s",
        clean(actor, 16),
        clean(label, 120),
        clean(world, 64),
    )
}

enum End {
    Done,
    Cancelled,
    Chunk((i32, i32)),
    WorldGone,
}

fn seconds(since: Instant) -> String {
    format!("{:.1}", since.elapsed().as_secs_f64())
}

/// One game tick of every running edit.
fn tick(server: &Server) {
    let (jobs, config, mut entity_states, now) = {
        let mut s = state();
        s.tick += 1;
        (
            std::mem::take(&mut s.jobs),
            s.config.clone(),
            std::mem::take(&mut s.entity_states),
            s.tick,
        )
    };
    if jobs.is_empty() {
        if let Some(id) = state().task.take() {
            scheduler::cancel_task(id);
        }
        return;
    }
    let n = jobs.len() as u64;
    let per_us = ((config.tick_budget_ms * 1000.0) as u64 / n).max(100);
    let per_blocks = (config.max_blocks_per_tick / n).max(16);
    let flags = HostWorld::flags(config.neighbor_updates);
    let mut keep = Vec::with_capacity(jobs.len());
    let mut finished = Vec::new();
    for mut r in jobs {
        if r.world_gone {
            finished.push((r, End::WorldGone));
            continue;
        }
        let Some(world) = server.get_world_by_name(&r.job.world) else {
            finished.push((r, End::WorldGone));
            continue;
        };
        if r.cancel {
            r.tickets.release_all(&world);
            finished.push((r, End::Cancelled));
            continue;
        }
        r.tickets.want(&world, &r.job.chunks_ahead(chunks::AHEAD));
        let dimension = world.get_dimension();
        let mut access = HostWorld {
            server,
            world: &world,
            dimension: &dimension,
            flags,
            entity_states: &mut entity_states,
            start: Instant::now(),
        };
        match r.job.step(
            &mut access,
            &HostBlocks,
            Budget {
                micros: per_us,
                blocks: per_blocks,
            },
        ) {
            Step::Done => {
                r.tickets.release_all(&world);
                finished.push((r, End::Done));
            }
            Step::More => {
                r.blocked_since = None;
                if now.saturating_sub(r.last_message) >= config.progress_ticks {
                    r.last_message = now;
                    if let Some(p) = player_by(server, &r.owner) {
                        let (pc, done, total) = (
                            r.percent().to_string(),
                            r.job.done.to_string(),
                            r.job.total.to_string(),
                        );
                        bar(&p, "progress", &[&pc, &done, &total]);
                    }
                }
                keep.push(r);
            }
            Step::Blocked(c) => {
                let since = *r.blocked_since.get_or_insert_with(Instant::now);
                if since.elapsed().as_secs() >= config.chunk_wait_seconds {
                    r.tickets.release_all(&world);
                    finished.push((r, End::Chunk(c)));
                } else {
                    if now.saturating_sub(r.last_message) >= config.progress_ticks {
                        r.last_message = now;
                        if let Some(p) = player_by(server, &r.owner) {
                            bar(&p, "waiting_chunk", &[&c.0.to_string(), &c.1.to_string()]);
                        }
                    }
                    keep.push(r);
                }
            }
        }
    }
    let mut messages = Vec::new();
    // `edit-log`, before the state lock is taken: no host call runs under it.
    if config.edit_log {
        for (r, end) in &finished {
            let outcome = match end {
                End::Done => "done",
                End::Cancelled => "cancelled",
                End::Chunk(_) => "stopped at an unloaded chunk",
                End::WorldGone => "stopped, the world was unloaded",
            };
            let line = edit_log_line(
                &r.actor,
                &r.label,
                &r.job.world,
                r.area,
                r.job.changed,
                outcome,
                r.started.elapsed().as_secs_f64(),
            );
            strawberry_plugin_api::logging::log(
                strawberry_plugin_api::logging::LogLevel::Info,
                &line,
            );
        }
    }
    let empty = {
        let mut s = state();
        entity_states.extend(s.entity_states.drain());
        s.entity_states = entity_states;
        keep.append(&mut s.jobs);
        s.jobs = keep;
        for (r, end) in finished {
            messages.extend(finish(&mut s, r, &end));
        }
        s.jobs.is_empty()
    };
    for (owner, key, args) in messages {
        if let Some(p) = player_by(server, &owner) {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            say(&p, key, &args);
        }
    }
    if empty && let Some(id) = state().task.take() {
        scheduler::cancel_task(id);
    }
}

type Message = (String, &'static str, Vec<String>);

/// Keeps what a finished edit leaves (history, clipboard) and says how it ended.
fn finish(s: &mut State, r: Running, end: &End) -> Vec<Message> {
    let Running {
        owner,
        kind,
        job,
        started,
        ..
    } = r;
    let changed = job.changed.to_string();
    let total = job.total;
    let mut out: Vec<Message> = Vec::new();
    let record = match kind {
        Kind::Copy | Kind::Cut => job.take_clipboard().map(|(clipboard, record)| {
            if matches!(end, End::Done) {
                s.session(&owner).clipboard = Some(Arc::new(clipboard));
            }
            record
        }),
        Kind::Undo | Kind::Redo => None,
        Kind::Edit | Kind::Biome => Some(job.record),
    };
    if let Some(record) = record
        && !record.is_empty()
        && !s.history.push(&owner, record)
    {
        out.push((owner.clone(), "not_undoable", Vec::new()));
    }
    let line: Message = match end {
        End::Done => match kind {
            Kind::Edit => (owner.clone(), "done", vec![changed, seconds(started)]),
            Kind::Biome => (owner.clone(), "done_biome", vec![changed, seconds(started)]),
            Kind::Copy => (owner.clone(), "done_copy", vec![total.to_string()]),
            Kind::Cut => (owner.clone(), "done_cut", vec![total.to_string()]),
            Kind::Undo => (owner.clone(), "done_undo", vec![changed]),
            Kind::Redo => (owner.clone(), "done_redo", vec![changed]),
        },
        End::Cancelled => (owner.clone(), "cancelled", vec![changed]),
        End::Chunk((x, z)) => (
            owner.clone(),
            "stopped_chunk",
            vec![x.to_string(), z.to_string(), changed],
        ),
        End::WorldGone => (owner.clone(), "stopped_world", Vec::new()),
    };
    out.insert(0, line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_edit_log_line_names_who_what_where_and_how_many() {
        let area = Cuboid::from_corners(Pos::new(0, 190, 0), Pos::new(9, 199, 9));
        let line = edit_log_line(
            "Alex",
            "//set stone",
            "world",
            Some(area),
            1000,
            "done",
            0.5,
        );
        assert_eq!(
            line,
            "worldeditor-lite: Alex ran //set stone in world from 0,190,0 to 9,199,9: 1000 blocks changed, done, 0.5 s"
        );
        let odd = edit_log_line("Al\nex", &"x".repeat(500), "w\r", None, 0, "cancelled", 0.0);
        assert!(!odd.contains('\n') && !odd.contains('\r'));
        assert!(odd.len() < 220);
    }

    fn empty_state(keep_minutes: u64) -> State {
        let config = Config {
            session_keep_minutes: keep_minutes,
            ..Config::default()
        };
        State {
            history: History::new(1 << 20, 1 << 20),
            config,
            sessions: HashMap::new(),
            jobs: Vec::new(),
            task: None,
            tick: 0,
            air: Vec::new(),
            entity_states: HashMap::new(),
            active: HashSet::new(),
            current: None,
        }
    }

    #[test]
    fn sessions_of_players_who_left_are_kept_for_the_set_minutes() {
        let mut s = empty_state(10);
        s.session("online");
        s.session("left-now").left = Some(Instant::now());
        let long_ago = Instant::now().checked_sub(std::time::Duration::from_secs(11 * 60));
        if let Some(t) = long_ago {
            s.session("left-long-ago").left = Some(t);
        }
        s.prune_sessions();
        assert!(s.sessions.contains_key("online"));
        assert!(s.sessions.contains_key("left-now"));
        assert!(!s.sessions.contains_key("left-long-ago"));
        let mut s = empty_state(0);
        s.session("left-now").left = Some(Instant::now());
        s.prune_sessions();
        assert!(s.sessions.is_empty(), "0 minutes keeps nothing");
    }
}
