//! Positions, cuboids and the shapes an edit covers.
//!
//! An edit covers a list of disjoint cuboids, each inside one chunk column, so a batch touches
//! one chunk at a time and the progress count is exact.

/// A block position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl Pos {
    #[must_use]
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// The chunk column this position is in.
    #[must_use]
    pub const fn chunk(self) -> (i32, i32) {
        (self.x >> 4, self.z >> 4)
    }

    #[must_use]
    pub fn offset(self, dx: i32, dy: i32, dz: i32) -> Self {
        Self::new(
            self.x.saturating_add(dx),
            self.y.saturating_add(dy),
            self.z.saturating_add(dz),
        )
    }
}

/// A box of blocks, corners included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cuboid {
    pub min: Pos,
    pub max: Pos,
}

impl Cuboid {
    /// The box between two corners in any order.
    #[must_use]
    pub fn from_corners(a: Pos, b: Pos) -> Self {
        Self {
            min: Pos::new(a.x.min(b.x), a.y.min(b.y), a.z.min(b.z)),
            max: Pos::new(a.x.max(b.x), a.y.max(b.y), a.z.max(b.z)),
        }
    }

    #[must_use]
    pub fn size(&self) -> (u64, u64, u64) {
        let span = |a: i32, b: i32| (i64::from(b) - i64::from(a) + 1) as u64;
        (
            span(self.min.x, self.max.x),
            span(self.min.y, self.max.y),
            span(self.min.z, self.max.z),
        )
    }

    #[must_use]
    pub fn volume(&self) -> u64 {
        let (x, y, z) = self.size();
        x * y * z
    }

    #[must_use]
    pub const fn contains(&self, p: Pos) -> bool {
        p.x >= self.min.x
            && p.x <= self.max.x
            && p.y >= self.min.y
            && p.y <= self.max.y
            && p.z >= self.min.z
            && p.z <= self.max.z
    }

    /// The part of this box inside `y_min..=y_max`, if any.
    #[must_use]
    pub fn clamp_y(&self, y_min: i32, y_max: i32) -> Option<Self> {
        let lo = self.min.y.max(y_min);
        let hi = self.max.y.min(y_max);
        (lo <= hi).then(|| Self {
            min: Pos::new(self.min.x, lo, self.min.z),
            max: Pos::new(self.max.x, hi, self.max.z),
        })
    }

    /// The four side faces, full height, without overlaps.
    #[must_use]
    pub fn walls(&self) -> Vec<Self> {
        let (a, b) = (self.min, self.max);
        let mut out = vec![Self::from_corners(a, Pos::new(a.x, b.y, b.z))];
        if b.x > a.x {
            out.push(Self::from_corners(Pos::new(b.x, a.y, a.z), b));
        }
        if b.x - a.x >= 2 {
            out.push(Self::from_corners(
                Pos::new(a.x + 1, a.y, a.z),
                Pos::new(b.x - 1, b.y, a.z),
            ));
            if b.z > a.z {
                out.push(Self::from_corners(
                    Pos::new(a.x + 1, a.y, b.z),
                    Pos::new(b.x - 1, b.y, b.z),
                ));
            }
        }
        out
    }

    /// The six faces, without overlaps.
    #[must_use]
    pub fn outline(&self) -> Vec<Self> {
        let (a, b) = (self.min, self.max);
        let mut out = self.walls();
        if b.x - a.x >= 2 && b.z - a.z >= 2 {
            out.push(Self::from_corners(
                Pos::new(a.x + 1, a.y, a.z + 1),
                Pos::new(b.x - 1, a.y, b.z - 1),
            ));
            if b.y > a.y {
                out.push(Self::from_corners(
                    Pos::new(a.x + 1, b.y, a.z + 1),
                    Pos::new(b.x - 1, b.y, b.z - 1),
                ));
            }
        }
        out
    }

    /// This box cut along chunk borders, in chunk order (x, then z).
    #[must_use]
    pub fn split_by_chunk(&self) -> Vec<Self> {
        let mut out = Vec::new();
        let (cx0, cz0) = self.min.chunk();
        let (cx1, cz1) = self.max.chunk();
        for cz in cz0..=cz1 {
            for cx in cx0..=cx1 {
                let x0 = self.min.x.max(cx * 16);
                let x1 = self.max.x.min(cx * 16 + 15);
                let z0 = self.min.z.max(cz * 16);
                let z1 = self.max.z.min(cz * 16 + 15);
                out.push(Self {
                    min: Pos::new(x0, self.min.y, z0),
                    max: Pos::new(x1, self.max.y, z1),
                });
            }
        }
        out
    }

    /// Every chunk column this box touches.
    #[must_use]
    pub fn chunks(&self) -> Vec<(i32, i32)> {
        let (cx0, cz0) = self.min.chunk();
        let (cx1, cz1) = self.max.chunk();
        let mut out = Vec::new();
        for cz in cz0..=cz1 {
            for cx in cx0..=cx1 {
                out.push((cx, cz));
            }
        }
        out
    }
}

/// A filter inside the boxes of an edit: brushes paint spheres and cylinders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    All,
    /// Blocks whose centre is within `radius` of `center`'s centre.
    Sphere {
        center: Pos,
        radius: f64,
    },
    /// A vertical cylinder from `center.y` up, `height` blocks tall.
    Cylinder {
        center: Pos,
        radius: f64,
        height: i32,
    },
}

impl Shape {
    #[must_use]
    pub fn contains(&self, p: Pos) -> bool {
        match *self {
            Self::All => true,
            Self::Sphere { center, radius } => {
                let d = |a: i32, b: i32| f64::from(a) - f64::from(b);
                let (dx, dy, dz) = (d(p.x, center.x), d(p.y, center.y), d(p.z, center.z));
                dx * dx + dy * dy + dz * dz <= (radius + 0.5) * (radius + 0.5)
            }
            Self::Cylinder {
                center,
                radius,
                height,
            } => {
                let dx = f64::from(p.x) - f64::from(center.x);
                let dz = f64::from(p.z) - f64::from(center.z);
                p.y >= center.y
                    && p.y < center.y.saturating_add(height)
                    && dx * dx + dz * dz <= (radius + 0.5) * (radius + 0.5)
            }
        }
    }

    /// The box around the shape (for `All`, none).
    #[must_use]
    pub fn bounds(&self) -> Option<Cuboid> {
        match *self {
            Self::All => None,
            Self::Sphere { center, radius } => {
                let r = radius.ceil() as i32;
                Some(Cuboid::from_corners(
                    center.offset(-r, -r, -r),
                    center.offset(r, r, r),
                ))
            }
            Self::Cylinder {
                center,
                radius,
                height,
            } => {
                let r = radius.ceil() as i32;
                Some(Cuboid::from_corners(
                    center.offset(-r, 0, -r),
                    center.offset(r, height.max(1) - 1, r),
                ))
            }
        }
    }
}

/// Walks the boxes of an edit, one position at a time, y then z then x inside each box.
#[derive(Debug, Clone)]
pub struct Cursor {
    boxes: Vec<Cuboid>,
    index: usize,
    next: Option<Pos>,
}

impl Cursor {
    /// Boxes are cut per chunk first, so a cursor never leaves a chunk inside one box.
    #[must_use]
    pub fn new(parts: &[Cuboid]) -> Self {
        let boxes: Vec<Cuboid> = parts.iter().flat_map(Cuboid::split_by_chunk).collect();
        let next = boxes.first().map(|b| b.min);
        Self {
            boxes,
            index: 0,
            next,
        }
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.boxes.iter().map(Cuboid::volume).sum()
    }

    /// The chunk of the next position, if any is left.
    #[must_use]
    pub fn chunk(&self) -> Option<(i32, i32)> {
        self.next.map(Pos::chunk)
    }

    /// Chunks of the next `ahead` boxes from here (for loading ahead).
    #[must_use]
    pub fn chunks_ahead(&self, ahead: usize) -> Vec<(i32, i32)> {
        let mut out: Vec<(i32, i32)> = Vec::new();
        for b in self.boxes.iter().skip(self.index) {
            let c = b.min.chunk();
            if !out.contains(&c) {
                out.push(c);
                if out.len() >= ahead {
                    break;
                }
            }
        }
        out
    }

    #[must_use]
    pub fn is_done(&self) -> bool {
        self.next.is_none()
    }
}

impl Iterator for Cursor {
    type Item = Pos;

    fn next(&mut self) -> Option<Pos> {
        let current = self.next?;
        let b = self.boxes[self.index];
        let mut p = current;
        p.x += 1;
        if p.x > b.max.x {
            p.x = b.min.x;
            p.z += 1;
            if p.z > b.max.z {
                p.z = b.min.z;
                p.y += 1;
            }
        }
        if p.y > b.max.y {
            self.index += 1;
            self.next = self.boxes.get(self.index).map(|n| n.min);
        } else {
            self.next = Some(p);
        }
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn all(parts: &[Cuboid]) -> Vec<Pos> {
        Cursor::new(parts).collect()
    }

    #[test]
    fn corners_in_any_order_give_the_same_box() {
        let a = Cuboid::from_corners(Pos::new(5, 70, -3), Pos::new(-2, 64, 10));
        assert_eq!(a.min, Pos::new(-2, 64, -3));
        assert_eq!(a.max, Pos::new(5, 70, 10));
        assert_eq!(a.size(), (8, 7, 14));
        assert_eq!(a.volume(), 8 * 7 * 14);
    }

    #[test]
    fn the_cursor_visits_every_block_once_chunk_by_chunk() {
        let b = Cuboid::from_corners(Pos::new(-20, 0, 7), Pos::new(17, 2, 40));
        let visited = all(&[b]);
        assert_eq!(visited.len() as u64, b.volume());
        let unique: HashSet<_> = visited.iter().copied().collect();
        assert_eq!(unique.len(), visited.len());
        assert!(visited.iter().all(|p| b.contains(*p)));
        // Chunks come in one run each.
        let mut seen = Vec::new();
        for p in &visited {
            if seen.last() != Some(&p.chunk()) {
                assert!(
                    !seen.contains(&p.chunk()),
                    "chunk {:?} visited twice",
                    p.chunk()
                );
                seen.push(p.chunk());
            }
        }
        assert_eq!(Cursor::new(&[b]).total(), b.volume());
    }

    #[test]
    fn walls_and_outline_are_the_faces_without_overlap() {
        for (a, b) in [
            (Pos::new(0, 0, 0), Pos::new(4, 3, 5)),
            (Pos::new(0, 0, 0), Pos::new(0, 3, 5)),
            (Pos::new(0, 0, 0), Pos::new(1, 0, 1)),
            (Pos::new(0, 0, 0), Pos::new(2, 2, 0)),
            (Pos::new(3, 3, 3), Pos::new(3, 3, 3)),
        ] {
            let c = Cuboid::from_corners(a, b);
            let walls = all(&c.walls());
            let expect_walls: HashSet<Pos> = all(&[c])
                .into_iter()
                .filter(|p| p.x == c.min.x || p.x == c.max.x || p.z == c.min.z || p.z == c.max.z)
                .collect();
            assert_eq!(walls.len(), expect_walls.len(), "walls of {c:?}");
            assert_eq!(walls.into_iter().collect::<HashSet<_>>(), expect_walls);

            let outline = all(&c.outline());
            let expect_outline: HashSet<Pos> = all(&[c])
                .into_iter()
                .filter(|p| {
                    p.x == c.min.x
                        || p.x == c.max.x
                        || p.z == c.min.z
                        || p.z == c.max.z
                        || p.y == c.min.y
                        || p.y == c.max.y
                })
                .collect();
            assert_eq!(outline.len(), expect_outline.len(), "outline of {c:?}");
            assert_eq!(outline.into_iter().collect::<HashSet<_>>(), expect_outline);
        }
    }

    #[test]
    fn shapes_hold_what_they_should() {
        let c = Pos::new(0, 64, 0);
        let sphere = Shape::Sphere {
            center: c,
            radius: 2.0,
        };
        assert!(sphere.contains(c));
        assert!(sphere.contains(Pos::new(2, 64, 0)));
        assert!(!sphere.contains(Pos::new(2, 66, 2)));
        let bounds = sphere.bounds().unwrap();
        let count = all(&[bounds])
            .into_iter()
            .filter(|p| sphere.contains(*p))
            .count();
        // dx²+dy²+dz² <= 6.25: 1 + 6 + 12 + 8 + 6 + 24 + 24 points for sums 0 to 6.
        assert_eq!(count, 81, "a radius-2 ball");
        let cyl = Shape::Cylinder {
            center: c,
            radius: 1.0,
            height: 3,
        };
        assert!(cyl.contains(Pos::new(1, 66, 1)));
        assert!(!cyl.contains(Pos::new(0, 67, 0)));
        assert!(!cyl.contains(Pos::new(0, 63, 0)));
        let cb = cyl.bounds().unwrap();
        assert_eq!(
            all(&[cb]).into_iter().filter(|p| cyl.contains(*p)).count(),
            9 * 3
        );
    }

    #[test]
    fn clamping_and_chunks() {
        let b = Cuboid::from_corners(Pos::new(0, -100, 0), Pos::new(31, 400, 15));
        let c = b.clamp_y(-64, 319).unwrap();
        assert_eq!((c.min.y, c.max.y), (-64, 319));
        assert!(b.clamp_y(500, 600).is_none());
        assert_eq!(b.chunks(), vec![(0, 0), (1, 0)]);
        let cursor = Cursor::new(&[b]);
        assert_eq!(cursor.chunks_ahead(5), vec![(0, 0), (1, 0)]);
    }
}
