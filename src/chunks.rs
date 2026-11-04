//! Chunk access for edits, in one place.
//!
//! - Without the `chunk-tickets` feature (the default): a chunk counts as loaded when
//!   `world.get-chunk` returns it. An edit that reaches an unloaded chunk waits for it (a player
//!   may come back), then stops; it never reads or writes there.
//! - With `chunk-tickets` (needs plugin chunk tickets, not in the SDK yet): the edit asks for the
//!   chunks ahead of it with `world.add-plugin-chunk-ticket`, polls `world.is-chunk-loaded`,
//!   and gives every ticket back with `remove-plugin-chunk-ticket` once it has passed the chunk
//!   or ended. This branch is not built until the SDK has those calls.

use std::collections::HashSet;

use strawberry_plugin_api::World;

/// How many chunks ahead an edit asks for.
pub const AHEAD: usize = 4;

#[must_use]
pub fn is_loaded(world: &World, chunk: (i32, i32)) -> bool {
    #[cfg(feature = "chunk-tickets")]
    {
        world.is_chunk_loaded(chunk.0, chunk.1)
    }
    #[cfg(not(feature = "chunk-tickets"))]
    {
        world.get_chunk(chunk.0, chunk.1).is_some()
    }
}

/// Whether this build can load chunks itself.
#[must_use]
pub const fn can_load() -> bool {
    cfg!(feature = "chunk-tickets")
}

/// The tickets one edit holds.
#[derive(Debug, Default)]
pub struct Tickets {
    held: HashSet<(i32, i32)>,
}

impl Tickets {
    /// Holds tickets for exactly `wanted` (the chunks ahead of the edit): new ones are asked
    /// for, passed ones given back.
    pub fn want(&mut self, world: &World, wanted: &[(i32, i32)]) {
        if !can_load() {
            return;
        }
        let keep: HashSet<(i32, i32)> = wanted.iter().copied().collect();
        let passed: Vec<(i32, i32)> = self.held.difference(&keep).copied().collect();
        for c in passed {
            release(world, c);
            self.held.remove(&c);
        }
        for c in keep {
            if !self.held.contains(&c) && request(world, c) {
                self.held.insert(c);
            }
        }
    }

    /// Gives every ticket back.
    pub fn release_all(&mut self, world: &World) {
        for c in self.held.drain() {
            release(world, c);
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

#[cfg(feature = "chunk-tickets")]
fn request(world: &World, c: (i32, i32)) -> bool {
    world.add_plugin_chunk_ticket(c.0, c.1)
}

#[cfg(not(feature = "chunk-tickets"))]
fn request(_world: &World, _c: (i32, i32)) -> bool {
    false
}

#[cfg(feature = "chunk-tickets")]
fn release(world: &World, c: (i32, i32)) {
    world.remove_plugin_chunk_ticket(c.0, c.1);
}

#[cfg(not(feature = "chunk-tickets"))]
fn release(_world: &World, _c: (i32, i32)) {}
