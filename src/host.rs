//! The server side of [`Blocks`] and [`WorldAccess`]: every host call of an edit is here.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Instant;

use strawberry_plugin_api::common::BlockPos;
use strawberry_plugin_api::world::{self, BlockFlags};
use strawberry_plugin_api::{Block, BlockState, Server, World};

use crate::chunks;
use crate::geom::Pos;
use crate::job::WorldAccess;
use crate::pattern::{Blocks, Props};

pub const fn wit(p: Pos) -> BlockPos {
    BlockPos {
        x: p.x,
        y: p.y,
        z: p.z,
    }
}

/// The server's block registry.
pub struct HostBlocks;

/// A block's states with their properties.
type StateList = Arc<Vec<(u16, Props)>>;

/// Every state of each block asked for so far, with its properties (one entry per block type).
static STATES: LazyLock<Mutex<HashMap<String, StateList>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn states_of(name: &str) -> Option<StateList> {
    if let Some(found) = STATES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(name)
    {
        return Some(Arc::clone(found));
    }
    let block = Block::from_name(name)?;
    let list: Vec<(u16, Props)> = block
        .state_ids
        .iter()
        .map(|&id| (id, world::get_block_properties(id)))
        .collect();
    let list = Arc::new(list);
    STATES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(name.to_owned(), Arc::clone(&list));
    Some(list)
}

impl Blocks for HostBlocks {
    /// Picked from the block's own state list. `world.resolve-block-state` is never given
    /// properties, so a property the block lacks never reaches the server.
    fn resolve(&self, name: &str, props: &[(String, String)]) -> Option<u16> {
        let block = Block::from_name(name)?;
        if props.is_empty() {
            return Some(block.default_state_id);
        }
        let states = states_of(name)?;
        let default = states
            .iter()
            .find(|(id, _)| *id == block.default_state_id)
            .map(|(_, p)| p.clone())?;
        crate::pattern::match_state(&default, &states, props)
    }

    fn states(&self, name: &str) -> Option<Vec<(u16, Props)>> {
        states_of(name).map(|s| s.as_ref().clone())
    }

    fn info(&self, state: u16) -> Option<(String, Props)> {
        world::block_state_to_info(state).map(|i| (i.name, i.properties))
    }
}

/// The states that are air, for `//paste -a` and the default `//replace` mask.
#[must_use]
pub fn air_states() -> Vec<u16> {
    ["minecraft:air", "minecraft:cave_air", "minecraft:void_air"]
        .iter()
        .filter_map(|n| Block::from_name(n).map(|b| b.default_state_id))
        .collect()
}

/// One world, for one step of one edit.
pub struct HostWorld<'a> {
    pub server: &'a Server,
    pub world: &'a World,
    pub dimension: &'a str,
    pub flags: BlockFlags,
    /// Whether a state has block entity data, kept across edits.
    pub entity_states: &'a mut HashMap<u16, bool>,
    pub start: Instant,
}

impl HostWorld<'_> {
    #[must_use]
    pub fn flags(neighbor_updates: bool) -> BlockFlags {
        let flags = BlockFlags::NOTIFY_LISTENERS
            | BlockFlags::SKIP_DROPS
            | BlockFlags::SKIP_BLOCK_ENTITY_REPLACED_CALLBACK;
        if neighbor_updates {
            flags | BlockFlags::NOTIFY_NEIGHBORS
        } else {
            flags
        }
    }
}

impl WorldAccess for HostWorld<'_> {
    fn loaded(&mut self, chunk: (i32, i32)) -> bool {
        chunks::is_loaded(self.world, chunk)
    }

    fn get(&mut self, p: Pos) -> u16 {
        self.world.get_block_state_id(wit(p))
    }

    fn set(&mut self, p: Pos, state: u16) {
        self.world.set_block_state(wit(p), state, self.flags);
    }

    fn has_entity(&mut self, state: u16) -> bool {
        *self.entity_states.entry(state).or_insert_with(|| {
            BlockState::from_id(state).is_some_and(|s| s.block_entity_type != u16::MAX)
        })
    }

    fn get_entity(&mut self, p: Pos) -> Option<Vec<u8>> {
        self.world.get_block_entity_nbt(wit(p))
    }

    fn set_entity(&mut self, p: Pos, data: &[u8]) {
        // Only bytes the server gave out or this plugin wrote come here; they are read once
        // more, because bytes the server cannot read would end this plugin's instance.
        if crate::nbt::read_unnamed(data).is_ok() {
            let _ = self.world.set_block_entity_nbt(wit(p), data);
        }
    }

    fn get_biome(&mut self, cell: Pos) -> Option<u8> {
        crate::biome::get(self.world, cell)
    }

    fn fill_biome(&mut self, min: Pos, max: Pos, biome: u8) {
        crate::biome::fill(self.server, self.dimension, min, max, biome);
    }

    fn now_us(&mut self) -> u64 {
        u64::try_from(self.start.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}
