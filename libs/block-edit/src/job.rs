//! Edits as jobs that run in bounded steps, a few milliseconds per game tick.
//!
//! A job reads and writes the world through [`WorldAccess`], so the whole job runner is tested
//! against a fake world. A step stops at its time or block budget, or when the next block is in
//! a chunk that is not loaded: nothing is ever read from or written to an unloaded chunk (the
//! server would answer air and drop the write).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::clipboard::Clipboard;
use crate::geom::{Cursor, Pos, Shape};
use crate::history::{Change, Record};
use crate::pattern::{Blocks, Mask, Pattern};
use crate::transform::transform_state;

/// What a job needs from the world.
pub trait WorldAccess {
    fn loaded(&mut self, chunk: (i32, i32)) -> bool;
    fn get(&mut self, p: Pos) -> u16;
    fn set(&mut self, p: Pos, state: u16);
    /// Whether blocks in `state` carry block entity data.
    fn has_entity(&mut self, state: u16) -> bool;
    fn get_entity(&mut self, p: Pos) -> Option<Vec<u8>>;
    fn set_entity(&mut self, p: Pos, data: &[u8]);
    /// The biome (index in `biome::BIOMES`) of a 4 × 4 × 4 cell, by cell coordinates.
    fn get_biome(&mut self, cell: Pos) -> Option<u8>;
    /// Sets every cell from `min` to `max` (cell coordinates) to `biome`.
    fn fill_biome(&mut self, min: Pos, max: Pos, biome: u8);
    /// A monotonic clock in microseconds.
    fn now_us(&mut self) -> u64;
}

/// Blocks between two looks at the clock: a block costs tens of microseconds of host work, so
/// a step overruns its time budget by at most this many blocks.
pub const CLOCK_EVERY: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub micros: u64,
    pub blocks: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The budget ran out; more work is left.
    More,
    /// The next block is in this chunk, which is not loaded.
    Blocked((i32, i32)),
    Done,
}

/// What a job does with each position.
pub enum Work {
    /// Place `pattern` where `mask` matches (everything when there is no mask) inside `shape`.
    Fill {
        pattern: Pattern,
        mask: Option<Mask>,
        shape: Shape,
        seed: u64,
    },
    /// Paste `clipboard` with its transform at `at`; `skip_air` keeps the world's block where the
    /// clipboard has air.
    Paste {
        clipboard: Arc<Clipboard>,
        at: Pos,
        skip_air: bool,
        air: HashSet<u16>,
        cache: HashMap<u16, u16>,
    },
    /// Read the box into `clipboard` (lowest corner `min`); with `leave`, put that block in
    /// each read position afterwards (`//cut`).
    Copy {
        clipboard: Clipboard,
        min: Pos,
        leave: Option<u16>,
    },
    /// Write back one side of a record (undo writes the old states, redo the new ones).
    Restore {
        record: Record,
        undo: bool,
        index: usize,
        entities_done: bool,
        biome_index: usize,
    },
    /// Set the biome of cell boxes (`pieces`, cell coordinates), reading each piece first for
    /// undo.
    Biome {
        biome: u8,
        pieces: Vec<crate::geom::Cuboid>,
        piece: usize,
        read: Option<Cursor>,
    },
}

pub struct Job {
    pub world: String,
    pub work: Work,
    cursor: Cursor,
    pub done: u64,
    pub total: u64,
    pub changed: u64,
    /// What this job changed, for undo (empty for undo and redo themselves).
    pub record: Record,
    /// Whether block entity data (container contents, sign text) is read and written. Off: the
    /// job moves block states only, and nothing it does can copy or bring back the contents of
    /// a container.
    pub entity_data: bool,
}

impl Job {
    #[must_use]
    pub fn new(world: &str, parts: &[crate::geom::Cuboid], work: Work) -> Self {
        let cursor = Cursor::new(parts);
        let total = match &work {
            Work::Restore { record, .. } => (record.changes.len() + record.biomes.len()) as u64,
            Work::Biome { pieces, .. } => pieces.iter().map(crate::geom::Cuboid::volume).sum(),
            _ => cursor.total(),
        };
        Self {
            world: world.to_owned(),
            work,
            cursor,
            done: 0,
            total,
            changed: 0,
            record: Record {
                world: world.to_owned(),
                ..Record::default()
            },
            entity_data: true,
        }
    }

    /// The chunks the next part of this job needs, for loading ahead.
    #[must_use]
    pub fn chunks_ahead(&self, ahead: usize) -> Vec<(i32, i32)> {
        match &self.work {
            Work::Restore {
                record,
                index,
                undo,
                ..
            } => {
                let mut out: Vec<(i32, i32)> = Vec::new();
                let n = record.changes.len();
                for k in 0..n.saturating_sub(*index) {
                    let i = if *undo {
                        n - 1 - (index + k)
                    } else {
                        index + k
                    };
                    let c = record.changes[i].pos.chunk();
                    if !out.contains(&c) {
                        out.push(c);
                        if out.len() >= ahead {
                            break;
                        }
                    }
                }
                out
            }
            Work::Biome { pieces, piece, .. } => {
                let mut out: Vec<(i32, i32)> = Vec::new();
                for p in pieces.iter().skip(*piece) {
                    for c in cell_box_to_blocks(p).chunks() {
                        if !out.contains(&c) {
                            out.push(c);
                        }
                    }
                    if out.len() >= ahead {
                        break;
                    }
                }
                out
            }
            _ => self.cursor.chunks_ahead(ahead),
        }
    }

    /// The clipboard a finished copy job made.
    #[must_use]
    pub fn take_clipboard(self) -> Option<(Clipboard, Record)> {
        match self.work {
            Work::Copy { clipboard, .. } => Some((clipboard, self.record)),
            _ => None,
        }
    }

    fn write(&mut self, world: &mut dyn WorldAccess, p: Pos, old: u16, new: u16) {
        if self.entity_data
            && world.has_entity(old)
            && let Some(data) = world.get_entity(p)
        {
            self.record.old_entities.push((p, data));
        }
        world.set(p, new);
        self.record.changes.push(Change { pos: p, old, new });
        self.changed += 1;
    }

    /// Runs until the budget is used, a chunk is missing or the job is done.
    pub fn step(
        &mut self,
        world: &mut dyn WorldAccess,
        blocks: &dyn Blocks,
        budget: Budget,
    ) -> Step {
        if let Work::Restore { .. } = self.work {
            return self.step_restore(world, budget);
        }
        if let Work::Biome { .. } = self.work {
            return self.step_biome(world, budget);
        }
        let start = world.now_us();
        let mut count: u64 = 0;
        let mut checked: Option<(i32, i32)> = None;
        loop {
            let Some(chunk) = self.cursor.chunk() else {
                return Step::Done;
            };
            if checked != Some(chunk) {
                if !world.loaded(chunk) {
                    return Step::Blocked(chunk);
                }
                checked = Some(chunk);
            }
            if count >= budget.blocks
                || (count.is_multiple_of(CLOCK_EVERY)
                    && count > 0
                    && world.now_us().saturating_sub(start) >= budget.micros)
            {
                return Step::More;
            }
            let Some(p) = self.cursor.next() else {
                return Step::Done;
            };
            self.done += 1;
            count += 1;
            self.visit(world, blocks, p);
        }
    }

    fn visit(&mut self, world: &mut dyn WorldAccess, blocks: &dyn Blocks, p: Pos) {
        let entity_data = self.entity_data;
        match &mut self.work {
            Work::Fill {
                pattern,
                mask,
                shape,
                seed,
            } => {
                if !shape.contains(p) {
                    return;
                }
                let new = pattern.pick(p.x, p.y, p.z, *seed);
                let old = world.get(p);
                if old == new || mask.as_ref().is_some_and(|m| !m.matches(old)) {
                    return;
                }
                self.write(world, p, old, new);
            }
            Work::Paste {
                clipboard,
                at,
                skip_air,
                air,
                cache,
            } => {
                let Some(cell) = clipboard.source(*at, p) else {
                    return;
                };
                let new =
                    transform_state(&clipboard.transform, blocks, clipboard.blocks[cell], cache);
                if *skip_air && air.contains(&new) {
                    return;
                }
                let entity = if entity_data {
                    clipboard.entities.get(&cell).cloned()
                } else {
                    None
                };
                let old = world.get(p);
                if old == new && entity.is_none() {
                    return;
                }
                self.write(world, p, old, new);
                if let Some(data) = entity {
                    world.set_entity(p, &data);
                    self.record.new_entities.push((p, data));
                }
            }
            Work::Copy {
                clipboard,
                min,
                leave,
            } => {
                let state = world.get(p);
                let (x, y, z) = (
                    (p.x - min.x) as u32,
                    (p.y - min.y) as u32,
                    (p.z - min.z) as u32,
                );
                let index = clipboard.index(x, y, z);
                clipboard.blocks[index] = state;
                if entity_data
                    && world.has_entity(state)
                    && let Some(data) = world.get_entity(p)
                {
                    clipboard.entities.insert(index, data);
                }
                if let Some(new) = *leave
                    && new != state
                {
                    self.write(world, p, state, new);
                }
            }
            Work::Restore { .. } | Work::Biome { .. } => {}
        }
    }

    fn step_restore(&mut self, world: &mut dyn WorldAccess, budget: Budget) -> Step {
        let entity_data = self.entity_data;
        let start = world.now_us();
        let mut count: u64 = 0;
        let mut checked: Option<(i32, i32)> = None;
        let Work::Restore {
            record,
            undo,
            index,
            entities_done,
            biome_index,
        } = &mut self.work
        else {
            return Step::Done;
        };
        let n = record.changes.len();
        while *index < n {
            let i = if *undo { n - 1 - *index } else { *index };
            let change = record.changes[i];
            let chunk = change.pos.chunk();
            if checked != Some(chunk) {
                if !world.loaded(chunk) {
                    return Step::Blocked(chunk);
                }
                checked = Some(chunk);
            }
            if count >= budget.blocks
                || (count.is_multiple_of(CLOCK_EVERY)
                    && count > 0
                    && world.now_us().saturating_sub(start) >= budget.micros)
            {
                return Step::More;
            }
            world.set(change.pos, if *undo { change.old } else { change.new });
            *index += 1;
            count += 1;
            self.done += 1;
            self.changed += 1;
        }
        if !*entities_done && entity_data {
            let entities = if *undo {
                &record.old_entities
            } else {
                &record.new_entities
            };
            for (p, _) in entities {
                if !world.loaded(p.chunk()) {
                    return Step::Blocked(p.chunk());
                }
            }
            for (p, data) in entities {
                world.set_entity(*p, data);
            }
            *entities_done = true;
        }
        // Biomes, in runs along x of one target biome: one fill per run.
        let biomes = &record.biomes;
        while *biome_index < biomes.len() {
            if count > 0 && world.now_us().saturating_sub(start) >= budget.micros {
                return Step::More;
            }
            let first = biomes[*biome_index];
            let target = |c: &crate::history::BiomeChange| if *undo { c.old } else { c.new };
            let mut end = *biome_index + 1;
            while end < biomes.len() {
                let (a, b) = (biomes[end - 1], biomes[end]);
                if b.cell.y == a.cell.y
                    && b.cell.z == a.cell.z
                    && b.cell.x == a.cell.x + 1
                    && target(&b) == target(&first)
                {
                    end += 1;
                } else {
                    break;
                }
            }
            let last = biomes[end - 1];
            let blocks_box = cell_box_to_blocks(&crate::geom::Cuboid {
                min: first.cell,
                max: last.cell,
            });
            for c in blocks_box.chunks() {
                if !world.loaded(c) {
                    return Step::Blocked(c);
                }
            }
            world.fill_biome(first.cell, last.cell, target(&first));
            let n = (end - *biome_index) as u64;
            *biome_index = end;
            count += n;
            self.done += n;
            self.changed += n;
        }
        Step::Done
    }

    fn step_biome(&mut self, world: &mut dyn WorldAccess, budget: Budget) -> Step {
        let start = world.now_us();
        let mut count: u64 = 0;
        loop {
            let Work::Biome {
                biome,
                pieces,
                piece,
                read,
            } = &mut self.work
            else {
                return Step::Done;
            };
            let Some(current) = pieces.get(*piece).copied() else {
                return Step::Done;
            };
            if read.is_none() {
                for c in cell_box_to_blocks(&current).chunks() {
                    if !world.loaded(c) {
                        return Step::Blocked(c);
                    }
                }
                *read = Some(Cursor::new(&[current]));
            }
            let Some(cursor) = read.as_mut() else {
                return Step::Done;
            };
            while let Some(cell) = cursor.next() {
                if let Some(old) = world.get_biome(cell)
                    && old != *biome
                {
                    self.record.biomes.push(crate::history::BiomeChange {
                        cell,
                        old,
                        new: *biome,
                    });
                }
                count += 1;
                self.done += 1;
                if count >= budget.blocks
                    || (count.is_multiple_of(CLOCK_EVERY)
                        && world.now_us().saturating_sub(start) >= budget.micros)
                {
                    if cursor.is_done() {
                        break;
                    }
                    return Step::More;
                }
            }
            // The whole piece is read: one fill for it, when anything in it changes.
            let changed_here = self
                .record
                .biomes
                .iter()
                .rev()
                .take_while(|c| current.contains(c.cell))
                .count();
            if changed_here > 0 {
                for c in cell_box_to_blocks(&current).chunks() {
                    if !world.loaded(c) {
                        return Step::Blocked(c);
                    }
                }
                world.fill_biome(current.min, current.max, *biome);
                self.changed += changed_here as u64;
            }
            *piece += 1;
            *read = None;
            if world.now_us().saturating_sub(start) >= budget.micros {
                return Step::More;
            }
        }
    }
}

/// The blocks a box of biome cells covers.
#[must_use]
pub fn cell_box_to_blocks(cells: &crate::geom::Cuboid) -> crate::geom::Cuboid {
    crate::geom::Cuboid {
        min: Pos::new(cells.min.x * 4, cells.min.y * 4, cells.min.z * 4),
        max: Pos::new(
            cells.max.x * 4 + 3,
            cells.max.y * 4 + 3,
            cells.max.z * 4 + 3,
        ),
    }
}

/// Cuts a box of cells into fill pieces of at most `max_cells` cells: columns of 16 × 16 cells
/// (4 × 4 chunks), as tall as fits.
#[must_use]
pub fn biome_pieces(cells: crate::geom::Cuboid, max_cells: u64) -> Vec<crate::geom::Cuboid> {
    let tall = (max_cells / 256).max(1) as i32;
    let mut out = Vec::new();
    let mut z = cells.min.z;
    while z <= cells.max.z {
        let z1 = (z + 15).min(cells.max.z);
        let mut x = cells.min.x;
        while x <= cells.max.x {
            let x1 = (x + 15).min(cells.max.x);
            let mut y = cells.min.y;
            while y <= cells.max.y {
                let y1 = (y + tall - 1).min(cells.max.y);
                out.push(crate::geom::Cuboid {
                    min: Pos::new(x, y, z),
                    max: Pos::new(x1, y1, z1),
                });
                y = y1 + 1;
            }
            x = x1 + 1;
        }
        z = z1 + 1;
    }
    out
}

#[cfg(test)]
pub mod fake {
    use super::*;

    /// A world of one map, every chunk loaded unless listed, a clock that moves 1 µs per read.
    #[derive(Default)]
    pub struct FakeWorld {
        pub blocks: HashMap<Pos, u16>,
        pub entities: HashMap<Pos, Vec<u8>>,
        pub unloaded: HashSet<(i32, i32)>,
        pub entity_states: HashSet<u16>,
        pub clock: u64,
        pub writes: u64,
        pub biomes: HashMap<Pos, u8>,
        pub biome_fills: u64,
    }

    impl WorldAccess for FakeWorld {
        fn loaded(&mut self, chunk: (i32, i32)) -> bool {
            !self.unloaded.contains(&chunk)
        }
        fn get(&mut self, p: Pos) -> u16 {
            self.clock += 1;
            assert!(
                !self.unloaded.contains(&p.chunk()),
                "read from an unloaded chunk"
            );
            self.blocks.get(&p).copied().unwrap_or(0)
        }
        fn set(&mut self, p: Pos, state: u16) {
            self.clock += 1;
            self.writes += 1;
            assert!(
                !self.unloaded.contains(&p.chunk()),
                "write to an unloaded chunk"
            );
            if !self.entity_states.contains(&state) {
                self.entities.remove(&p);
            }
            self.blocks.insert(p, state);
        }
        fn has_entity(&mut self, state: u16) -> bool {
            self.entity_states.contains(&state)
        }
        fn get_entity(&mut self, p: Pos) -> Option<Vec<u8>> {
            self.entities.get(&p).cloned()
        }
        fn set_entity(&mut self, p: Pos, data: &[u8]) {
            self.entities.insert(p, data.to_vec());
        }
        fn get_biome(&mut self, cell: Pos) -> Option<u8> {
            self.clock += 1;
            Some(self.biomes.get(&cell).copied().unwrap_or(0))
        }
        fn fill_biome(&mut self, min: Pos, max: Pos, biome: u8) {
            self.biome_fills += 1;
            for c in Cursor::new(&[crate::geom::Cuboid { min, max }]) {
                self.biomes.insert(c, biome);
            }
        }
        fn now_us(&mut self) -> u64 {
            self.clock
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeWorld;
    use super::*;
    use crate::geom::Cuboid;
    use crate::pattern::fake::FakeBlocks;

    const BIG: Budget = Budget {
        micros: u64::MAX,
        blocks: u64::MAX,
    };

    fn run(job: &mut Job, world: &mut FakeWorld, budget: Budget) -> (Step, usize) {
        let blocks = FakeBlocks::default();
        let mut steps = 0;
        loop {
            steps += 1;
            match job.step(world, &blocks, budget) {
                Step::More => continue,
                other => return (other, steps),
            }
        }
    }

    fn fill(c: Cuboid, state: u16) -> Job {
        Job::new(
            "world",
            &[c],
            Work::Fill {
                pattern: Pattern::single(state),
                mask: None,
                shape: Shape::All,
                seed: 1,
            },
        )
    }

    #[test]
    fn a_fill_runs_in_bounded_steps_and_records_every_change() {
        let mut world = FakeWorld::default();
        world.blocks.insert(Pos::new(0, 0, 0), 1); // already stone: not a change
        let c = Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(19, 9, 19));
        let mut job = fill(c, 1);
        let (end, steps) = run(
            &mut job,
            &mut world,
            Budget {
                micros: u64::MAX,
                blocks: 500,
            },
        );
        assert_eq!(end, Step::Done);
        assert_eq!(
            steps, 8,
            "4000 blocks in steps of 500; the last step ends the job"
        );
        assert_eq!(job.done, 4000);
        assert_eq!(job.changed, 3999);
        assert_eq!(job.record.changes.len(), 3999);
        assert!(Cursor::new(&[c]).all(|p| world.blocks.get(&p) == Some(&1)));
    }

    #[test]
    fn the_time_budget_stops_a_step() {
        let mut world = FakeWorld::default();
        let mut job = fill(
            Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(15, 15, 15)),
            2,
        );
        let blocks = FakeBlocks::default();
        assert_eq!(
            job.step(
                &mut world,
                &blocks,
                Budget {
                    micros: 200,
                    blocks: u64::MAX
                }
            ),
            Step::More
        );
        // Each block costs two clock ticks here (read and write); the check runs every 64 blocks.
        assert!(job.done >= 100 && job.done <= 192, "done {}", job.done);
    }

    #[test]
    fn an_unloaded_chunk_blocks_the_job_and_nothing_touches_it() {
        let mut world = FakeWorld::default();
        world.unloaded.insert((1, 0));
        let mut job = fill(
            Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(31, 0, 0)),
            1,
        );
        let (end, _) = run(&mut job, &mut world, BIG);
        assert_eq!(end, Step::Blocked((1, 0)));
        assert_eq!(job.done, 16);
        world.unloaded.clear();
        let (end, _) = run(&mut job, &mut world, BIG);
        assert_eq!(end, Step::Done);
        assert_eq!(job.done, 32);
    }

    #[test]
    fn replace_touches_only_the_mask() {
        let blocks = FakeBlocks::default();
        let mut world = FakeWorld::default();
        for x in 0..10 {
            world
                .blocks
                .insert(Pos::new(x, 0, 0), if x % 2 == 0 { 1 } else { 2 });
        }
        let mask = Mask::parse("stone", &blocks).unwrap();
        let mut job = Job::new(
            "world",
            &[Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(9, 0, 0))],
            Work::Fill {
                pattern: Pattern::single(0),
                mask: Some(mask),
                shape: Shape::All,
                seed: 0,
            },
        );
        run(&mut job, &mut world, BIG);
        assert_eq!(job.changed, 5);
        for x in 0..10 {
            assert_eq!(
                world.blocks[&Pos::new(x, 0, 0)],
                if x % 2 == 0 { 0 } else { 2 }
            );
        }
    }

    #[test]
    fn undo_and_redo_restore_blocks_and_block_entities() {
        let mut world = FakeWorld::default();
        world.entity_states.insert(9);
        world.blocks.insert(Pos::new(1, 0, 0), 9);
        world.entities.insert(Pos::new(1, 0, 0), vec![1, 2, 3]);
        let mut job = fill(
            Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(3, 0, 0)),
            1,
        );
        run(&mut job, &mut world, BIG);
        assert!(world.entities.is_empty());
        let record = job.record.clone();
        assert_eq!(
            record.old_entities,
            vec![(Pos::new(1, 0, 0), vec![1, 2, 3])]
        );

        let mut undo = Job::new(
            "world",
            &[],
            Work::Restore {
                record: record.clone(),
                undo: true,
                index: 0,
                entities_done: false,
                biome_index: 0,
            },
        );
        assert_eq!(undo.total, 4);
        assert_eq!(
            run(
                &mut undo,
                &mut world,
                Budget {
                    micros: u64::MAX,
                    blocks: 1
                }
            )
            .0,
            Step::Done
        );
        assert_eq!(world.blocks[&Pos::new(1, 0, 0)], 9);
        assert_eq!(world.blocks[&Pos::new(0, 0, 0)], 0);
        assert_eq!(world.entities[&Pos::new(1, 0, 0)], vec![1, 2, 3]);
        assert!(undo.record.is_empty(), "undo leaves no record of its own");

        let mut redo = Job::new(
            "world",
            &[],
            Work::Restore {
                record,
                undo: false,
                index: 0,
                entities_done: false,
                biome_index: 0,
            },
        );
        run(&mut redo, &mut world, BIG);
        assert!((0..4).all(|x| world.blocks[&Pos::new(x, 0, 0)] == 1));
    }

    #[test]
    fn copy_cut_and_paste_with_a_rotation() {
        let blocks = FakeBlocks::default();
        let stairs_north = blocks
            .resolve("minecraft:oak_stairs", &[("facing".into(), "north".into())])
            .unwrap();
        let stairs_east = blocks
            .resolve("minecraft:oak_stairs", &[("facing".into(), "east".into())])
            .unwrap();
        let mut world = FakeWorld::default();
        world.entity_states.insert(2);
        world.blocks.insert(Pos::new(0, 64, 0), stairs_north);
        world.blocks.insert(Pos::new(1, 64, 0), 2);
        world.entities.insert(Pos::new(1, 64, 0), vec![7]);
        let selection = Cuboid::from_corners(Pos::new(0, 64, 0), Pos::new(1, 64, 0));
        let player = Pos::new(0, 64, -1);
        let clip = Clipboard::new((2, 1, 1), Pos::new(0, 0, 1), selection.min).unwrap();
        let mut copy = Job::new(
            "world",
            &[selection],
            Work::Copy {
                clipboard: clip,
                min: selection.min,
                leave: Some(0),
            },
        );
        run(&mut copy, &mut world, BIG);
        assert_eq!(copy.changed, 2, "the cut left air");
        assert_eq!(world.blocks[&Pos::new(0, 64, 0)], 0);
        let (mut clipboard, record) = copy.take_clipboard().unwrap();
        assert_eq!(record.changes.len(), 2);
        assert_eq!(clipboard.blocks, vec![stairs_north, 2]);
        assert_eq!(clipboard.entities.get(&1), Some(&vec![7]));
        let _ = player;

        clipboard.transform = crate::transform::Transform::rotate_y(1);
        let at = Pos::new(100, 70, 100);
        let target = clipboard.target(at);
        let mut paste = Job::new(
            "world",
            &[target],
            Work::Paste {
                clipboard: Arc::new(clipboard),
                at,
                skip_air: false,
                air: HashSet::from([0]),
                cache: HashMap::new(),
            },
        );
        run(&mut paste, &mut world, BIG);
        // Offset (0, 0, 1) turns to (-1, 0, 0): the first block lands west of the paste point.
        assert_eq!(world.blocks[&Pos::new(99, 70, 100)], stairs_east);
        assert_eq!(world.blocks[&Pos::new(99, 70, 101)], 2);
        assert_eq!(world.entities[&Pos::new(99, 70, 101)], vec![7]);
        assert_eq!(paste.record.new_entities.len(), 1);
    }

    #[test]
    fn biome_pieces_cover_the_cells_once_within_the_limit() {
        let cells = Cuboid::from_corners(Pos::new(-3, -16, 0), Pos::new(40, 79, 20));
        let pieces = biome_pieces(cells, 32_768);
        assert!(pieces.iter().all(|p| p.volume() <= 32_768));
        let total: u64 = pieces.iter().map(Cuboid::volume).sum();
        assert_eq!(total, cells.volume());
        let all: HashSet<Pos> = Cursor::new(&pieces).collect();
        assert_eq!(all.len() as u64, cells.volume());
    }

    #[test]
    fn a_biome_edit_reads_first_and_undo_puts_the_old_biomes_back() {
        let mut world = FakeWorld::default();
        // One cell is already the target; another has a third biome.
        world.biomes.insert(Pos::new(0, 16, 0), 5);
        world.biomes.insert(Pos::new(3, 16, 1), 7);
        let cells = Cuboid::from_corners(Pos::new(0, 16, 0), Pos::new(3, 17, 3));
        let mut job = Job::new(
            "world",
            &[],
            Work::Biome {
                biome: 5,
                pieces: biome_pieces(cells, 16),
                piece: 0,
                read: None,
            },
        );
        let (end, _) = run(
            &mut job,
            &mut world,
            Budget {
                micros: u64::MAX,
                blocks: 5,
            },
        );
        assert_eq!(end, Step::Done);
        assert_eq!(job.done, 32);
        assert_eq!(job.record.biomes.len(), 31);
        assert!(Cursor::new(&[cells]).all(|c| world.biomes.get(&c) == Some(&5)));
        assert_eq!(world.biome_fills, 2, "two pieces of 16 cells");

        let mut undo = Job::new(
            "world",
            &[],
            Work::Restore {
                record: job.record.clone(),
                undo: true,
                index: 0,
                entities_done: false,
                biome_index: 0,
            },
        );
        run(&mut undo, &mut world, BIG);
        assert_eq!(world.biomes[&Pos::new(0, 16, 0)], 5);
        assert_eq!(world.biomes[&Pos::new(3, 16, 1)], 7);
        assert_eq!(world.biomes[&Pos::new(2, 17, 2)], 0);
    }

    #[test]
    fn without_entity_data_no_edit_copies_or_brings_back_contents() {
        let mut world = FakeWorld::default();
        world.entity_states.insert(2);
        world.blocks.insert(Pos::new(0, 0, 0), 2);
        world.entities.insert(Pos::new(0, 0, 0), vec![9, 9]);
        let one = Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(0, 0, 0));

        // A copy carries the block, not its contents.
        let clip = Clipboard::new((1, 1, 1), Pos::new(0, 0, 0), one.min).unwrap();
        let mut copy = Job::new(
            "world",
            &[one],
            Work::Copy {
                clipboard: clip,
                min: one.min,
                leave: None,
            },
        );
        copy.entity_data = false;
        run(&mut copy, &mut world, BIG);
        let (mut clipboard, _) = copy.take_clipboard().unwrap();
        assert_eq!(clipboard.blocks, vec![2]);
        assert!(clipboard.entities.is_empty());

        // An edit over the container keeps no contents for undo.
        let mut set = fill(one, 1);
        set.entity_data = false;
        run(&mut set, &mut world, BIG);
        assert!(set.record.old_entities.is_empty());

        // Contents someone else's clipboard or a file carries are not written.
        clipboard.entities.insert(0, vec![7]);
        let mut paste = Job::new(
            "world",
            &[Cuboid::from_corners(Pos::new(5, 0, 0), Pos::new(5, 0, 0))],
            Work::Paste {
                clipboard: Arc::new(clipboard),
                at: Pos::new(5, 0, 0),
                skip_air: false,
                air: HashSet::new(),
                cache: HashMap::new(),
            },
        );
        paste.entity_data = false;
        run(&mut paste, &mut world, BIG);
        assert_eq!(world.blocks[&Pos::new(5, 0, 0)], 2);
        assert!(!world.entities.contains_key(&Pos::new(5, 0, 0)));

        // An undo of a record that holds contents puts the block back, empty.
        let mut record = set.record.clone();
        record.old_entities.push((Pos::new(0, 0, 0), vec![9, 9]));
        world.entities.clear();
        let mut undo = Job::new(
            "world",
            &[],
            Work::Restore {
                record,
                undo: true,
                index: 0,
                entities_done: false,
                biome_index: 0,
            },
        );
        undo.entity_data = false;
        run(&mut undo, &mut world, BIG);
        assert_eq!(world.blocks[&Pos::new(0, 0, 0)], 2);
        assert!(world.entities.is_empty());
    }

    #[test]
    fn a_brush_sphere_only_paints_its_ball() {
        let mut world = FakeWorld::default();
        let shape = Shape::Sphere {
            center: Pos::new(0, 64, 0),
            radius: 2.0,
        };
        let mut job = Job::new(
            "world",
            &[shape.bounds().unwrap()],
            Work::Fill {
                pattern: Pattern::single(1),
                mask: None,
                shape,
                seed: 0,
            },
        );
        run(&mut job, &mut world, BIG);
        assert_eq!(job.changed, 81);
    }
}
