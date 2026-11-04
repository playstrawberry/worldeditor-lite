//! The commands. Each has its own permission, `worldeditor-lite:<name>`, given to operators
//! (level 2) by default. The region commands start with a second slash (`//set`).
//!
//! A command only checks its input and queues an edit; the edit itself runs in bounded steps
//! over the next ticks (`state`). File work for schematics runs in async tasks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use strawberry_plugin_api::command::{
    Arg, ArgumentType, Command, CommandError, CommandNode, CommandSender, ConsumedArgs, StringType,
};
use strawberry_plugin_api::commands::CommandHandler;
use strawberry_plugin_api::common::Hand;
use strawberry_plugin_api::permission::{Permission, PermissionDefault, PermissionLevel};
use strawberry_plugin_api::scheduler::SchedulerExt;
use strawberry_plugin_api::text::TextComponent;
use strawberry_plugin_api::{Context, ItemStack, Player, Result, Server, World};

use crate::biome;
use crate::clipboard::Clipboard;
use crate::geom::{Cuboid, Pos, Shape};
use crate::history::CHANGE_BYTES;
use crate::host::HostBlocks;
use crate::job::{Job, Work, biome_pieces};
use crate::pattern::{Mask, Pattern, PatternError};
use crate::schematic;
use crate::state::{self, Brush, BrushShape, Kind, Running, raw_id, say, state, text_for, uuid_of};
use crate::transform::Transform;

pub const PLUGIN: &str = "worldeditor-lite";
pub const SCHEMATIC_DIR: &str = "data/schematics";

type Fail = (&'static str, Vec<String>);
type Res = std::result::Result<i32, Fail>;

fn fail(key: &'static str, args: &[&str]) -> Fail {
    (key, args.iter().map(|s| (*s).to_owned()).collect())
}

#[must_use]
pub fn node(name: &str) -> String {
    format!("{PLUGIN}:{name}")
}

struct Cmd {
    name: &'static str,
    run: fn(&Player, &Server, &str) -> Res,
    with_args: bool,
}

impl CommandHandler for Cmd {
    fn handle(
        &self,
        sender: CommandSender,
        server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let Some(player) = sender.as_player() else {
            let text = crate::lang::text("en", "player_only", &[]);
            return Err(CommandError::CommandFailed(TextComponent::text(&text)));
        };
        let text = if self.with_args {
            match args.get_value("args") {
                Arg::Simple(s) | Arg::Msg(s) => s,
                _ => String::new(),
            }
        } else {
            String::new()
        };
        if text.len() > 1024 {
            return Err(CommandError::CommandFailed(TextComponent::text(&text_for(
                &player,
                "pattern_too_long",
                &[],
            ))));
        }
        // The server let the command through, so the player holds this plugin's permission:
        // from now on the click handlers look at their clicks.
        let log = state().config.edit_log;
        let actor = log.then(|| player.get_name());
        {
            let mut s = state();
            s.active.insert(raw_id(&player));
            s.current = actor.map(|a| {
                (
                    a,
                    format!("/{} {}", self.name, text.trim())
                        .trim_end()
                        .to_owned(),
                )
            });
        }
        let result = (self.run)(&player, &server, text.trim());
        state().current = None;
        match result {
            Ok(n) => Ok(n),
            Err((key, a)) => {
                let a: Vec<&str> = a.iter().map(String::as_str).collect();
                Err(CommandError::CommandFailed(TextComponent::text(&text_for(
                    &player, key, &a,
                ))))
            }
        }
    }
}

struct Spec {
    names: &'static [&'static str],
    node: &'static str,
    description: &'static str,
    run: fn(&Player, &Server, &str) -> Res,
}

const COMMANDS: &[Spec] = &[
    Spec {
        names: &["/wand"],
        node: "wand",
        description: "Gives the selection wand.",
        run: wand,
    },
    Spec {
        names: &["/pos1"],
        node: "pos1",
        description: "Sets the first position.",
        run: pos1,
    },
    Spec {
        names: &["/pos2"],
        node: "pos2",
        description: "Sets the second position.",
        run: pos2,
    },
    Spec {
        names: &["/hpos1"],
        node: "hpos1",
        description: "Sets the first position to the block you look at.",
        run: hpos1,
    },
    Spec {
        names: &["/hpos2"],
        node: "hpos2",
        description: "Sets the second position to the block you look at.",
        run: hpos2,
    },
    Spec {
        names: &["/desel"],
        node: "desel",
        description: "Clears the selection.",
        run: desel,
    },
    Spec {
        names: &["/size"],
        node: "size",
        description: "Shows the size of the selection.",
        run: size,
    },
    Spec {
        names: &["/set"],
        node: "set",
        description: "Fills the selection with a pattern.",
        run: set,
    },
    Spec {
        names: &["/replace"],
        node: "replace",
        description: "Replaces blocks in the selection.",
        run: replace,
    },
    Spec {
        names: &["/walls"],
        node: "walls",
        description: "Builds the four walls of the selection.",
        run: walls,
    },
    Spec {
        names: &["/outline", "/faces"],
        node: "outline",
        description: "Builds the six faces of the selection.",
        run: outline,
    },
    Spec {
        names: &["/copy"],
        node: "copy",
        description: "Copies the selection.",
        run: copy,
    },
    Spec {
        names: &["/cut"],
        node: "cut",
        description: "Copies the selection and clears it.",
        run: cut,
    },
    Spec {
        names: &["/paste"],
        node: "paste",
        description: "Pastes the clipboard.",
        run: paste,
    },
    Spec {
        names: &["/rotate"],
        node: "rotate",
        description: "Turns the clipboard.",
        run: rotate,
    },
    Spec {
        names: &["/flip"],
        node: "flip",
        description: "Mirrors the clipboard.",
        run: flip,
    },
    Spec {
        names: &["/undo"],
        node: "undo",
        description: "Undoes your last edit.",
        run: undo,
    },
    Spec {
        names: &["/redo"],
        node: "redo",
        description: "Does your last undone edit again.",
        run: redo,
    },
    Spec {
        names: &["/clearhistory"],
        node: "clearhistory",
        description: "Forgets your edit history.",
        run: clear_history,
    },
    Spec {
        names: &["/cancel"],
        node: "cancel",
        description: "Stops your running edit.",
        run: cancel,
    },
    Spec {
        names: &["/schem", "/schematic"],
        node: "schem",
        description: "Saves, loads, lists and deletes schematics.",
        run: schem,
    },
    Spec {
        names: &["brush", "br"],
        node: "brush",
        description: "Binds a brush to the item in your hand.",
        run: brush,
    },
    Spec {
        names: &["/setbiome"],
        node: "setbiome",
        description: "Sets the biome of the selection.",
        run: set_biome,
    },
];

const SCHEM_NODES: [&str; 4] = ["schem.save", "schem.load", "schem.list", "schem.delete"];

/// Without this node an edit moves block states only: no copy, paste, undo, redo or schematic
/// carries a container's contents or a sign's text (no way to multiply what a chest holds).
pub const ENTITY_NODE: &str = "block-entity-data";

pub fn register(context: &Context) -> Result<()> {
    let permission = |name: &str, description: &str| Permission {
        node: node(name),
        description: description.to_owned(),
        default: PermissionDefault::Op(PermissionLevel::Two),
        children: Vec::new(),
    };
    for spec in COMMANDS {
        context.register_permission(&permission(spec.node, spec.description))?;
        let names: Vec<String> = spec.names.iter().map(|s| (*s).to_owned()).collect();
        let command = Command::new(&names, spec.description)
            .execute(Cmd {
                name: spec.names[0],
                run: spec.run,
                with_args: false,
            })
            .then(
                CommandNode::argument("args", &ArgumentType::String(StringType::Greedy)).execute(
                    Cmd {
                        name: spec.names[0],
                        run: spec.run,
                        with_args: true,
                    },
                ),
            );
        context.register_command(command, &node(spec.node));
    }
    for sub in SCHEM_NODES {
        context.register_permission(&permission(sub, "One //schem action."))?;
    }
    context.register_permission(&permission(
        ENTITY_NODE,
        "Edits carry block entity data: container contents, sign text.",
    ))?;
    Ok(())
}

// ---- helpers ----

#[must_use]
pub fn block_pos(player: &Player) -> Pos {
    let (x, y, z) = player.get_position();
    Pos::new(x.floor() as i32, y.floor() as i32, z.floor() as i32)
}

/// The lowest and highest block of a world.
#[must_use]
pub fn bounds(world: &World) -> (i32, i32) {
    let min = world.get_min_y();
    let height = if world.get_dimension() == "minecraft:overworld" {
        384
    } else {
        256
    };
    (min, min + height - 1)
}

fn show(p: Pos) -> String {
    format!("{}, {}, {}", p.x, p.y, p.z)
}

fn coords(text: &str) -> Option<Pos> {
    let parts: Vec<i32> = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    match parts.as_slice() {
        [x, y, z] if x.abs() <= 30_000_000 && z.abs() <= 30_000_000 => Some(Pos::new(*x, *y, *z)),
        _ => None,
    }
}

fn pattern_fail(e: PatternError) -> Fail {
    match e {
        PatternError::Empty => fail("pattern_empty", &[]),
        PatternError::BadSyntax(t) => fail("bad_pattern", &[&t]),
        PatternError::UnknownBlock(n) => fail("unknown_block", &[&n]),
        PatternError::BadProperty(p) => fail("bad_property", &[&p]),
        PatternError::TooMany => fail("pattern_too_long", &[]),
    }
}

fn seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x5EED, |d| d.as_nanos() as u64)
}

/// Sets position `which` (0 or 1). `click` is a wand click: the same click seen twice within
/// half a second (interact and break) is reported once.
pub fn set_position(player: &Player, which: usize, p: Pos, click: bool) -> Res {
    let world = player.get_world();
    let (lo, hi) = bounds(&world);
    if p.y < lo || p.y > hi {
        return Err(fail("out_of_world", &[]));
    }
    let name = world.get_name();
    let owner = uuid_of(player);
    let volume = {
        let mut s = state();
        let session = s.session(&owner);
        if click
            && let Some((w, last, at)) = session.last_click
            && w == which
            && last == p
            && at.elapsed().as_millis() < 500
        {
            return Ok(0);
        }
        if click {
            session.last_click = Some((which, p, std::time::Instant::now()));
        }
        session.pos[which] = Some((name.clone(), p));
        session.pos[1 - which]
            .as_ref()
            .filter(|(w, _)| *w == name)
            .map(|(_, other)| Cuboid::from_corners(p, *other).volume())
    };
    let shown = show(p);
    match (which, volume) {
        (0, Some(v)) => say(player, "pos1_set", &[&shown, &v.to_string()]),
        (_, Some(v)) => say(player, "pos2_set", &[&shown, &v.to_string()]),
        (0, None) => say(player, "pos1_set_alone", &[&shown]),
        _ => say(player, "pos2_set_alone", &[&shown]),
    }
    Ok(1)
}

/// The selection in the player's world, cut to the world's height.
fn selection(player: &Player) -> std::result::Result<(String, Cuboid), Fail> {
    let world = player.get_world();
    let name = world.get_name();
    let owner = uuid_of(player);
    let (a, b) = {
        let mut s = state();
        let session = s.session(&owner);
        (session.pos[0].clone(), session.pos[1].clone())
    };
    let (Some((wa, a)), Some((wb, b))) = (a, b) else {
        return Err(fail("no_selection", &[]));
    };
    if wa != name || wb != name {
        return Err(fail("selection_other_world", &[]));
    }
    let (lo, hi) = bounds(&world);
    let c = Cuboid::from_corners(a, b)
        .clamp_y(lo, hi)
        .ok_or_else(|| fail("out_of_world", &[]))?;
    Ok((name, c))
}

/// The most blocks one edit may change: the setting, and what fits in the player's undo memory.
fn edit_limit() -> u64 {
    let s = state();
    s.config
        .max_blocks_per_edit
        .min((s.history.per_player_bytes / CHANGE_BYTES) as u64)
}

fn check_size(volume: u64) -> std::result::Result<(), Fail> {
    let limit = edit_limit();
    if volume > limit {
        return Err(fail("too_big", &[&volume.to_string(), &limit.to_string()]));
    }
    Ok(())
}

fn check_busy(owner: &str) -> std::result::Result<(), Fail> {
    if let Some(percent) = state().busy(owner) {
        return Err(fail("busy", &[&percent.to_string()]));
    }
    Ok(())
}

/// Checks size and busy, then queues the edit. The parts are checked before the job is built,
/// so a huge selection never allocates anything.
fn queue(player: &Player, kind: Kind, world: &str, parts: &[Cuboid], work: Work) -> Res {
    let owner = uuid_of(player);
    let volume: u64 = parts.iter().map(Cuboid::volume).sum();
    if !matches!(kind, Kind::Copy) {
        check_size(volume)?;
    }
    check_busy(&owner)?;
    let mut job = Job::new(world, parts, work);
    job.entity_data = player.has_permission(&node(ENTITY_NODE));
    let total = job.total;
    if total > state().config.max_blocks_per_tick {
        say(player, "edit_started", &[&total.to_string()]);
    }
    let mut running = Running::new(&owner, kind, job);
    running.area = parts.iter().copied().reduce(|a, b| {
        Cuboid::from_corners(
            Pos::new(
                a.min.x.min(b.min.x),
                a.min.y.min(b.min.y),
                a.min.z.min(b.min.z),
            ),
            Pos::new(
                a.max.x.max(b.max.x),
                a.max.y.max(b.max.y),
                a.max.z.max(b.max.z),
            ),
        )
    });
    state::start(running);
    Ok(i32::try_from(total).unwrap_or(i32::MAX))
}

fn fill_work(pattern: Pattern, mask: Option<Mask>) -> Work {
    Work::Fill {
        pattern,
        mask,
        shape: Shape::All,
        seed: seed(),
    }
}

// ---- selection ----

fn wand(player: &Player, _server: &Server, _args: &str) -> Res {
    let item = state().config.wand_item.clone();
    let inventory = player.get_inventory().as_inventory();
    for slot in 0..36u32 {
        let empty = inventory.get_item(slot).is_none_or(|i| {
            i.get_count() == 0 || crate::events::item_key(&i.get_registry_key()) == "minecraft:air"
        });
        if empty {
            inventory.set_item(slot, Some(ItemStack::new(&item, 1)));
            let shown = item.strip_prefix("minecraft:").unwrap_or(&item).to_owned();
            say(player, "wand_given", &[&shown]);
            return Ok(1);
        }
    }
    Err(fail("wand_full", &[]))
}

fn pos_from(player: &Player, args: &str) -> std::result::Result<Pos, Fail> {
    if args.is_empty() {
        return Ok(block_pos(player));
    }
    coords(args).ok_or_else(|| fail("bad_coordinates", &[]))
}

fn pos1(player: &Player, _server: &Server, args: &str) -> Res {
    set_position(player, 0, pos_from(player, args)?, false)
}

fn pos2(player: &Player, _server: &Server, args: &str) -> Res {
    set_position(player, 1, pos_from(player, args)?, false)
}

fn target(player: &Player) -> std::result::Result<Pos, Fail> {
    let (x, y, z) = player
        .get_target_block(100)
        .ok_or_else(|| fail("no_target", &[]))?;
    Ok(Pos::new(
        x.floor() as i32,
        y.floor() as i32,
        z.floor() as i32,
    ))
}

fn hpos1(player: &Player, _server: &Server, _args: &str) -> Res {
    set_position(player, 0, target(player)?, false)
}

fn hpos2(player: &Player, _server: &Server, _args: &str) -> Res {
    set_position(player, 1, target(player)?, false)
}

fn desel(player: &Player, _server: &Server, _args: &str) -> Res {
    let owner = uuid_of(player);
    state().session(&owner).pos = [None, None];
    say(player, "selection_cleared", &[]);
    Ok(1)
}

fn size(player: &Player, _server: &Server, _args: &str) -> Res {
    let (_, c) = selection(player)?;
    let (x, y, z) = c.size();
    say(
        player,
        "size",
        &[
            &x.to_string(),
            &y.to_string(),
            &z.to_string(),
            &c.volume().to_string(),
        ],
    );
    Ok(i32::try_from(c.volume()).unwrap_or(i32::MAX))
}

// ---- region edits ----

fn set(player: &Player, _server: &Server, args: &str) -> Res {
    if args.is_empty() {
        return Err(fail("usage_set", &[]));
    }
    let (world, c) = selection(player)?;
    check_size(c.volume())?;
    let pattern = Pattern::parse(args, &HostBlocks).map_err(pattern_fail)?;
    queue(player, Kind::Edit, &world, &[c], fill_work(pattern, None))
}

fn replace(player: &Player, _server: &Server, args: &str) -> Res {
    let mut words = args.splitn(2, char::is_whitespace);
    let first = words.next().unwrap_or("").trim();
    let rest = words.next().unwrap_or("").trim();
    if first.is_empty() {
        return Err(fail("usage_replace", &[]));
    }
    let (world, c) = selection(player)?;
    check_size(c.volume())?;
    let (mask, pattern) = if rest.is_empty() {
        (
            Mask::existing(&state().air),
            Pattern::parse(first, &HostBlocks).map_err(pattern_fail)?,
        )
    } else {
        (
            Mask::parse(first, &HostBlocks).map_err(pattern_fail)?,
            Pattern::parse(rest, &HostBlocks).map_err(pattern_fail)?,
        )
    };
    queue(
        player,
        Kind::Edit,
        &world,
        &[c],
        fill_work(pattern, Some(mask)),
    )
}

fn faces(player: &Player, args: &str, usage: &'static str, outline: bool) -> Res {
    if args.is_empty() {
        return Err(fail(usage, &[]));
    }
    let (world, c) = selection(player)?;
    let parts = if outline { c.outline() } else { c.walls() };
    check_size(parts.iter().map(Cuboid::volume).sum())?;
    let pattern = Pattern::parse(args, &HostBlocks).map_err(pattern_fail)?;
    queue(player, Kind::Edit, &world, &parts, fill_work(pattern, None))
}

fn walls(player: &Player, _server: &Server, args: &str) -> Res {
    faces(player, args, "usage_walls", false)
}

fn outline(player: &Player, _server: &Server, args: &str) -> Res {
    faces(player, args, "usage_outline", true)
}

// ---- clipboard ----

fn copy_or_cut(player: &Player, leave: Option<u16>) -> Res {
    let (world, c) = selection(player)?;
    let owner = uuid_of(player);
    let volume = c.volume();
    {
        let mut s = state();
        s.prune_sessions();
        let own = s
            .sessions
            .get(&owner)
            .and_then(|x| x.clipboard.as_ref())
            .map_or(0, |c| c.volume());
        if volume > s.config.clipboard_max_blocks {
            return Err(fail(
                "too_big",
                &[
                    &volume.to_string(),
                    &s.config.clipboard_max_blocks.to_string(),
                ],
            ));
        }
        if s.clipboard_blocks() - own + volume > s.config.clipboard_total_blocks {
            return Err(fail("clipboard_full", &[]));
        }
    }
    if leave.is_some() {
        check_size(volume)?;
    }
    let me = block_pos(player);
    let (x, y, z) = c.size();
    let offset = Pos::new(c.min.x - me.x, c.min.y - me.y, c.min.z - me.z);
    let clipboard = Clipboard::new((x as u32, y as u32, z as u32), offset, c.min)
        .ok_or_else(|| fail("too_big", &[&volume.to_string(), &volume.to_string()]))?;
    let kind = if leave.is_some() {
        Kind::Cut
    } else {
        Kind::Copy
    };
    queue(
        player,
        kind,
        &world,
        &[c],
        Work::Copy {
            clipboard,
            min: c.min,
            leave,
        },
    )
}

fn copy(player: &Player, _server: &Server, _args: &str) -> Res {
    copy_or_cut(player, None)
}

fn cut(player: &Player, _server: &Server, args: &str) -> Res {
    let air = state().air.first().copied().unwrap_or(0);
    let leave = if args.is_empty() {
        air
    } else {
        Pattern::parse(args, &HostBlocks)
            .map_err(pattern_fail)?
            .states()
            .next()
            .unwrap_or(air)
    };
    copy_or_cut(player, Some(leave))
}

fn clipboard_of(player: &Player) -> std::result::Result<Arc<Clipboard>, Fail> {
    let owner = uuid_of(player);
    state()
        .sessions
        .get(&owner)
        .and_then(|s| s.clipboard.clone())
        .ok_or_else(|| fail("no_clipboard", &[]))
}

fn paste(player: &Player, _server: &Server, args: &str) -> Res {
    let mut skip_air = false;
    let mut original = false;
    for flag in args.split_whitespace() {
        match flag {
            "-a" => skip_air = true,
            "-o" => original = true,
            "-ao" | "-oa" => (skip_air, original) = (true, true),
            _ => return Err(fail("usage_paste", &[])),
        }
    }
    let clipboard = clipboard_of(player)?;
    let world = player.get_world();
    let at = if original {
        clipboard.original_point()
    } else {
        block_pos(player)
    };
    let (lo, hi) = bounds(&world);
    let target = clipboard
        .target(at)
        .clamp_y(lo, hi)
        .ok_or_else(|| fail("out_of_world", &[]))?;
    let air: HashSet<u16> = state().air.iter().copied().collect();
    let work = Work::Paste {
        clipboard,
        at,
        skip_air,
        air,
        cache: HashMap::new(),
    };
    queue(player, Kind::Edit, &world.get_name(), &[target], work)
}

fn change_clipboard(player: &Player, t: Transform) -> std::result::Result<(), Fail> {
    let owner = uuid_of(player);
    let mut s = state();
    let clipboard = s
        .sessions
        .get_mut(&owner)
        .and_then(|x| x.clipboard.as_mut())
        .ok_or_else(|| fail("no_clipboard", &[]))?;
    let c = Arc::make_mut(clipboard);
    c.transform = t.then_after(c.transform);
    Ok(())
}

fn rotate(player: &Player, _server: &Server, args: &str) -> Res {
    let degrees: i32 = args
        .split_whitespace()
        .next()
        .and_then(|d| d.parse().ok())
        .ok_or_else(|| fail("usage_rotate", &[]))?;
    if degrees % 90 != 0 || degrees.abs() > 3600 {
        return Err(fail("usage_rotate", &[]));
    }
    change_clipboard(player, Transform::rotate_y(degrees / 90))?;
    say(player, "rotated", &[&degrees.to_string()]);
    Ok(1)
}

/// The direction a player looks in: up, down, or the nearest of the four.
fn facing(player: &Player) -> &'static str {
    let pitch = player.get_pitch();
    if pitch > 67.5 {
        return "down";
    }
    if pitch < -67.5 {
        return "up";
    }
    let yaw = f64::from(player.get_yaw()).rem_euclid(360.0);
    match (((yaw + 45.0) / 90.0) as i32) % 4 {
        0 => "south",
        1 => "west",
        2 => "north",
        _ => "east",
    }
}

fn flip(player: &Player, _server: &Server, args: &str) -> Res {
    let direction = if args.is_empty() {
        facing(player)
    } else {
        args
    };
    let axis = match direction {
        "east" | "west" => 0,
        "up" | "down" => 1,
        "north" | "south" => 2,
        _ => return Err(fail("usage_flip", &[])),
    };
    change_clipboard(player, Transform::flip(axis))?;
    say(player, "flipped", &[direction]);
    Ok(1)
}

// ---- history ----

fn undo(player: &Player, _server: &Server, args: &str) -> Res {
    if !args.is_empty() && args != "1" {
        return Err(fail("usage_undo", &[]));
    }
    restore(player, true)
}

fn redo(player: &Player, _server: &Server, _args: &str) -> Res {
    restore(player, false)
}

fn restore(player: &Player, undo: bool) -> Res {
    let owner = uuid_of(player);
    check_busy(&owner)?;
    let record = {
        let mut s = state();
        if undo {
            s.history.take_undo(&owner)
        } else {
            s.history.take_redo(&owner)
        }
    };
    let Some(record) = record else {
        return Err(fail(if undo { "no_undo" } else { "no_redo" }, &[]));
    };
    let world = record.world.clone();
    let mut job = Job::new(
        &world,
        &[],
        Work::Restore {
            record,
            undo,
            index: 0,
            entities_done: false,
            biome_index: 0,
        },
    );
    job.entity_data = player.has_permission(&node(ENTITY_NODE));
    let total = job.total;
    state::start(Running::new(
        &owner,
        if undo { Kind::Undo } else { Kind::Redo },
        job,
    ));
    Ok(i32::try_from(total).unwrap_or(i32::MAX))
}

fn clear_history(player: &Player, _server: &Server, _args: &str) -> Res {
    let owner = uuid_of(player);
    state().history.clear(&owner);
    say(player, "history_cleared", &[]);
    Ok(1)
}

fn cancel(player: &Player, _server: &Server, _args: &str) -> Res {
    let owner = uuid_of(player);
    let mut s = state();
    let Some(running) = s.jobs.iter_mut().find(|r| r.owner == owner) else {
        return Err(fail("nothing_to_cancel", &[]));
    };
    running.cancel = true;
    Ok(1)
}

// ---- schematics ----

/// The metadata of a plain file at path: a symbolic link (or anything else) is not one, so a
/// link in the folder never leads out of it.
fn plain_file(path: &str) -> Option<std::fs::Metadata> {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
}

fn schematic_path(name: &str) -> String {
    format!("{SCHEMATIC_DIR}/{name}.schem")
}

fn tell(server: &Server, owner: &str, key: &'static str, args: &[&str]) {
    if let Some(p) =
        strawberry_plugin_api::uuid::parse(owner).and_then(|id| server.get_player_by_uuid(id))
    {
        say(&p, key, args);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

fn schem(player: &Player, server: &Server, args: &str) -> Res {
    let mut words = args.split_whitespace();
    let action = words.next().unwrap_or("");
    let name = words.next().unwrap_or("").to_owned();
    let needs_name = matches!(action, "save" | "load" | "delete");
    if !matches!(action, "save" | "load" | "delete" | "list")
        || (needs_name && name.is_empty())
        || words.next().is_some()
    {
        return Err(fail("usage_schem", &[]));
    }
    if !player.has_permission(&node(&format!("schem.{action}"))) {
        return Err(fail("no_permission", &[]));
    }
    if needs_name && !schematic::valid_name(&name) {
        return Err(fail("schem_bad_name", &[]));
    }
    let owner = uuid_of(player);
    let (max_kib, max_blocks, total_blocks, air) = {
        let s = state();
        (
            s.config.schematic_max_kib,
            s.config.clipboard_max_blocks,
            s.config.clipboard_total_blocks,
            s.air.first().copied().unwrap_or(0),
        )
    };
    match action {
        "save" => {
            let clipboard = clipboard_of(player)?;
            server.schedule_delayed_async_task(1, move |server| {
                let path = schematic_path(&name);
                if std::fs::symlink_metadata(&path).is_ok() {
                    tell(&server, &owner, "schem_exists", &[&name]);
                    return;
                }
                let bytes = schematic::write(&clipboard, &HostBlocks, now_ms());
                let temporary = format!("{path}.tmp");
                let written = std::fs::create_dir_all(SCHEMATIC_DIR)
                    .and_then(|()| std::fs::write(&temporary, &bytes))
                    .and_then(|()| std::fs::rename(&temporary, &path));
                match written {
                    Ok(()) => tell(
                        &server,
                        &owner,
                        "schem_saved",
                        &[&name, &clipboard.volume().to_string()],
                    ),
                    Err(e) => {
                        strawberry_plugin_api::logging::log(
                            strawberry_plugin_api::logging::LogLevel::Warn,
                            &format!("worldeditor-lite: could not save schematic {name}: {e}"),
                        );
                        tell(&server, &owner, "schem_write_failed", &[&name]);
                    }
                }
            });
        }
        "load" => {
            server.schedule_delayed_async_task(1, move |server| {
                let path = schematic_path(&name);
                let Some(meta) = plain_file(&path) else {
                    tell(&server, &owner, "schem_not_found", &[&name]);
                    return;
                };
                if meta.len() > max_kib * 1024 {
                    tell(
                        &server,
                        &owner,
                        "schem_too_large",
                        &[&name, &max_kib.to_string()],
                    );
                    return;
                }
                let limits = schematic::Limits {
                    max_unpacked: 16 << 20,
                    max_volume: max_blocks,
                };
                let loaded = std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| {
                        schematic::read(&bytes, limits, &HostBlocks, air).map_err(|e| e.to_string())
                    });
                let loaded = match loaded {
                    Ok(l) => l,
                    Err(e) => {
                        strawberry_plugin_api::logging::log(
                            strawberry_plugin_api::logging::LogLevel::Warn,
                            &format!("worldeditor-lite: could not read schematic {name}: {e}"),
                        );
                        tell(&server, &owner, "schem_failed", &[&name]);
                        return;
                    }
                };
                let volume = loaded.clipboard.volume();
                let fits = {
                    let mut s = state();
                    s.prune_sessions();
                    let own = s
                        .sessions
                        .get(&owner)
                        .and_then(|x| x.clipboard.as_ref())
                        .map_or(0, |c| c.volume());
                    let fits = s.clipboard_blocks() - own + volume <= total_blocks;
                    if fits {
                        s.session(&owner).clipboard = Some(Arc::new(loaded.clipboard));
                    }
                    fits
                };
                if !fits {
                    tell(&server, &owner, "clipboard_full", &[]);
                    return;
                }
                tell(
                    &server,
                    &owner,
                    "schem_loaded",
                    &[&name, &volume.to_string()],
                );
                if loaded
                    .data_version
                    .is_some_and(|v| v > i64::from(schematic::DATA_VERSION))
                {
                    tell(&server, &owner, "schem_newer", &[&name]);
                }
                if loaded.unknown_count > 0 {
                    tell(
                        &server,
                        &owner,
                        "schem_unknown_blocks",
                        &[
                            &loaded.unknown_count.to_string(),
                            &loaded.unknown.join(", "),
                        ],
                    );
                }
            });
        }
        "delete" => {
            server.schedule_delayed_async_task(1, move |server| {
                let path = schematic_path(&name);
                let removed = plain_file(&path).is_some() && std::fs::remove_file(&path).is_ok();
                if removed {
                    tell(&server, &owner, "schem_deleted", &[&name]);
                } else {
                    tell(&server, &owner, "schem_not_found", &[&name]);
                }
            });
        }
        _ => {
            server.schedule_delayed_async_task(1, move |server| {
                let mut names: Vec<String> = std::fs::read_dir(SCHEMATIC_DIR)
                    .map(|dir| {
                        dir.filter_map(std::result::Result::ok)
                            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                            .filter_map(|e| e.file_name().into_string().ok())
                            .filter_map(|n| n.strip_suffix(".schem").map(str::to_owned))
                            .filter(|n| schematic::valid_name(n))
                            .take(1000)
                            .collect()
                    })
                    .unwrap_or_default();
                names.sort();
                if names.is_empty() {
                    tell(&server, &owner, "schem_list_empty", &[]);
                } else {
                    let count = names.len().to_string();
                    names.truncate(100);
                    tell(&server, &owner, "schem_list", &[&count, &names.join(", ")]);
                }
            });
        }
    }
    Ok(1)
}

// ---- brushes ----

fn number(text: Option<&str>, default: u32, max: u32) -> std::result::Result<u32, Fail> {
    match text {
        None => Ok(default.min(max)),
        Some(t) => t
            .parse::<u32>()
            .ok()
            .filter(|v| (1..=max).contains(v))
            .ok_or_else(|| fail("brush_radius", &[&max.to_string()])),
    }
}

fn brush(player: &Player, _server: &Server, args: &str) -> Res {
    let mut words = args.split_whitespace();
    let kind = words.next().unwrap_or("");
    let item = player
        .get_item_in_hand(Hand::Right)
        .map(|i| crate::events::item_key(&i.get_registry_key()))
        .filter(|k| k != "minecraft:air")
        .ok_or_else(|| fail("brush_no_item", &[]))?;
    let (wand, max) = {
        let s = state();
        (s.config.wand_item.clone(), s.config.brush_max_radius)
    };
    let shown = item.strip_prefix("minecraft:").unwrap_or(&item).to_owned();
    let owner = uuid_of(player);
    if kind == "none" {
        state().session(&owner).brush = None;
        say(player, "brush_cleared", &[&shown]);
        return Ok(1);
    }
    if item == wand {
        return Err(fail("brush_wand", &[]));
    }
    let Some(text) = words.next() else {
        return Err(fail("usage_brush", &[]));
    };
    let shape = match kind {
        "sphere" | "s" => BrushShape::Sphere {
            radius: number(words.next(), 3, max)?,
        },
        "cylinder" | "cyl" | "c" => {
            let radius = number(words.next(), 3, max)?;
            BrushShape::Cylinder {
                radius,
                height: number(words.next(), 1, max.saturating_mul(2))?,
            }
        }
        _ => return Err(fail("usage_brush", &[])),
    };
    if words.next().is_some() {
        return Err(fail("usage_brush", &[]));
    }
    let pattern = Pattern::parse(text, &HostBlocks).map_err(pattern_fail)?;
    state().session(&owner).brush = Some(Brush {
        item,
        shape,
        pattern,
    });
    match shape {
        BrushShape::Sphere { radius } => say(
            player,
            "brush_sphere_set",
            &[&shown, text, &radius.to_string()],
        ),
        BrushShape::Cylinder { radius, height } => {
            say(
                player,
                "brush_cylinder_set",
                &[&shown, text, &radius.to_string(), &height.to_string()],
            );
        }
    }
    Ok(1)
}

/// Paints the player's brush at the clicked block, or at the block they look at.
pub fn use_brush(player: &Player, clicked: Option<Pos>) -> Res {
    let owner = uuid_of(player);
    let (shape, pattern) = {
        let s = state();
        let Some(b) = s.sessions.get(&owner).and_then(|x| x.brush.as_ref()) else {
            return Ok(0);
        };
        (b.shape, b.pattern.clone())
    };
    let center = match clicked {
        Some(p) => p,
        None => target(player)?,
    };
    let shape = match shape {
        BrushShape::Sphere { radius } => Shape::Sphere {
            center,
            radius: f64::from(radius),
        },
        BrushShape::Cylinder { radius, height } => Shape::Cylinder {
            center,
            radius: f64::from(radius),
            height: i32::try_from(height).unwrap_or(1),
        },
    };
    let world = player.get_world();
    let (lo, hi) = bounds(&world);
    let Some(area) = shape.bounds().and_then(|b| b.clamp_y(lo, hi)) else {
        return Err(fail("out_of_world", &[]));
    };
    queue(
        player,
        Kind::Edit,
        &world.get_name(),
        &[area],
        Work::Fill {
            pattern,
            mask: None,
            shape,
            seed: seed(),
        },
    )
}

// ---- biomes ----

fn set_biome(player: &Player, server: &Server, args: &str) -> Res {
    if args.is_empty() || args.contains(char::is_whitespace) {
        return Err(fail("usage_setbiome", &[]));
    }
    let biome = biome::index_of(args).ok_or_else(|| fail("biome_unknown", &[args]))?;
    let (world, c) = selection(player)?;
    let dimension = player.get_world().get_dimension();
    let first = server
        .get_all_worlds()
        .into_iter()
        .find(|w| w.get_dimension() == dimension)
        .map(|w| w.get_name());
    if first.as_deref() != Some(world.as_str()) {
        return Err(fail("biome_world", &[]));
    }
    let cells = Cuboid {
        min: Pos::new(c.min.x >> 2, c.min.y >> 2, c.min.z >> 2),
        max: Pos::new(c.max.x >> 2, c.max.y >> 2, c.max.z >> 2),
    };
    check_size(cells.volume())?;
    let owner = uuid_of(player);
    check_busy(&owner)?;
    let job = Job::new(
        &world,
        &[],
        Work::Biome {
            biome,
            pieces: biome_pieces(cells, biome::MAX_CELLS),
            piece: 0,
            read: None,
        },
    );
    let total = job.total;
    let mut running = Running::new(&owner, Kind::Biome, job);
    running.area = Some(c);
    state::start(running);
    Ok(i32::try_from(total).unwrap_or(i32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_parse_with_spaces_or_commas() {
        assert_eq!(coords("1 2 3"), Some(Pos::new(1, 2, 3)));
        assert_eq!(coords("-1,64,-3"), Some(Pos::new(-1, 64, -3)));
        assert_eq!(coords("1, 2, 3"), Some(Pos::new(1, 2, 3)));
        assert_eq!(coords("1 2"), None);
        assert_eq!(coords("1 2 x"), None);
        assert_eq!(coords("99999999 1 1"), None);
    }

    #[test]
    fn every_command_has_its_own_permission() {
        let mut nodes: Vec<&str> = COMMANDS.iter().map(|c| c.node).collect();
        nodes.extend(SCHEM_NODES);
        nodes.push(ENTITY_NODE);
        let before = nodes.len();
        nodes.sort_unstable();
        nodes.dedup();
        assert_eq!(nodes.len(), before);
        assert!(COMMANDS.iter().all(|c| !c.names.is_empty()));
    }
}
