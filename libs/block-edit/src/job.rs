//! Edits as jobs that run in bounded steps, a few milliseconds per game tick.
//!
//! A job reads and writes the world through [`WorldAccess`], so the whole engine is tested
//! against a fake world. A step stops at its time or block budget, or when the next block is in
//! a chunk that is not loaded: nothing is ever read from or written to an unloaded chunk (the
//! server would answer air and drop the write).
//!
//! Writes go to the server in batches, never block by block: a step reads each block (for the
//! undo record), queues what changes, and sends it in one call of at most [`MAX_BATCH`] blocks.
//! A fill of one state with no mask sends boxes instead ([`WorldAccess::fill`]). A batch the
//! server refuses wrote nothing: its changes leave the record, so undo stays exact, and the job
//! ends with [`Step::Refused`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::clipboard::Clipboard;
use crate::geom::{Cuboid, Cursor, Pos, Shape};
use crate::history::{Change, Record};
use crate::pattern::{Blocks, Mask, Pattern};
use crate::transform::transform_state;

/// What a job needs from the world.
pub trait WorldAccess {
    fn loaded(&mut self, chunk: (i32, i32)) -> bool;
    fn get(&mut self, p: Pos) -> u16;
    /// Queues one block write for the next [`Self::flush`]; nothing is written yet.
    fn queue(&mut self, p: Pos, state: u16);
    /// Writes the queued blocks in one call, in queue order, each exactly as one single write,
    /// and empties the queue. At most [`MAX_BATCH`] blocks. `Err`: the server refused the
    /// whole batch and wrote none of it.
    fn flush(&mut self) -> Result<u32, String>;
    /// Writes `state` into every block from `min` to `max` (included) in one call, each exactly
    /// as one single write. At most [`MAX_BATCH`] blocks. `Err`: refused, nothing written.
    fn fill(&mut self, min: Pos, max: Pos, state: u16) -> Result<u32, String>;
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

/// Blocks between two looks at the clock: a block costs far more than the look does, so a step
/// overruns its time budget by at most this many blocks.
pub const CLOCK_EVERY: u64 = 8;

/// The most blocks of one write call (the server's limit, that of `/fill`).
pub const MAX_BATCH: usize = 32_768;

/// What one queued write is first taken to cost, in nanoseconds, until a sent batch measured it.
const FIRST_WRITE_NS: u64 = 1_000;

/// A batch smaller than this does not change the measured write cost (too short to time).
const LEARN_FROM: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub micros: u64,
    pub blocks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The budget ran out; more work is left.
    More,
    /// The next block is in this chunk, which is not loaded.
    Blocked((i32, i32)),
    Done,
    /// The server refused a write batch (its answer). Nothing of that batch was written or is
    /// in the record; the job ends here.
    Refused(String),
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

/// The record's lengths at the last write the server took.
#[derive(Debug, Clone, Copy, Default)]
struct Marks {
    changes: usize,
    old_entities: usize,
    new_entities: usize,
}

impl Marks {
    const fn of(record: &Record) -> Self {
        Self {
            changes: record.changes.len(),
            old_entities: record.old_entities.len(),
            new_entities: record.new_entities.len(),
        }
    }
}

/// Positions of a fill of one state that are read and not written yet: `first` to `last` in
/// cursor order, all inside one cut box.
#[derive(Debug, Clone, Copy)]
struct Run {
    box_index: usize,
    bounds: Cuboid,
    first: Pos,
    last: Pos,
    state: u16,
    /// Whether any block of the run differs from the state (else nothing is sent).
    changes: bool,
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
    /// Blocks read and not yet sent (queued writes, or the positions of the open run).
    pending: usize,
    marks: Marks,
    /// What one write costs, in nanoseconds, measured on the batches sent: the read loop keeps
    /// time for the writes it queues, so a step with its writes stays inside its budget.
    write_ns: u64,
    /// For a fill of one state everywhere: that state (decided at the first step).
    uniform: Option<Option<u16>>,
    run: Option<Run>,
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
            pending: 0,
            marks: Marks::default(),
            write_ns: FIRST_WRITE_NS,
            uniform: None,
            run: None,
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

    /// Records a change (with the block entity data it replaces) and queues its write.
    fn write(&mut self, world: &mut dyn WorldAccess, p: Pos, old: u16, new: u16) {
        if self.entity_data
            && world.has_entity(old)
            && let Some(data) = world.get_entity(p)
        {
            self.record.old_entities.push((p, data));
        }
        world.queue(p, new);
        self.pending += 1;
        self.record.changes.push(Change { pos: p, old, new });
        self.changed += 1;
    }

    /// Microseconds this step has used, with the time its unsent writes will take.
    fn spent(&self, world: &mut dyn WorldAccess, start: u64) -> u64 {
        let writes = (self.pending as u64).saturating_mul(self.write_ns) / 1000;
        world.now_us().saturating_sub(start).saturating_add(writes)
    }

    /// Runs until the budget is used, a chunk is missing or the job is done.
    pub fn step(
        &mut self,
        world: &mut dyn WorldAccess,
        blocks: &dyn Blocks,
        budget: Budget,
    ) -> Step {
        match self.work {
            Work::Restore { .. } => self.step_restore(world, budget),
            Work::Biome { .. } => self.step_biome(world, budget),
            _ => {
                let step = self.step_blocks(world, blocks, budget);
                if matches!(step, Step::Refused(_)) {
                    return step;
                }
                // Every step ends with its writes sent: nothing waits across ticks.
                match self.flush(world) {
                    Ok(()) => step,
                    Err(e) => Step::Refused(e),
                }
            }
        }
    }

    fn step_blocks(
        &mut self,
        world: &mut dyn WorldAccess,
        blocks: &dyn Blocks,
        budget: Budget,
    ) -> Step {
        let uniform = *self
            .uniform
            .get_or_insert_with(|| uniform_state(&self.work, world));
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
                    && self.spent(world, start) >= budget.micros)
            {
                return Step::More;
            }
            if self.pending >= MAX_BATCH
                && let Err(e) = self.flush(world)
            {
                return Step::Refused(e);
            }
            let Some((box_index, bounds)) = self.cursor.current_box() else {
                return Step::Done;
            };
            let Some(p) = self.cursor.next() else {
                return Step::Done;
            };
            self.done += 1;
            count += 1;
            if let Some(state) = uniform {
                if let Err(e) = self.visit_uniform(world, box_index, bounds, p, state) {
                    return Step::Refused(e);
                }
            } else {
                self.visit(world, blocks, p);
            }
        }
    }

    /// One position of a fill of one state: read and recorded now, written with its run.
    fn visit_uniform(
        &mut self,
        world: &mut dyn WorldAccess,
        box_index: usize,
        bounds: Cuboid,
        p: Pos,
        state: u16,
    ) -> Result<(), String> {
        if self.run.is_some_and(|r| r.box_index != box_index) {
            self.flush(world)?;
        }
        let old = world.get(p);
        let changes = old != state;
        if changes {
            if self.entity_data
                && world.has_entity(old)
                && let Some(data) = world.get_entity(p)
            {
                self.record.old_entities.push((p, data));
            }
            self.record.changes.push(Change {
                pos: p,
                old,
                new: state,
            });
            self.changed += 1;
        }
        let run = self.run.get_or_insert(Run {
            box_index,
            bounds,
            first: p,
            last: p,
            state,
            changes: false,
        });
        run.last = p;
        run.changes |= changes;
        self.pending += 1;
        Ok(())
    }

    /// Sends what this job has read and not yet written: the open run as boxes, or the queued
    /// writes as one batch, then the block entity data of a paste. A refused call takes its
    /// changes back out of the record (it wrote nothing) and ends the job.
    fn flush(&mut self, world: &mut dyn WorldAccess) -> Result<(), String> {
        let n = std::mem::take(&mut self.pending);
        let run = self.run.take();
        if n == 0 || run.is_some_and(|r| !r.changes) {
            return Ok(());
        }
        let start = world.now_us();
        if let Some(run) = run {
            let pieces = run_boxes(run.bounds, run.first, run.last, MAX_BATCH as u64);
            for (k, piece) in pieces.iter().enumerate() {
                if let Err(e) = world.fill(piece.min, piece.max, run.state) {
                    // The pieces before this one are written and keep their changes.
                    let written = |p: Pos| pieces[..k].iter().any(|b| b.contains(p));
                    let changes = self.record.changes[self.marks.changes..]
                        .iter()
                        .take_while(|c| written(c.pos))
                        .count();
                    let old_entities = self.record.old_entities[self.marks.old_entities..]
                        .iter()
                        .take_while(|(p, _)| written(*p))
                        .count();
                    self.take_back(Marks {
                        changes: self.marks.changes + changes,
                        old_entities: self.marks.old_entities + old_entities,
                        new_entities: self.marks.new_entities,
                    });
                    return Err(e);
                }
            }
        } else if let Err(e) = world.flush() {
            self.take_back(self.marks);
            return Err(e);
        } else {
            for (p, data) in &self.record.new_entities[self.marks.new_entities..] {
                world.set_entity(*p, data);
            }
        }
        learn(&mut self.write_ns, world.now_us().saturating_sub(start), n);
        self.marks = Marks::of(&self.record);
        Ok(())
    }

    /// Drops the changes after `to` from the record: the server did not write them.
    fn take_back(&mut self, to: Marks) {
        let dropped = self.record.changes.len().saturating_sub(to.changes);
        self.record.changes.truncate(to.changes);
        self.record.old_entities.truncate(to.old_entities);
        self.record.new_entities.truncate(to.new_entities);
        self.changed = self.changed.saturating_sub(dropped as u64);
        self.marks = to;
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
                // Given to the block once its batch is written (see `flush`).
                if let Some(data) = entity {
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
            let stop = if checked == Some(chunk) {
                None
            } else if world.loaded(chunk) {
                checked = Some(chunk);
                None
            } else {
                Some(Step::Blocked(chunk))
            };
            let writes = (self.pending as u64).saturating_mul(self.write_ns) / 1000;
            let stop = stop.or_else(|| {
                (count >= budget.blocks
                    || (count.is_multiple_of(CLOCK_EVERY)
                        && count > 0
                        && world.now_us().saturating_sub(start).saturating_add(writes)
                            >= budget.micros))
                    .then_some(Step::More)
            });
            if stop.is_some() || self.pending >= MAX_BATCH {
                let sent = std::mem::take(&mut self.pending);
                if let Err(e) = send_restore(world, sent, &mut self.write_ns) {
                    // That batch wrote nothing: its changes are not done, and the job ends.
                    *index -= sent;
                    self.done -= sent as u64;
                    self.changed -= sent as u64;
                    return Step::Refused(e);
                }
                if let Some(stop) = stop {
                    return stop;
                }
            }
            world.queue(change.pos, if *undo { change.old } else { change.new });
            self.pending += 1;
            *index += 1;
            count += 1;
            self.done += 1;
            self.changed += 1;
        }
        let sent = std::mem::take(&mut self.pending);
        if let Err(e) = send_restore(world, sent, &mut self.write_ns) {
            *index -= sent;
            self.done -= sent as u64;
            self.changed -= sent as u64;
            return Step::Refused(e);
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

/// Sends `n` queued restore writes in one call and learns what they cost.
fn send_restore(world: &mut dyn WorldAccess, n: usize, write_ns: &mut u64) -> Result<(), String> {
    if n == 0 {
        return Ok(());
    }
    let start = world.now_us();
    world.flush()?;
    learn(write_ns, world.now_us().saturating_sub(start), n);
    Ok(())
}

/// Takes the time `n` sent writes took into the cost of one write.
fn learn(write_ns: &mut u64, micros: u64, n: usize) {
    if n >= LEARN_FROM {
        let measured = micros.saturating_mul(1000) / n as u64;
        *write_ns = (*write_ns + measured) / 2;
    }
}

/// The one state a fill job places everywhere it goes: a pattern of one block, no mask, the
/// whole box, and a state without block entity data (whose blocks the fill may write over
/// themselves). Such a fill is sent as boxes.
fn uniform_state(work: &Work, world: &mut dyn WorldAccess) -> Option<u16> {
    match work {
        Work::Fill {
            pattern,
            mask: None,
            shape: Shape::All,
            ..
        } => pattern.only().filter(|&s| !world.has_entity(s)),
        _ => None,
    }
}

/// The boxes that hold exactly the positions from `first` to `last` of a cursor walk through
/// `bounds` (x, then z, then y), in walk order, each of at most `limit` blocks: a part of the
/// first row, the rest of the first layer, whole layers, the start of the last layer, a part of
/// the last row.
#[must_use]
pub fn run_boxes(bounds: Cuboid, first: Pos, last: Pos, limit: u64) -> Vec<Cuboid> {
    let (x0, x1, z0, z1) = (bounds.min.x, bounds.max.x, bounds.min.z, bounds.max.z);
    let mut out = Vec::new();
    let mut p = first;
    // The first row, when the run starts inside it or also ends in it.
    if p.x != x0 || (p.y == last.y && p.z == last.z) {
        if p.y == last.y && p.z == last.z {
            split_box(Cuboid { min: p, max: last }, limit, &mut out);
            return out;
        }
        split_box(
            Cuboid {
                min: p,
                max: Pos::new(x1, p.y, p.z),
            },
            limit,
            &mut out,
        );
        p = if p.z < z1 {
            Pos::new(x0, p.y, p.z + 1)
        } else {
            Pos::new(x0, p.y + 1, z0)
        };
    }
    // `p` starts a row. The rest of its layer, when the run starts inside a layer or ends in it.
    if p.z != z0 || p.y == last.y {
        if p.y == last.y {
            last_rows(bounds, p.z, last, limit, &mut out);
            return out;
        }
        split_box(
            Cuboid {
                min: p,
                max: Pos::new(x1, p.y, z1),
            },
            limit,
            &mut out,
        );
        p = Pos::new(x0, p.y + 1, z0);
    }
    // `p` starts a layer: whole layers, then the start of the last one.
    let last_layer_whole = last.x == x1 && last.z == z1;
    let end_y = if last_layer_whole { last.y } else { last.y - 1 };
    if end_y >= p.y {
        split_box(
            Cuboid {
                min: p,
                max: Pos::new(x1, end_y, z1),
            },
            limit,
            &mut out,
        );
    }
    if !last_layer_whole {
        last_rows(bounds, z0, last, limit, &mut out);
    }
    out
}

/// Rows `from_z` to `last`'s row of `last`'s layer, the last row up to `last`.
fn last_rows(bounds: Cuboid, from_z: i32, last: Pos, limit: u64, out: &mut Vec<Cuboid>) {
    let (x0, x1) = (bounds.min.x, bounds.max.x);
    let whole_last_row = last.x == x1;
    let end_z = if whole_last_row { last.z } else { last.z - 1 };
    if end_z >= from_z {
        split_box(
            Cuboid {
                min: Pos::new(x0, last.y, from_z),
                max: Pos::new(x1, last.y, end_z),
            },
            limit,
            out,
        );
    }
    if !whole_last_row {
        split_box(
            Cuboid {
                min: Pos::new(x0, last.y, last.z),
                max: last,
            },
            limit,
            out,
        );
    }
}

/// Cuts `c` into boxes of at most `limit` blocks, in walk order: layers, else rows, else parts
/// of rows.
fn split_box(c: Cuboid, limit: u64, out: &mut Vec<Cuboid>) {
    let limit = limit.max(1);
    let (w, h, d) = c.size();
    if w * h * d <= limit {
        out.push(c);
        return;
    }
    // Steps are at most the side they cut, so they fit an i32 coordinate.
    let cut = |from: i32,
               to: i32,
               step: u64,
               out: &mut Vec<Cuboid>,
               make: &dyn Fn(i32, i32) -> Cuboid| {
        let step = i64::try_from(step).unwrap_or(i64::MAX);
        let mut a = i64::from(from);
        while a <= i64::from(to) {
            let b = a.saturating_add(step - 1).min(i64::from(to));
            out.push(make(a as i32, b as i32));
            a = b + 1;
        }
    };
    if w * d <= limit {
        cut(c.min.y, c.max.y, limit / (w * d), out, &|y0, y1| Cuboid {
            min: Pos::new(c.min.x, y0, c.min.z),
            max: Pos::new(c.max.x, y1, c.max.z),
        });
        return;
    }
    for y in c.min.y..=c.max.y {
        if w <= limit {
            cut(c.min.z, c.max.z, limit / w, out, &|z0, z1| Cuboid {
                min: Pos::new(c.min.x, y, z0),
                max: Pos::new(c.max.x, y, z1),
            });
        } else {
            for z in c.min.z..=c.max.z {
                cut(c.min.x, c.max.x, limit, out, &|x0, x1| Cuboid {
                    min: Pos::new(x0, y, z),
                    max: Pos::new(x1, y, z),
                });
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
        /// Writes queued for the next flush.
        pub queued: Vec<(Pos, u16)>,
        /// Write calls the server took: batches, fills, and the most blocks one carried.
        pub batches: u64,
        pub fills: u64,
        pub largest: u64,
        /// Write calls made, refused ones included.
        pub calls: u64,
        /// The write call (1 = the first) the server refuses, writing nothing.
        pub refuse_call: Option<u64>,
    }

    impl FakeWorld {
        /// One block write, as the server makes it.
        fn put(&mut self, p: Pos, state: u16) -> bool {
            self.clock += 1;
            self.writes += 1;
            assert!(
                !self.unloaded.contains(&p.chunk()),
                "write to an unloaded chunk"
            );
            if !self.entity_states.contains(&state) {
                self.entities.remove(&p);
            }
            self.blocks.insert(p, state) != Some(state)
        }

        /// The server's answer to a write call of `blocks` blocks.
        fn call(&mut self, blocks: u64) -> Result<(), String> {
            self.calls += 1;
            if blocks > MAX_BATCH as u64 {
                return Err(format!("{blocks} blocks is more than {MAX_BATCH}"));
            }
            if self.refuse_call == Some(self.calls) {
                return Err("refused".to_owned());
            }
            self.largest = self.largest.max(blocks);
            Ok(())
        }
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
        fn queue(&mut self, p: Pos, state: u16) {
            self.queued.push((p, state));
        }
        fn flush(&mut self) -> Result<u32, String> {
            let queued = std::mem::take(&mut self.queued);
            self.call(queued.len() as u64)?;
            self.batches += 1;
            Ok(queued.into_iter().filter(|&(p, s)| self.put(p, s)).count() as u32)
        }
        fn fill(&mut self, min: Pos, max: Pos, state: u16) -> Result<u32, String> {
            let c = Cuboid { min, max };
            self.call(c.volume())?;
            self.fills += 1;
            Ok(Cursor::new(&[c]).filter(|&p| self.put(p, state)).count() as u32)
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

    /// The state at `p`, air where nothing was written.
    fn at(world: &FakeWorld, p: Pos) -> u16 {
        world.blocks.get(&p).copied().unwrap_or(0)
    }

    fn undo_of(record: Record) -> Job {
        Job::new(
            "world",
            &[],
            Work::Restore {
                record,
                undo: true,
                index: 0,
                entities_done: false,
                biome_index: 0,
            },
        )
    }

    fn replace_air(c: Cuboid, state: u16) -> Job {
        let blocks = FakeBlocks::default();
        Job::new(
            "world",
            &[c],
            Work::Fill {
                pattern: Pattern::single(state),
                mask: Some(Mask::parse("air", &blocks).unwrap()),
                shape: Shape::All,
                seed: 0,
            },
        )
    }

    #[test]
    fn run_boxes_hold_exactly_the_walk_in_order_within_the_limit() {
        let b = Cuboid::from_corners(Pos::new(1, 2, 3), Pos::new(3, 4, 5));
        let all: Vec<Pos> = Cursor::new(&[b]).collect();
        for limit in [1, 2, 3, 4, 7, 9, 27, 1000] {
            for i in 0..all.len() {
                for j in i..all.len() {
                    let boxes = run_boxes(b, all[i], all[j], limit);
                    assert!(boxes.iter().all(|c| c.volume() <= limit));
                    let got: Vec<Pos> = boxes.iter().flat_map(|c| Cursor::new(&[*c])).collect();
                    assert_eq!(got, all[i..=j], "limit {limit}, {i}..={j}");
                    if limit >= 27 {
                        assert!(boxes.len() <= 5, "{i}..={j}: {boxes:?}");
                    }
                }
            }
        }
        assert_eq!(
            run_boxes(b, all[0], all[26], 27),
            vec![b],
            "a whole box is one call"
        );
    }

    #[test]
    fn a_fill_of_one_state_is_sent_as_boxes_within_the_call_limit() {
        let mut world = FakeWorld::default();
        world.blocks.insert(Pos::new(3, 7, 3), 1);
        let c = Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(15, 199, 15));
        let mut job = fill(c, 1);
        assert_eq!(run(&mut job, &mut world, BIG).0, Step::Done);
        assert_eq!(world.batches, 0, "no list call");
        assert_eq!(world.fills, 2, "51,200 blocks: two boxes of at most 32,768");
        assert!(world.largest <= MAX_BATCH as u64);
        assert_eq!(job.changed, 51_199);
        assert_eq!(job.record.changes.len(), 51_199);
        assert!(
            job.record
                .changes
                .iter()
                .all(|ch| ch.pos != Pos::new(3, 7, 3) && ch.old == 0 && ch.new == 1)
        );
        assert!(Cursor::new(&[c]).all(|p| at(&world, p) == 1));
    }

    #[test]
    fn a_fill_that_changes_nothing_sends_nothing() {
        let mut world = FakeWorld::default();
        let c = Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(3, 3, 3));
        for p in Cursor::new(&[c]) {
            world.blocks.insert(p, 1);
        }
        let mut job = fill(c, 1);
        run(&mut job, &mut world, BIG);
        assert_eq!((world.calls, world.writes, job.changed), (0, 0, 0));
        assert!(job.record.is_empty());
    }

    #[test]
    fn a_replace_is_sent_as_lists_within_the_call_limit() {
        let mut world = FakeWorld::default();
        world.blocks.insert(Pos::new(5, 5, 5), 2);
        let c = Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(15, 199, 15));
        let mut job = replace_air(c, 1);
        assert_eq!(run(&mut job, &mut world, BIG).0, Step::Done);
        assert_eq!(world.fills, 0, "a mask is never a box");
        assert_eq!(world.batches, 2);
        assert_eq!(world.largest, MAX_BATCH as u64);
        assert_eq!(world.writes, 51_199, "only the changes are sent");
        assert_eq!(job.record.changes.len(), 51_199);
        assert_eq!(at(&world, Pos::new(5, 5, 5)), 2);
    }

    #[test]
    fn a_fill_with_block_entity_data_goes_as_a_list_and_leaves_equal_blocks_alone() {
        let mut world = FakeWorld::default();
        world.entity_states.insert(9);
        world.blocks.insert(Pos::new(0, 0, 0), 9);
        world.entities.insert(Pos::new(0, 0, 0), vec![7]);
        let mut job = fill(
            Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(3, 0, 0)),
            9,
        );
        run(&mut job, &mut world, BIG);
        assert_eq!((world.fills, world.batches, world.writes), (0, 1, 3));
        assert_eq!(world.entities[&Pos::new(0, 0, 0)], vec![7]);
        assert_eq!(job.changed, 3);
    }

    #[test]
    fn a_step_keeps_time_for_its_writes() {
        // Reads and writes each cost 1 µs here; the first estimate of a write is 1 µs too.
        let mut world = FakeWorld::default();
        let blocks = FakeBlocks::default();
        let mut job = replace_air(
            Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(15, 15, 15)),
            1,
        );
        let budget = Budget {
            micros: 200,
            blocks: u64::MAX,
        };
        assert_eq!(job.step(&mut world, &blocks, budget), Step::More);
        assert_eq!(world.batches, 1, "one call for the step");
        assert!(
            world.clock <= 200 + 2 * CLOCK_EVERY,
            "step with its writes took {} µs",
            world.clock
        );
    }

    #[test]
    fn a_refused_list_leaves_its_changes_out_and_undo_is_exact() {
        let mut world = FakeWorld::default();
        world.entity_states.insert(9);
        world.blocks.insert(Pos::new(3, 0, 0), 9);
        world.entities.insert(Pos::new(3, 0, 0), vec![1]);
        world.blocks.insert(Pos::new(13, 0, 0), 9);
        world.entities.insert(Pos::new(13, 0, 0), vec![2]);
        let c = Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(29, 0, 0));
        let before: Vec<u16> = Cursor::new(&[c]).map(|p| at(&world, p)).collect();
        let blocks = FakeBlocks::default();
        let mut job = Job::new(
            "world",
            &[c],
            Work::Fill {
                pattern: Pattern::parse("stone,dirt", &blocks).unwrap(),
                mask: None,
                shape: Shape::All,
                seed: 3,
            },
        );
        world.refuse_call = Some(2);
        let (end, _) = run(
            &mut job,
            &mut world,
            Budget {
                micros: u64::MAX,
                blocks: 10,
            },
        );
        assert_eq!(end, Step::Refused("refused".to_owned()));
        // Only the first batch (x 0 to 9) was written, and only it is in the record.
        assert!(job.record.changes.iter().all(|ch| ch.pos.x < 10));
        assert_eq!(job.changed, job.record.changes.len() as u64);
        assert_eq!(job.record.old_entities, vec![(Pos::new(3, 0, 0), vec![1])]);
        assert!((10..30).all(|x| at(&world, Pos::new(x, 0, 0)) == before[x as usize]));
        assert_eq!(world.entities[&Pos::new(13, 0, 0)], vec![2]);

        let mut undo = undo_of(job.record);
        assert_eq!(run(&mut undo, &mut world, BIG).0, Step::Done);
        let after: Vec<u16> = Cursor::new(&[c]).map(|p| at(&world, p)).collect();
        assert_eq!(after, before);
        assert_eq!(world.entities[&Pos::new(3, 0, 0)], vec![1]);
    }

    #[test]
    fn a_refused_box_keeps_the_boxes_written_before_it_and_undo_is_exact() {
        let mut world = FakeWorld::default();
        world.entity_states.insert(9);
        world.blocks.insert(Pos::new(3, 0, 0), 9);
        world.entities.insert(Pos::new(3, 0, 0), vec![1]);
        world.blocks.insert(Pos::new(0, 1, 1), 9);
        world.entities.insert(Pos::new(0, 1, 1), vec![2]);
        let c = Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(3, 2, 3));
        let mut job = fill(c, 1);
        let blocks = FakeBlocks::default();
        let two = Budget {
            micros: u64::MAX,
            blocks: 2,
        };
        assert_eq!(job.step(&mut world, &blocks, two), Step::More);
        assert_eq!(world.fills, 1);
        // The rest is three boxes: the end of the first row, the rest of the first layer, two
        // whole layers. The second is refused, so the third is never sent.
        world.refuse_call = Some(3);
        assert_eq!(
            job.step(&mut world, &blocks, BIG),
            Step::Refused("refused".to_owned())
        );
        assert_eq!(world.calls, 3);
        let written: Vec<Pos> = job.record.changes.iter().map(|ch| ch.pos).collect();
        assert_eq!(
            written,
            (0..4).map(|x| Pos::new(x, 0, 0)).collect::<Vec<_>>()
        );
        assert_eq!(job.changed, 4);
        assert_eq!(job.record.old_entities, vec![(Pos::new(3, 0, 0), vec![1])]);
        for p in Cursor::new(&[c]) {
            let expect = if written.contains(&p) {
                1
            } else if p == Pos::new(0, 1, 1) {
                9
            } else {
                0
            };
            assert_eq!(at(&world, p), expect, "{p:?}");
        }

        let mut undo = undo_of(job.record);
        assert_eq!(run(&mut undo, &mut world, BIG).0, Step::Done);
        assert_eq!(at(&world, Pos::new(3, 0, 0)), 9);
        assert_eq!(world.entities[&Pos::new(3, 0, 0)], vec![1]);
        assert!((0..3).all(|x| at(&world, Pos::new(x, 0, 0)) == 0));
    }

    #[test]
    fn a_refused_undo_batch_counts_nothing_of_it() {
        let mut world = FakeWorld::default();
        let c = Cuboid::from_corners(Pos::new(0, 0, 0), Pos::new(29, 0, 0));
        let mut job = fill(c, 1);
        run(&mut job, &mut world, BIG);
        let mut undo = undo_of(job.record);
        world.refuse_call = Some(world.calls + 2);
        let (end, _) = run(
            &mut undo,
            &mut world,
            Budget {
                micros: u64::MAX,
                blocks: 10,
            },
        );
        assert_eq!(end, Step::Refused("refused".to_owned()));
        assert_eq!((undo.done, undo.changed), (10, 10));
        // Undo walks back from the last change: x 20 to 29 are air again, the rest is not.
        assert!((0..30).all(|x| at(&world, Pos::new(x, 0, 0)) == u16::from(x < 20)));
        let Work::Restore { index, .. } = undo.work else {
            unreachable!()
        };
        assert_eq!(index, 10);
    }
}
