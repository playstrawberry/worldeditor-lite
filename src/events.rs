//! The wand and the brushes, joins and leaves, and unloaded worlds.
//!
//! - Block damage (blocking): the first hit on a block with the wand sets the first position and
//!   is cancelled, so the block is not mined, in survival or creative. This is the server's
//!   "start digging" event; it fires before any break.
//!   No break handler: the server fires the damage event before every break it starts from a
//!   player, so a break listener would only add a plugin call to every mined block.
//! - Interact (blocking): with the wand, a right click on a block sets the second position and a
//!   left click on a block the first; the click does nothing else. With an item that has a
//!   brush, a right click paints at the clicked block, or at the block looked at.
//! - Join and leave (observers): the set of players who may use the wand or a brush. A player
//!   outside it costs the click handlers one host call (the player's id), whatever they click or
//!   mine. A player given the permission while online is in the set after any command of this
//!   plugin, or at the next join. Leaving drops the selection, clipboard and brush, or keeps them
//!   for `session-keep-minutes`; the undo history stays (it is bounded).
//! - World unload (blocking, never cancels): the world's undo history is forgotten and its edits
//!   stop, so nothing is written into a world loaded later under the same name.
//!
//! A failed handler lets the event go on (`FailureVerdict::Allow`): these are tools, not
//! protection.

use strawberry_plugin_api::common::Hand;
use strawberry_plugin_api::events::{
    BlockDamageEvent, EventData, EventHandler, FailureVerdict, HandlerOptions, HandlerPriority,
    InteractAction, PlayerInteractEvent, PlayerJoinEvent, PlayerLeaveEvent, WorldUnloadEvent,
};
use strawberry_plugin_api::{Context, Player, Result, Server};

use crate::commands::{node, set_position, use_brush};
use crate::geom::Pos;
use crate::state::{raw_id, say, state, text_for, uuid_of};

/// What the item in the main hand is to this plugin.
enum Tool {
    Wand,
    Brush,
    None,
}

/// An item key with its namespace: the server may answer `wooden_axe` or `minecraft:wooden_axe`.
#[must_use]
pub fn item_key(key: &str) -> String {
    if key.contains(':') {
        key.to_owned()
    } else {
        format!("minecraft:{key}")
    }
}

/// Whether the player may use a tool of this plugin at all: one host call (the id) and a set
/// lookup. Everyone else stops here.
fn active(player: &Player) -> bool {
    let id = raw_id(player);
    state().active.contains(&id)
}

fn tool(player: &Player) -> Tool {
    if !active(player) {
        return Tool::None;
    }
    let Some(key) = player
        .get_item_in_hand(Hand::Right)
        .map(|i| item_key(&i.get_registry_key()))
    else {
        return Tool::None;
    };
    let any_brush = {
        let s = state();
        if key == s.config.wand_item {
            return Tool::Wand;
        }
        s.sessions
            .values()
            .any(|x| x.brush.as_ref().is_some_and(|b| b.item == key))
    };
    if !any_brush {
        return Tool::None;
    }
    let owner = uuid_of(player);
    let has = state()
        .sessions
        .get(&owner)
        .and_then(|x| x.brush.as_ref())
        .is_some_and(|b| b.item == key);
    if has { Tool::Brush } else { Tool::None }
}

fn report(player: &Player, result: std::result::Result<i32, (&'static str, Vec<String>)>) {
    if let Err((key, args)) = result {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let text = text_for(player, key, &args);
        player.send_system_message(
            strawberry_plugin_api::text::TextComponent::text(&text),
            true,
        );
    }
}

/// The wand's first position from a hit on `p`; true when the event must be cancelled.
fn wand_hit(player: &Player, p: Pos) -> bool {
    if !matches!(tool(player), Tool::Wand) || !player.has_permission(&node("wand")) {
        return false;
    }
    report(player, set_position(player, 0, p, true));
    true
}

struct Interact;

impl EventHandler<PlayerInteractEvent> for Interact {
    fn handle(
        &self,
        _server: Server,
        mut event: EventData<PlayerInteractEvent>,
    ) -> EventData<PlayerInteractEvent> {
        // A left click into the air is nothing to the wand or a brush: no host call at all.
        if matches!(event.action, InteractAction::LeftClickAir) {
            return event;
        }
        let clicked = event.clicked_pos.map(|p| Pos::new(p.x, p.y, p.z));
        match tool(&event.player) {
            Tool::Wand => {
                if !event.player.has_permission(&node("wand")) {
                    return event;
                }
                let which = match event.action {
                    InteractAction::LeftClickBlock => 0,
                    InteractAction::RightClickBlock => 1,
                    _ => return event,
                };
                if let Some(p) = clicked {
                    event.cancelled = true;
                    report(&event.player, set_position(&event.player, which, p, true));
                }
            }
            Tool::Brush => {
                let at = match event.action {
                    InteractAction::RightClickBlock => clicked,
                    InteractAction::RightClickAir => None,
                    _ => return event,
                };
                if event.player.has_permission(&node("brush")) {
                    event.cancelled = true;
                    if state().config.edit_log {
                        let name = event.player.get_name();
                        state().current = Some((name, "/brush (click)".to_owned()));
                    }
                    report(&event.player, use_brush(&event.player, at));
                    state().current = None;
                }
            }
            Tool::None => {}
        }
        event
    }
}

struct Damage;

impl EventHandler<BlockDamageEvent> for Damage {
    fn handle(
        &self,
        _server: Server,
        mut event: EventData<BlockDamageEvent>,
    ) -> EventData<BlockDamageEvent> {
        let p = event.block_pos;
        if wand_hit(&event.player, Pos::new(p.x, p.y, p.z)) {
            event.cancelled = true;
        }
        event
    }
}

/// Whether the player may use the wand or a brush, from their permissions now.
pub fn may_use_tools(player: &Player) -> bool {
    player.has_permission(&node("wand")) || player.has_permission(&node("brush"))
}

struct Join;

impl EventHandler<PlayerJoinEvent> for Join {
    fn handle(
        &self,
        _server: Server,
        event: EventData<PlayerJoinEvent>,
    ) -> EventData<PlayerJoinEvent> {
        let tools = may_use_tools(&event.player);
        let id = raw_id(&event.player);
        let owner = uuid_of(&event.player);
        let mut s = state();
        if tools {
            s.active.insert(id);
        }
        s.prune_sessions();
        if let Some(session) = s.sessions.get_mut(&owner) {
            session.left = None;
        }
        event
    }
}

struct Leave;

impl EventHandler<PlayerLeaveEvent> for Leave {
    fn handle(
        &self,
        _server: Server,
        event: EventData<PlayerLeaveEvent>,
    ) -> EventData<PlayerLeaveEvent> {
        let id = raw_id(&event.player);
        let owner = uuid_of(&event.player);
        let mut s = state();
        s.active.remove(&id);
        if s.config.session_keep_minutes == 0 {
            s.sessions.remove(&owner);
        } else if let Some(session) = s.sessions.get_mut(&owner) {
            session.left = Some(std::time::Instant::now());
            session.last_click = None;
        }
        s.prune_sessions();
        event
    }
}

struct Unload;

impl EventHandler<WorldUnloadEvent> for Unload {
    fn handle(
        &self,
        server: Server,
        event: EventData<WorldUnloadEvent>,
    ) -> EventData<WorldUnloadEvent> {
        let world = event.target_world.get_name();
        let lost = {
            let mut s = state();
            for running in s.jobs.iter_mut().filter(|r| r.job.world == world) {
                running.world_gone = true;
            }
            s.history.forget_world(&world)
        };
        for owner in lost {
            if let Some(p) = strawberry_plugin_api::uuid::parse(&owner)
                .and_then(|id| server.get_player_by_uuid(id))
            {
                say(&p, "history_world_unloaded", &[&world]);
            }
        }
        event
    }
}

fn options(blocking: bool) -> HandlerOptions {
    HandlerOptions {
        priority: HandlerPriority::Normal,
        blocking,
        ignore_cancelled: true,
        on_failure: FailureVerdict::Allow,
    }
}

pub fn register(context: &Context) -> Result<()> {
    context
        .register_event_handler_with_options::<PlayerInteractEvent, _>(Interact, options(true))?;
    context.register_event_handler_with_options::<BlockDamageEvent, _>(Damage, options(true))?;
    context.register_event_handler_with_options::<PlayerJoinEvent, _>(Join, options(false))?;
    context.register_event_handler_with_options::<PlayerLeaveEvent, _>(Leave, options(false))?;
    // Blocking so that it runs before anything can load a world of the same name; it never
    // cancels. `ignore_cancelled: false`: the server unloads the world even when a handler
    // cancels the event (it has already shut the world down when the event fires).
    context.register_event_handler_with_options::<WorldUnloadEvent, _>(
        Unload,
        HandlerOptions {
            ignore_cancelled: false,
            ..options(true)
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn item_keys_get_their_namespace() {
        assert_eq!(super::item_key("wooden_axe"), "minecraft:wooden_axe");
        assert_eq!(super::item_key("minecraft:stick"), "minecraft:stick");
        assert_eq!(super::item_key("mod:wand"), "mod:wand");
    }
}
