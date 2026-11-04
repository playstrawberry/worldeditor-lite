//! Rotations around the vertical axis and flips, for positions and for block states.
//!
//! A transform is a 3×3 matrix of 0 and ±1. `//rotate 90` turns the clipboard 90° clockwise
//! seen from above (north becomes east); `//flip east` mirrors east and west. Block states
//! follow: `facing`, `axis`, `rotation` (signs, banners, heads), the side properties of fences,
//! walls, panes and wire, stair shapes, door hinges, chest halves, rail shapes, `orientation`,
//! and on an up-down flip `half`, slab `type`, `face`, bell `attachment`, `hanging` and
//! `vertical_direction`.

use crate::geom::Pos;
use crate::pattern::Props;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transform {
    /// Row-major: `out[i] = sum m[i][j] * in[j]`, axes x, y, z.
    pub m: [[i32; 3]; 3],
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The horizontal and vertical directions, with their vectors.
const DIRECTIONS: [(&str, [i32; 3]); 6] = [
    ("north", [0, 0, -1]),
    ("east", [1, 0, 0]),
    ("south", [0, 0, 1]),
    ("west", [-1, 0, 0]),
    ("up", [0, 1, 0]),
    ("down", [0, -1, 0]),
];

impl Transform {
    pub const IDENTITY: Self = Self {
        m: [[1, 0, 0], [0, 1, 0], [0, 0, 1]],
    };

    /// `quarters` quarter turns clockwise seen from above.
    #[must_use]
    pub fn rotate_y(quarters: i32) -> Self {
        // One clockwise quarter: (x, z) -> (-z, x).
        let step = Self {
            m: [[0, 0, -1], [0, 1, 0], [1, 0, 0]],
        };
        let mut out = Self::IDENTITY;
        for _ in 0..quarters.rem_euclid(4) {
            out = step.then_after(out);
        }
        out
    }

    /// Mirror along one axis: 0 = x (east-west), 1 = y (up-down), 2 = z (north-south).
    #[must_use]
    pub fn flip(axis: usize) -> Self {
        let mut out = Self::IDENTITY;
        out.m[axis.min(2)][axis.min(2)] = -1;
        out
    }

    /// `self` applied after `first`.
    #[must_use]
    pub fn then_after(self, first: Self) -> Self {
        let mut m = [[0; 3]; 3];
        for (i, row) in m.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = (0..3).map(|k| self.m[i][k] * first.m[k][j]).sum();
            }
        }
        Self { m }
    }

    /// The inverse (the matrix is orthogonal: its transpose).
    #[must_use]
    pub fn inverse(self) -> Self {
        let mut m = [[0; 3]; 3];
        for (i, row) in m.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = self.m[j][i];
            }
        }
        Self { m }
    }

    #[must_use]
    pub fn apply_vec(&self, v: [i32; 3]) -> [i32; 3] {
        let mut out = [0; 3];
        for (i, o) in out.iter_mut().enumerate() {
            *o = (0..3).map(|k| self.m[i][k] * v[k]).sum();
        }
        out
    }

    #[must_use]
    pub fn apply(&self, p: Pos) -> Pos {
        let [x, y, z] = self.apply_vec([p.x, p.y, p.z]);
        Pos::new(x, y, z)
    }

    #[must_use]
    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    /// Whether east and west (or north and south) trade places: left and right swap.
    fn mirrors_horizontally(&self) -> bool {
        self.m[0][0] * self.m[2][2] - self.m[0][2] * self.m[2][0] < 0
    }

    fn flips_vertically(&self) -> bool {
        self.m[1][1] < 0
    }

    /// The name of the direction `name` turns into.
    fn direction(&self, name: &str) -> Option<&'static str> {
        let v = DIRECTIONS.iter().find(|(n, _)| *n == name)?.1;
        let w = self.apply_vec(v);
        DIRECTIONS.iter().find(|(_, d)| *d == w).map(|(n, _)| *n)
    }

    /// The new value of a `rotation` property (0 to 15, 0 = south, clockwise seen from above).
    fn rotation(&self, r: i32) -> i32 {
        let index = |name: &str| {
            ["south", "west", "north", "east"]
                .iter()
                .position(|n| *n == name)
                .map_or(0, |i| i as i32 * 4)
        };
        let south = self.direction("south").map_or(0, index);
        if self.mirrors_horizontally() {
            (south - r).rem_euclid(16)
        } else {
            (south + r).rem_euclid(16)
        }
    }

    /// Transforms block state properties. Unknown properties stay as they are.
    #[must_use]
    pub fn props(&self, block: &str, props: &Props) -> Props {
        if self.is_identity() {
            return props.clone();
        }
        let mirror = self.mirrors_horizontally();
        let vflip = self.flips_vertically();
        let is_rail = block.ends_with("rail");
        let swap = |v: &str, a: &str, b: &str| -> String {
            if v == a {
                b.to_owned()
            } else if v == b {
                a.to_owned()
            } else {
                v.to_owned()
            }
        };
        let side_keys = props.iter().all(|(k, _)| {
            DIRECTIONS.iter().any(|(n, _)| n == k) || k == "waterlogged" || k == "power"
        });
        let mut out = Vec::with_capacity(props.len());
        for (k, v) in props {
            let key = k.as_str();
            let value: String = match key {
                "facing" => self.direction(v).unwrap_or(v).to_owned(),
                "axis" => {
                    let unit = match v.as_str() {
                        "x" => [1, 0, 0],
                        "y" => [0, 1, 0],
                        _ => [0, 0, 1],
                    };
                    let w = self.apply_vec(unit);
                    if w[0] != 0 {
                        "x"
                    } else if w[1] != 0 {
                        "y"
                    } else {
                        "z"
                    }
                    .to_owned()
                }
                "rotation" => v
                    .parse::<i32>()
                    .map_or_else(|_| v.clone(), |r| self.rotation(r).to_string()),
                "shape" if is_rail => rail_shape(self, v),
                "shape" if mirror => {
                    let s = swap(v, "inner_left", "inner_right");
                    if s == *v {
                        swap(v, "outer_left", "outer_right")
                    } else {
                        s
                    }
                }
                "hinge" if mirror => swap(v, "left", "right"),
                "type" if mirror && (v == "left" || v == "right") => swap(v, "left", "right"),
                "type" if vflip => swap(v, "top", "bottom"),
                "half" if vflip => swap(v, "top", "bottom"),
                "face" if vflip => swap(v, "floor", "ceiling"),
                "attachment" if vflip => swap(v, "floor", "ceiling"),
                "hanging" if vflip => swap(v, "true", "false"),
                "vertical_direction" if vflip => swap(v, "up", "down"),
                "orientation" => v
                    .split_once('_')
                    .and_then(|(a, b)| {
                        Some(format!("{}_{}", self.direction(a)?, self.direction(b)?))
                    })
                    .unwrap_or_else(|| v.clone()),
                _ => v.clone(),
            };
            let new_key = if side_keys {
                self.direction(key).unwrap_or(key).to_owned()
            } else {
                k.clone()
            };
            out.push((new_key, value));
        }
        out
    }
}

/// The state `state` becomes under `t`, through the registry; the same state when the
/// transformed properties name no state. `cache` keeps the answers of one edit.
pub fn transform_state(
    t: &Transform,
    blocks: &dyn crate::pattern::Blocks,
    state: u16,
    cache: &mut std::collections::HashMap<u16, u16>,
) -> u16 {
    if t.is_identity() {
        return state;
    }
    *cache.entry(state).or_insert_with(|| {
        blocks
            .info(state)
            .filter(|(_, props)| !props.is_empty())
            .and_then(|(name, props)| blocks.resolve(&name, &t.props(&name, &props)))
            .unwrap_or(state)
    })
}

/// Rail shapes: `north_south`, `east_west`, `ascending_<dir>` and the four curves.
fn rail_shape(t: &Transform, v: &str) -> String {
    if let Some(dir) = v.strip_prefix("ascending_") {
        return t
            .direction(dir)
            .map_or_else(|| v.to_owned(), |d| format!("ascending_{d}"));
    }
    let Some((a, b)) = v.split_once('_') else {
        return v.to_owned();
    };
    let (Some(a), Some(b)) = (t.direction(a), t.direction(b)) else {
        return v.to_owned();
    };
    let ns = |d: &str| d == "north" || d == "south";
    if ns(a) == ns(b) {
        // A straight rail.
        return if ns(a) {
            "north_south".to_owned()
        } else {
            "east_west".to_owned()
        };
    }
    // A curve: the north or south part first.
    if ns(a) {
        format!("{a}_{b}")
    } else {
        format!("{b}_{a}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pairs: &[(&str, &str)]) -> Props {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn get<'a>(props: &'a Props, key: &str) -> &'a str {
        props.iter().find(|(k, _)| k == key).map_or("", |(_, v)| v)
    }

    #[test]
    fn positions_turn_clockwise_and_mirror() {
        let r = Transform::rotate_y(1);
        assert_eq!(
            r.apply(Pos::new(0, 5, -1)),
            Pos::new(1, 5, 0),
            "north becomes east"
        );
        assert_eq!(Transform::rotate_y(4), Transform::IDENTITY);
        assert_eq!(Transform::rotate_y(-1), Transform::rotate_y(3));
        let two = Transform::rotate_y(1).then_after(Transform::rotate_y(1));
        assert_eq!(two, Transform::rotate_y(2));
        let f = Transform::flip(0);
        assert_eq!(f.apply(Pos::new(3, 1, 2)), Pos::new(-3, 1, 2));
        for t in [
            Transform::rotate_y(1),
            Transform::flip(2).then_after(Transform::rotate_y(3)),
        ] {
            assert_eq!(t.inverse().then_after(t), Transform::IDENTITY);
        }
    }

    #[test]
    fn facing_axis_and_rotation_follow_a_quarter_turn() {
        let r = Transform::rotate_y(1);
        assert_eq!(
            get(
                &r.props("minecraft:oak_stairs", &p(&[("facing", "north")])),
                "facing"
            ),
            "east"
        );
        assert_eq!(
            get(
                &r.props("minecraft:piston", &p(&[("facing", "up")])),
                "facing"
            ),
            "up"
        );
        assert_eq!(
            get(&r.props("minecraft:oak_log", &p(&[("axis", "x")])), "axis"),
            "z"
        );
        assert_eq!(
            get(&r.props("minecraft:oak_log", &p(&[("axis", "y")])), "axis"),
            "y"
        );
        // rotation 0 = south; a clockwise quarter makes it west (4).
        assert_eq!(
            get(
                &r.props("minecraft:oak_sign", &p(&[("rotation", "0")])),
                "rotation"
            ),
            "4"
        );
        assert_eq!(
            get(
                &r.props("minecraft:oak_sign", &p(&[("rotation", "14")])),
                "rotation"
            ),
            "2"
        );
    }

    #[test]
    fn mirrors_swap_left_and_right() {
        let f = Transform::flip(0);
        let s = f.props(
            "minecraft:oak_stairs",
            &p(&[
                ("facing", "east"),
                ("half", "bottom"),
                ("shape", "inner_left"),
            ]),
        );
        assert_eq!(get(&s, "facing"), "west");
        assert_eq!(get(&s, "shape"), "inner_right");
        assert_eq!(get(&s, "half"), "bottom");
        assert_eq!(
            get(
                &f.props("minecraft:oak_door", &p(&[("hinge", "left")])),
                "hinge"
            ),
            "right"
        );
        assert_eq!(
            get(&f.props("minecraft:chest", &p(&[("type", "left")])), "type"),
            "right"
        );
        // rotation 4 = west; mirrored east-west it is east (12).
        assert_eq!(
            get(
                &f.props("minecraft:oak_sign", &p(&[("rotation", "4")])),
                "rotation"
            ),
            "12"
        );
        assert_eq!(
            get(
                &Transform::flip(2).props("minecraft:oak_sign", &p(&[("rotation", "0")])),
                "rotation"
            ),
            "8"
        );
        // Fence sides trade places.
        let fence = f.props(
            "minecraft:oak_fence",
            &p(&[
                ("east", "true"),
                ("north", "false"),
                ("south", "false"),
                ("west", "false"),
            ]),
        );
        assert_eq!(get(&fence, "west"), "true");
        assert_eq!(get(&fence, "east"), "false");
    }

    #[test]
    fn an_up_down_flip_turns_halves_and_faces() {
        let f = Transform::flip(1);
        assert_eq!(
            get(
                &f.props(
                    "minecraft:oak_stairs",
                    &p(&[("half", "bottom"), ("shape", "inner_left")])
                ),
                "half"
            ),
            "top"
        );
        assert_eq!(
            get(
                &f.props("minecraft:oak_stairs", &p(&[("shape", "inner_left")])),
                "shape"
            ),
            "inner_left"
        );
        assert_eq!(
            get(
                &f.props("minecraft:oak_slab", &p(&[("type", "bottom")])),
                "type"
            ),
            "top"
        );
        assert_eq!(
            get(
                &f.props("minecraft:oak_slab", &p(&[("type", "double")])),
                "type"
            ),
            "double"
        );
        assert_eq!(
            get(
                &f.props("minecraft:oak_door", &p(&[("half", "upper")])),
                "half"
            ),
            "upper"
        );
        assert_eq!(
            get(
                &f.props("minecraft:lever", &p(&[("face", "floor")])),
                "face"
            ),
            "ceiling"
        );
        assert_eq!(
            get(
                &f.props("minecraft:piston", &p(&[("facing", "up")])),
                "facing"
            ),
            "down"
        );
    }

    #[test]
    fn states_transform_through_the_registry() {
        use crate::pattern::Blocks;
        let blocks = crate::pattern::fake::FakeBlocks::default();
        let north = blocks
            .resolve("minecraft:oak_stairs", &p(&[("facing", "north")]))
            .unwrap();
        let east = blocks
            .resolve("minecraft:oak_stairs", &p(&[("facing", "east")]))
            .unwrap();
        let mut cache = std::collections::HashMap::new();
        let r = Transform::rotate_y(1);
        assert_eq!(transform_state(&r, &blocks, north, &mut cache), east);
        assert_eq!(
            transform_state(&r, &blocks, 1, &mut cache),
            1,
            "stone has no properties"
        );
        assert_eq!(
            transform_state(&Transform::IDENTITY, &blocks, north, &mut cache),
            north
        );
    }

    #[test]
    fn rails_and_orientation() {
        let r = Transform::rotate_y(1);
        let shape = |t: &Transform, v: &str| {
            get(&t.props("minecraft:rail", &p(&[("shape", v)])), "shape").to_owned()
        };
        assert_eq!(shape(&r, "north_south"), "east_west");
        assert_eq!(shape(&r, "ascending_north"), "ascending_east");
        assert_eq!(shape(&r, "north_east"), "south_east");
        assert_eq!(shape(&r, "south_west"), "north_west");
        assert_eq!(shape(&Transform::flip(0), "north_east"), "north_west");
        assert_eq!(
            get(
                &r.props("minecraft:crafter", &p(&[("orientation", "north_up")])),
                "orientation"
            ),
            "east_up"
        );
        assert_eq!(
            get(
                &r.props("minecraft:jigsaw", &p(&[("orientation", "up_north")])),
                "orientation"
            ),
            "up_east"
        );
    }
}
