//! The clipboard: a box of block states and block entity data, kept relative to where the
//! player stood when copying, with the rotation and flips still to apply.
//!
//! A paste walks the target box chunk by chunk and looks each position up through the inverse
//! transform, so rotating or flipping never rebuilds the arrays.

use std::collections::BTreeMap;

use crate::geom::{Cuboid, Pos};
use crate::transform::Transform;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipboard {
    /// Width (x), height (y), length (z).
    pub size: (u32, u32, u32),
    /// The box's lowest corner relative to the copy point.
    pub offset: Pos,
    /// Where the box's lowest corner was in the world when copied.
    pub origin: Pos,
    /// States, index `(y * length + z) * width + x`.
    pub blocks: Vec<u16>,
    /// Block entity data (server bytes) by block index.
    pub entities: BTreeMap<usize, Vec<u8>>,
    pub transform: Transform,
}

impl Clipboard {
    /// An empty clipboard of `size`, filled with state 0, or none when the size is zero or
    /// does not fit in memory terms.
    #[must_use]
    pub fn new(size: (u32, u32, u32), offset: Pos, origin: Pos) -> Option<Self> {
        let volume = u64::from(size.0) * u64::from(size.1) * u64::from(size.2);
        if volume == 0 || volume > u64::from(u32::MAX) {
            return None;
        }
        Some(Self {
            size,
            offset,
            origin,
            blocks: vec![0; usize::try_from(volume).ok()?],
            entities: BTreeMap::new(),
            transform: Transform::IDENTITY,
        })
    }

    #[must_use]
    pub fn volume(&self) -> u64 {
        self.blocks.len() as u64
    }

    /// The bytes this clipboard holds.
    #[must_use]
    pub fn cost(&self) -> usize {
        self.blocks.len() * 2 + self.entities.values().map(|b| b.len() + 16).sum::<usize>()
    }

    #[must_use]
    pub fn index(&self, x: u32, y: u32, z: u32) -> usize {
        ((y as usize * self.size.2 as usize) + z as usize) * self.size.0 as usize + x as usize
    }

    /// The cell at a position relative to the copy point (before the transform), if inside.
    #[must_use]
    pub fn cell(&self, relative: Pos) -> Option<usize> {
        let x = i64::from(relative.x) - i64::from(self.offset.x);
        let y = i64::from(relative.y) - i64::from(self.offset.y);
        let z = i64::from(relative.z) - i64::from(self.offset.z);
        let inside = |v: i64, n: u32| (0..i64::from(n)).contains(&v);
        (inside(x, self.size.0) && inside(y, self.size.1) && inside(z, self.size.2))
            .then(|| self.index(x as u32, y as u32, z as u32))
    }

    /// The world box a paste at `at` (the player's block) covers, with the transform.
    #[must_use]
    pub fn target(&self, at: Pos) -> Cuboid {
        let (w, h, l) = (
            self.size.0 as i32 - 1,
            self.size.1 as i32 - 1,
            self.size.2 as i32 - 1,
        );
        let a = self.transform.apply(self.offset);
        let b = self.transform.apply(self.offset.offset(w, h, l));
        let rel = Cuboid::from_corners(a, b);
        Cuboid::from_corners(
            at.offset(rel.min.x, rel.min.y, rel.min.z),
            at.offset(rel.max.x, rel.max.y, rel.max.z),
        )
    }

    /// The cell that lands on world position `p` in a paste at `at`.
    #[must_use]
    pub fn source(&self, at: Pos, p: Pos) -> Option<usize> {
        let d = Pos::new(
            p.x.wrapping_sub(at.x),
            p.y.wrapping_sub(at.y),
            p.z.wrapping_sub(at.z),
        );
        self.cell(self.transform.inverse().apply(d))
    }

    /// The paste point that puts the box back where it was copied (`//paste -o`).
    #[must_use]
    pub fn original_point(&self) -> Pos {
        Pos::new(
            self.origin.x.wrapping_sub(self.offset.x),
            self.origin.y.wrapping_sub(self.offset.y),
            self.origin.z.wrapping_sub(self.offset.z),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Cursor;

    fn sample() -> Clipboard {
        // A 3 x 2 x 2 box whose lowest corner was one block east of the player.
        let mut c = Clipboard::new((3, 2, 2), Pos::new(1, 0, 0), Pos::new(11, 64, 0)).unwrap();
        for (i, b) in c.blocks.iter_mut().enumerate() {
            *b = i as u16 + 1;
        }
        c
    }

    #[test]
    fn a_plain_paste_puts_every_cell_once() {
        let c = sample();
        let at = Pos::new(100, 70, 100);
        let target = c.target(at);
        assert_eq!(
            target,
            Cuboid::from_corners(Pos::new(101, 70, 100), Pos::new(103, 71, 101))
        );
        let mut seen: Vec<usize> = Cursor::new(&[target])
            .filter_map(|p| c.source(at, p))
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..12).collect::<Vec<_>>());
        assert_eq!(c.original_point(), Pos::new(10, 64, 0));
    }

    #[test]
    fn a_rotated_paste_turns_around_the_paste_point() {
        let mut c = sample();
        c.transform = Transform::rotate_y(1);
        let at = Pos::new(0, 0, 0);
        let target = c.target(at);
        // East of the player becomes south.
        assert_eq!(
            target,
            Cuboid::from_corners(Pos::new(-1, 0, 1), Pos::new(0, 1, 3))
        );
        let cells: Vec<usize> = Cursor::new(&[target])
            .filter_map(|p| c.source(at, p))
            .collect();
        assert_eq!(cells.len(), 12);
        // The cell nearest the player (x offset 1) lands one block south.
        assert_eq!(c.source(at, Pos::new(0, 0, 1)), Some(c.index(0, 0, 0)));
        assert!(c.source(at, Pos::new(1, 0, 0)).is_none());
    }

    #[test]
    fn sizes_are_checked() {
        assert!(Clipboard::new((0, 1, 1), Pos::new(0, 0, 0), Pos::new(0, 0, 0)).is_none());
        let c = sample();
        assert_eq!(c.volume(), 12);
        assert_eq!(c.cost(), 24);
    }
}
