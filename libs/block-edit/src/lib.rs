//! block-edit: the pure part of block editing, shared by every Strawberry plugin that edits or
//! reads blocks in bulk.
//!
//! Nothing here calls the server. The server's block registry comes in through
//! [`pattern::Blocks`], the world through [`job::WorldAccess`], so a plugin keeps its host calls in
//! one adapter and everything here is tested on the host target.
//!
//! - [`geom`]: positions, boxes, walls and outlines, chunk splits, brush shapes, a chunk-ordered
//!   cursor.
//! - [`pattern`]: block text (`oak_stairs[facing=east]`), weighted patterns, masks.
//! - [`transform`]: quarter turns and flips of positions and block states.
//! - [`clipboard`], [`history`]: copied boxes; undo and redo with memory and step limits.
//! - [`job`]: edits as jobs that run in bounded steps and never touch an unloaded chunk.
//! - [`nbt`], [`gzip`], [`schematic`]: bounded readers and writers; `.schem` schematics v1 to v3
//!   read (low level: [`schematic::decode`]), v3 written. Every limit is an argument.

pub mod clipboard;
pub mod geom;
pub mod gzip;
pub mod history;
pub mod job;
pub mod nbt;
pub mod pattern;
pub mod schematic;
pub mod transform;
