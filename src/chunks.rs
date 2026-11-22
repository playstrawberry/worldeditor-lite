//! Chunk access for edits, in one place.
//!
//! An edit asks for the chunks ahead of it with `world.add-plugin-chunk-ticket` (at most [`AHEAD`]
//! at a time), polls `world.is-chunk-loaded`, and gives every ticket back with
//! `remove-plugin-chunk-ticket` once it has passed the chunk or ended. A chunk the server does not
//! load in time stops the edit (`chunk-wait-seconds`); nothing is ever read from or written to an
//! unloaded chunk.

use std::collections::HashSet;

use strawberry_plugin_api::World;

/// How many chunks ahead an edit asks for.
pub const AHEAD: usize = 4;

#[must_use]
pub fn is_loaded(world: &World, chunk: (i32, i32)) -> bool {
    world.is_chunk_loaded(chunk.0, chunk.1)
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

fn request(world: &World, c: (i32, i32)) -> bool {
    world.add_plugin_chunk_ticket(c.0, c.1)
}

fn release(world: &World, c: (i32, i32)) {
    world.remove_plugin_chunk_ticket(c.0, c.1);
}
