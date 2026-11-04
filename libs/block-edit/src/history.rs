//! Undo and redo, per player, with a memory limit per player and one for the whole server.
//!
//! Every edit leaves one record: the old and new state of each block it changed, and the old
//! and new block entity data where there was any. A record's cost is counted in bytes. When a
//! new record does not fit, the player's oldest records go first; when the server's total is
//! over its limit, the oldest records of anyone go.

use std::collections::{HashMap, VecDeque};

use crate::geom::Pos;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub pos: Pos,
    pub old: u16,
    pub new: u16,
}

/// The bytes one change costs in memory.
pub const CHANGE_BYTES: usize = std::mem::size_of::<Change>();

/// One biome cell (4 × 4 × 4 blocks, cell coordinates) and its biome before and after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BiomeChange {
    pub cell: Pos,
    pub old: u8,
    pub new: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub world: String,
    pub changes: Vec<Change>,
    /// Block entity data (server bytes) before the edit, by position.
    pub old_entities: Vec<(Pos, Vec<u8>)>,
    /// Block entity data the edit wrote, by position.
    pub new_entities: Vec<(Pos, Vec<u8>)>,
    pub biomes: Vec<BiomeChange>,
}

impl Record {
    #[must_use]
    pub fn cost(&self) -> usize {
        let entities: usize = self
            .old_entities
            .iter()
            .chain(&self.new_entities)
            .map(|(_, b)| b.len() + 16)
            .sum();
        self.changes.len() * CHANGE_BYTES
            + self.biomes.len() * std::mem::size_of::<BiomeChange>()
            + entities
            + self.world.len()
            + 64
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
            && self.old_entities.is_empty()
            && self.new_entities.is_empty()
            && self.biomes.is_empty()
    }
}

#[derive(Debug, Default)]
struct Player {
    /// (age, record), oldest first.
    undo: VecDeque<(u64, Record)>,
    redo: Vec<Record>,
}

#[derive(Debug)]
pub struct History {
    players: HashMap<String, Player>,
    pub per_player_bytes: usize,
    pub total_bytes: usize,
    /// The most edits each player keeps to undo; 0 is no step limit (memory still bounds it).
    pub max_steps: usize,
    used: usize,
    age: u64,
}

impl History {
    #[must_use]
    pub fn new(per_player_bytes: usize, total_bytes: usize) -> Self {
        Self {
            players: HashMap::new(),
            per_player_bytes,
            total_bytes,
            max_steps: 0,
            used: 0,
            age: 0,
        }
    }

    #[must_use]
    pub const fn used(&self) -> usize {
        self.used
    }

    fn player_used(&self, player: &str) -> usize {
        self.players.get(player).map_or(0, |p| {
            p.undo.iter().map(|(_, r)| r.cost()).sum::<usize>()
                + p.redo.iter().map(Record::cost).sum::<usize>()
        })
    }

    /// Whether a record of `cost` bytes can be kept at all.
    #[must_use]
    pub const fn fits(&self, cost: usize) -> bool {
        cost <= self.per_player_bytes && cost <= self.total_bytes
    }

    /// Keeps `record` as the player's newest edit; the redo list is cleared. Returns false
    /// (and keeps nothing) when the record alone is over a limit.
    pub fn push(&mut self, player: &str, record: Record) -> bool {
        if record.is_empty() {
            return true;
        }
        let cost = record.cost();
        if !self.fits(cost) {
            return false;
        }
        let redo: usize = self
            .players
            .get(player)
            .map_or(0, |p| p.redo.iter().map(Record::cost).sum());
        if let Some(p) = self.players.get_mut(player) {
            p.redo.clear();
        }
        self.used -= redo;
        while self.player_used(player) + cost > self.per_player_bytes {
            if !self.drop_oldest_of(player) {
                break;
            }
        }
        while self.used + cost > self.total_bytes {
            if !self.drop_oldest_anywhere() {
                break;
            }
        }
        if self.max_steps > 0 {
            while self
                .players
                .get(player)
                .is_some_and(|p| p.undo.len() >= self.max_steps)
            {
                if !self.drop_oldest_of(player) {
                    break;
                }
            }
        }
        self.age += 1;
        let age = self.age;
        self.players
            .entry(player.to_owned())
            .or_default()
            .undo
            .push_back((age, record));
        self.used += cost;
        true
    }

    fn drop_oldest_of(&mut self, player: &str) -> bool {
        let Some(p) = self.players.get_mut(player) else {
            return false;
        };
        if let Some((_, r)) = p.undo.pop_front() {
            self.used -= r.cost();
            return true;
        }
        if !p.redo.is_empty() {
            let r = p.redo.remove(0);
            self.used -= r.cost();
            return true;
        }
        false
    }

    fn drop_oldest_anywhere(&mut self) -> bool {
        let oldest = self
            .players
            .iter()
            .filter_map(|(name, p)| p.undo.front().map(|(age, _)| (*age, name.clone())))
            .min();
        if let Some((_, name)) = oldest {
            return self.drop_oldest_of(&name);
        }
        // Only redo records are left.
        let any = self
            .players
            .iter()
            .find(|(_, p)| !p.redo.is_empty())
            .map(|(n, _)| n.clone());
        any.is_some_and(|name| self.drop_oldest_of(&name))
    }

    /// Forgets every record (undo and redo) of world, for every player: the world was unloaded,
    /// and a world loaded later under the same name is another world. Returns the players who lost
    /// records.
    pub fn forget_world(&mut self, world: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (name, p) in &mut self.players {
            let before = p.undo.len() + p.redo.len();
            let mut freed = 0;
            p.undo.retain(|(_, r)| {
                let keep = r.world != world;
                if !keep {
                    freed += r.cost();
                }
                keep
            });
            p.redo.retain(|r| {
                let keep = r.world != world;
                if !keep {
                    freed += r.cost();
                }
                keep
            });
            self.used -= freed;
            if p.undo.len() + p.redo.len() != before {
                out.push(name.clone());
            }
        }
        out.sort();
        out
    }

    /// The newest edit to undo; it moves to the redo list.
    pub fn take_undo(&mut self, player: &str) -> Option<Record> {
        let p = self.players.get_mut(player)?;
        let (_, record) = p.undo.pop_back()?;
        p.redo.push(record.clone());
        Some(record)
    }

    /// The newest undone edit to do again; it moves back to the undo list.
    pub fn take_redo(&mut self, player: &str) -> Option<Record> {
        let p = self.players.get_mut(player)?;
        let record = p.redo.pop()?;
        self.age += 1;
        p.undo.push_back((self.age, record.clone()));
        Some(record)
    }

    pub fn clear(&mut self, player: &str) {
        let used = self.player_used(player);
        self.players.remove(player);
        self.used -= used;
    }

    #[must_use]
    pub fn counts(&self, player: &str) -> (usize, usize) {
        self.players
            .get(player)
            .map_or((0, 0), |p| (p.undo.len(), p.redo.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(n: usize) -> Record {
        Record {
            world: "world".into(),
            changes: (0..n)
                .map(|i| Change {
                    pos: Pos::new(i as i32, 0, 0),
                    old: 0,
                    new: 1,
                })
                .collect(),
            ..Record::default()
        }
    }

    #[test]
    fn undo_and_redo_walk_the_stack() {
        let mut h = History::new(1 << 20, 1 << 22);
        assert!(h.push("a", record(3)));
        assert!(h.push("a", record(5)));
        assert_eq!(h.take_undo("a").unwrap().changes.len(), 5);
        assert_eq!(h.take_undo("a").unwrap().changes.len(), 3);
        assert!(h.take_undo("a").is_none());
        assert_eq!(h.take_redo("a").unwrap().changes.len(), 3);
        assert_eq!(h.counts("a"), (1, 1));
        // A new edit clears what could be redone.
        assert!(h.push("a", record(1)));
        assert_eq!(h.counts("a"), (2, 0));
        assert!(h.take_redo("a").is_none());
        assert!(h.take_undo("b").is_none());
    }

    #[test]
    fn the_player_limit_drops_the_oldest_edits_first() {
        let one = record(100).cost();
        let mut h = History::new(one * 3, one * 100);
        for _ in 0..5 {
            assert!(h.push("a", record(100)));
        }
        assert_eq!(h.counts("a"), (3, 0));
        assert_eq!(h.used(), one * 3);
        // An edit bigger than the whole limit is not kept, and nothing else is dropped.
        assert!(!h.push("a", record(1000)));
        assert_eq!(h.counts("a"), (3, 0));
    }

    #[test]
    fn the_step_limit_keeps_the_newest_edits() {
        let mut h = History::new(1 << 20, 1 << 22);
        h.max_steps = 3;
        for n in 1..=5 {
            assert!(h.push("a", record(n)));
        }
        assert_eq!(h.counts("a"), (3, 0));
        assert_eq!(h.take_undo("a").unwrap().changes.len(), 5);
        assert_eq!(h.take_undo("a").unwrap().changes.len(), 4);
        assert_eq!(h.take_undo("a").unwrap().changes.len(), 3);
        assert!(h.take_undo("a").is_none());
        assert_eq!(h.used(), (3..=5).map(|n| record(n).cost()).sum::<usize>());
    }

    #[test]
    fn an_unloaded_world_takes_its_records_with_it() {
        let mut h = History::new(1 << 20, 1 << 22);
        let mut nether = record(4);
        nether.world = "nether".into();
        assert!(h.push("a", record(2)));
        assert!(h.push("a", nether.clone()));
        assert!(h.push("b", nether));
        assert!(
            h.take_undo("b").is_some(),
            "b's record is now on the redo list"
        );
        assert_eq!(
            h.forget_world("nether"),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert_eq!(h.counts("a"), (1, 0));
        assert_eq!(h.counts("b"), (0, 0));
        assert_eq!(h.used(), record(2).cost());
        assert!(h.forget_world("nether").is_empty());
    }

    #[test]
    fn the_total_limit_drops_the_oldest_edit_of_anyone() {
        let one = record(100).cost();
        let mut h = History::new(one * 10, one * 4);
        assert!(h.push("a", record(100)));
        assert!(h.push("b", record(100)));
        assert!(h.push("a", record(100)));
        assert!(h.push("b", record(100)));
        assert!(h.push("c", record(100)));
        assert_eq!(h.counts("a"), (1, 0), "a's first edit was the oldest");
        assert_eq!(h.counts("b"), (2, 0));
        assert_eq!(h.counts("c"), (1, 0));
        assert!(h.used() <= one * 4);
        h.clear("b");
        assert_eq!(h.used(), one * 2);
    }
}
