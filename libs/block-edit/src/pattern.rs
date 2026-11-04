//! Block patterns (what to place) and masks (what to touch), parsed from command text.
//!
//! - A pattern is one block or a weighted list: `stone`, `oak_stairs[facing=east]`,
//!   `70%stone,30%cobblestone`, `stone,dirt` (equal weights). A weight without `%` is not
//!   accepted; a block without one weighs 1.
//! - A mask is a list of blocks: a block without properties matches every state of that block;
//!   with properties, the states that have those values. `!` in front inverts the whole mask.
//!
//! The server's block registry is reached through [`Blocks`], so all of this is tested without a
//! server.

use std::collections::HashSet;

/// Block properties as the server names them.
pub type Props = Vec<(String, String)>;

/// What the plugin needs from the server's block registry.
pub trait Blocks {
    /// The state of `name` with `props` (the other properties at their defaults).
    fn resolve(&self, name: &str, props: &[(String, String)]) -> Option<u16>;
    /// Every state of `name` with its properties.
    fn states(&self, name: &str) -> Option<Vec<(u16, Props)>>;
    /// The block name and properties of a state.
    fn info(&self, state: u16) -> Option<(String, Props)>;
}

/// One block written as text: a name and the properties given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSpec {
    pub name: String,
    pub props: Props,
}

/// Why a text is not a pattern or a mask. The text is the part that is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    Empty,
    BadSyntax(String),
    UnknownBlock(String),
    BadProperty(String),
    TooMany,
}

/// The longest pattern or mask text taken.
pub const MAX_TEXT: usize = 512;
/// The most entries in one pattern or mask.
pub const MAX_ENTRIES: usize = 32;

fn valid_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | ':' | '.' | '-' | '/')
}

/// Splits on commas that are not inside `[...]`.
fn split_top(text: &str) -> Result<Vec<&str>, PatternError> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        if !(0..=1).contains(&depth) {
            return Err(PatternError::BadSyntax(text.to_owned()));
        }
    }
    if depth != 0 {
        return Err(PatternError::BadSyntax(text.to_owned()));
    }
    out.push(&text[start..]);
    Ok(out)
}

/// Parses `name` or `name[key=value,...]`.
pub fn parse_spec(text: &str) -> Result<BlockSpec, PatternError> {
    let text = text.trim();
    let (name, props) = match text.find('[') {
        Some(open) => {
            let close = text.rfind(']').filter(|&c| c == text.len() - 1 && c > open);
            let Some(close) = close else {
                return Err(PatternError::BadSyntax(text.to_owned()));
            };
            (&text[..open], Some(&text[open + 1..close]))
        }
        None => (text, None),
    };
    let name = name.to_ascii_lowercase();
    if name.is_empty() || !name.chars().all(valid_name_char) || name.matches(':').count() > 1 {
        return Err(PatternError::BadSyntax(text.to_owned()));
    }
    let name = if name.contains(':') {
        name
    } else {
        format!("minecraft:{name}")
    };
    let mut out = Vec::new();
    if let Some(props) = props.filter(|p| !p.trim().is_empty()) {
        for pair in props.split(',') {
            let Some((k, v)) = pair.split_once('=') else {
                return Err(PatternError::BadProperty(pair.trim().to_owned()));
            };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
            let ok =
                |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok(&k) || !ok(&v) {
                return Err(PatternError::BadProperty(pair.trim().to_owned()));
            }
            out.push((k, v));
        }
    }
    Ok(BlockSpec { name, props: out })
}

fn check_size(text: &str) -> Result<&str, PatternError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(PatternError::Empty);
    }
    if text.len() > MAX_TEXT {
        return Err(PatternError::TooMany);
    }
    Ok(text)
}

/// The state of a block with `wanted` properties set and the others at their defaults, picked
/// from the block's own state list: a key or value the block does not have gives none. This
/// never asks the server to build a state from unchecked text.
#[must_use]
pub fn match_state(
    default: &[(String, String)],
    states: &[(u16, Props)],
    wanted: &[(String, String)],
) -> Option<u16> {
    if wanted
        .iter()
        .any(|(k, _)| !default.iter().any(|(dk, _)| dk == k))
    {
        return None;
    }
    let target: Vec<(&str, &str)> = default
        .iter()
        .map(|(k, v)| {
            let v = wanted
                .iter()
                .rev()
                .find(|(wk, _)| wk == k)
                .map_or(v.as_str(), |(_, wv)| wv.as_str());
            (k.as_str(), v)
        })
        .collect();
    states
        .iter()
        .find(|(_, props)| {
            props.len() == target.len()
                && target
                    .iter()
                    .all(|(k, v)| props.iter().any(|(pk, pv)| pk == k && pv == v))
        })
        .map(|(id, _)| *id)
}

/// A pattern resolved to block states with cumulative weights.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    /// (cumulative weight, state), ascending.
    entries: Vec<(f64, u16)>,
}

impl Pattern {
    #[must_use]
    pub fn single(state: u16) -> Self {
        Self {
            entries: vec![(1.0, state)],
        }
    }

    pub fn parse(text: &str, blocks: &dyn Blocks) -> Result<Self, PatternError> {
        let text = check_size(text)?;
        let parts = split_top(text)?;
        if parts.len() > MAX_ENTRIES {
            return Err(PatternError::TooMany);
        }
        let mut entries = Vec::new();
        let mut total = 0.0;
        for part in parts {
            let part = part.trim();
            let (weight, block) = match part.split_once('%') {
                Some((w, rest)) if !w.contains('[') => {
                    let w: f64 = w
                        .trim()
                        .parse()
                        .map_err(|_| PatternError::BadSyntax(part.to_owned()))?;
                    if !w.is_finite() || w <= 0.0 || w > 1_000_000.0 {
                        return Err(PatternError::BadSyntax(part.to_owned()));
                    }
                    (w, rest)
                }
                _ => (1.0, part),
            };
            let spec = parse_spec(block)?;
            let state = blocks
                .resolve(&spec.name, &spec.props)
                .ok_or_else(|| unknown(&spec, blocks))?;
            total += weight;
            entries.push((total, state));
        }
        Ok(Self { entries })
    }

    /// The state for `p`, the same every time for the same position and seed.
    #[must_use]
    pub fn pick(&self, x: i32, y: i32, z: i32, seed: u64) -> u16 {
        if self.entries.len() == 1 {
            return self.entries[0].1;
        }
        let total = self.entries.last().map_or(1.0, |e| e.0);
        let r = (mix(x, y, z, seed) >> 11) as f64 / (1u64 << 53) as f64 * total;
        self.entries
            .iter()
            .find(|(w, _)| r < *w)
            .or(self.entries.last())
            .map_or(0, |e| e.1)
    }

    /// The states this pattern can place.
    pub fn states(&self) -> impl Iterator<Item = u16> + '_ {
        self.entries.iter().map(|e| e.1)
    }
}

/// Unknown block, or a known block with a property it does not have.
fn unknown(spec: &BlockSpec, blocks: &dyn Blocks) -> PatternError {
    if spec.props.is_empty() || blocks.states(&spec.name).is_none() {
        PatternError::UnknownBlock(spec.name.clone())
    } else {
        let props: Vec<String> = spec.props.iter().map(|(k, v)| format!("{k}={v}")).collect();
        PatternError::BadProperty(props.join(","))
    }
}

/// A hash of a position (SplitMix64 over the packed coordinates).
#[must_use]
pub fn mix(x: i32, y: i32, z: i32, seed: u64) -> u64 {
    let mut v = seed
        ^ (u64::from(x as u32)).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (u64::from(y as u32)).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (u64::from(z as u32)).wrapping_mul(0x1656_67B1_9E37_79F9);
    v = (v ^ (v >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    v = (v ^ (v >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    v ^ (v >> 31)
}

/// The set of states an edit may change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    states: HashSet<u16>,
    inverted: bool,
}

impl Mask {
    /// Every block except air (the default `from` of `//replace`).
    #[must_use]
    pub fn existing(air: &[u16]) -> Self {
        Self {
            states: air.iter().copied().collect(),
            inverted: true,
        }
    }

    pub fn parse(text: &str, blocks: &dyn Blocks) -> Result<Self, PatternError> {
        let text = check_size(text)?;
        let (inverted, text) = match text.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, text),
        };
        let parts = split_top(text)?;
        if parts.len() > MAX_ENTRIES {
            return Err(PatternError::TooMany);
        }
        let mut states = HashSet::new();
        for part in parts {
            let spec = parse_spec(part)?;
            let all = blocks
                .states(&spec.name)
                .ok_or_else(|| PatternError::UnknownBlock(spec.name.clone()))?;
            let before = states.len();
            let mut matched = false;
            for (id, props) in all {
                if spec.props.iter().all(|want| props.contains(want)) {
                    states.insert(id);
                    matched = true;
                }
            }
            if !matched && states.len() == before {
                return Err(unknown(&spec, blocks));
            }
        }
        Ok(Self { states, inverted })
    }

    #[must_use]
    pub fn matches(&self, state: u16) -> bool {
        self.states.contains(&state) != self.inverted
    }
}

#[cfg(test)]
pub mod fake {
    //! A small block registry for tests: air, stone, dirt, stairs with facing/half/shape,
    //! a fence with four sides, and a sign with rotation.
    use super::{Blocks, Props};

    pub struct FakeBlocks {
        pub states: Vec<(String, Props)>,
    }

    fn product(name: &str, keys: &[(&str, &[&str])]) -> Vec<(String, Props)> {
        let mut out: Vec<Props> = vec![Vec::new()];
        for (k, values) in keys {
            let mut next = Vec::new();
            for props in &out {
                for v in *values {
                    let mut p = props.clone();
                    p.push(((*k).to_owned(), (*v).to_owned()));
                    next.push(p);
                }
            }
            out = next;
        }
        out.into_iter().map(|p| (name.to_owned(), p)).collect()
    }

    impl Default for FakeBlocks {
        fn default() -> Self {
            let dirs: &[&str] = &["north", "east", "south", "west"];
            let tf: &[&str] = &["true", "false"];
            let mut states = vec![
                ("minecraft:air".to_owned(), Vec::new()),
                ("minecraft:stone".to_owned(), Vec::new()),
                ("minecraft:dirt".to_owned(), Vec::new()),
            ];
            states.extend(product(
                "minecraft:oak_stairs",
                &[
                    ("facing", dirs),
                    ("half", &["top", "bottom"]),
                    (
                        "shape",
                        &[
                            "straight",
                            "inner_left",
                            "inner_right",
                            "outer_left",
                            "outer_right",
                        ],
                    ),
                ],
            ));
            states.extend(product(
                "minecraft:oak_fence",
                &[("east", tf), ("north", tf), ("south", tf), ("west", tf)],
            ));
            let rotations: Vec<String> = (0..16).map(|r| r.to_string()).collect();
            let rotation_refs: Vec<&str> = rotations.iter().map(String::as_str).collect();
            states.extend(product(
                "minecraft:oak_sign",
                &[("rotation", &rotation_refs)],
            ));
            states.extend(product("minecraft:oak_log", &[("axis", &["x", "y", "z"])]));
            states.extend(product(
                "minecraft:piston",
                &[
                    ("extended", tf),
                    ("facing", &["north", "east", "south", "west", "up", "down"]),
                ],
            ));
            states.extend(product(
                "minecraft:rail",
                &[(
                    "shape",
                    &[
                        "north_south",
                        "east_west",
                        "ascending_east",
                        "ascending_west",
                        "ascending_north",
                        "ascending_south",
                        "south_east",
                        "south_west",
                        "north_west",
                        "north_east",
                    ],
                )],
            ));
            states.extend(product(
                "minecraft:oak_slab",
                &[("type", &["top", "bottom", "double"])],
            ));
            states.extend(product(
                "minecraft:oak_door",
                &[
                    ("facing", dirs),
                    ("half", &["upper", "lower"]),
                    ("hinge", &["left", "right"]),
                ],
            ));
            Self { states }
        }
    }

    impl Blocks for FakeBlocks {
        fn resolve(&self, name: &str, props: &[(String, String)]) -> Option<u16> {
            let first = self.states.iter().position(|(n, _)| n == name)?;
            let base = self.states[first].1.clone();
            if props
                .iter()
                .any(|(k, _)| !base.iter().any(|(bk, _)| bk == k))
            {
                return None;
            }
            self.states
                .iter()
                .enumerate()
                .position(|(_, (n, p))| {
                    n == name
                        && p.iter().all(|(k, v)| {
                            props.iter().find(|(pk, _)| pk == k).map_or_else(
                                || base.iter().any(|(bk, bv)| bk == k && bv == v),
                                |(_, pv)| pv == v,
                            )
                        })
                })
                .map(|i| i as u16)
        }

        fn states(&self, name: &str) -> Option<Vec<(u16, Props)>> {
            let out: Vec<(u16, Props)> = self
                .states
                .iter()
                .enumerate()
                .filter(|(_, (n, _))| n == name)
                .map(|(i, (_, p))| (i as u16, p.clone()))
                .collect();
            (!out.is_empty()).then_some(out)
        }

        fn info(&self, state: u16) -> Option<(String, Props)> {
            self.states.get(usize::from(state)).cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeBlocks;
    use super::*;

    #[test]
    fn states_are_matched_from_the_block_list_only() {
        let blocks = FakeBlocks::default();
        let states = blocks.states("minecraft:oak_stairs").unwrap();
        let default = states[0].1.clone();
        let p = |pairs: &[(&str, &str)]| -> Props {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let east = match_state(&default, &states, &p(&[("facing", "east")])).unwrap();
        let (_, props) = blocks.info(east).unwrap();
        assert!(props.contains(&("facing".into(), "east".into())));
        assert!(
            props.contains(&("half".into(), "top".into())),
            "the other properties keep their defaults"
        );
        assert_eq!(
            match_state(&default, &states, &p(&[("facing", "up")])),
            None,
            "a value the block lacks"
        );
        assert_eq!(
            match_state(&default, &states, &p(&[("color", "red")])),
            None,
            "a key the block lacks"
        );
        assert_eq!(match_state(&default, &states, &[]), Some(states[0].0));
    }

    #[test]
    fn specs_parse_names_and_properties() {
        assert_eq!(parse_spec("stone").unwrap().name, "minecraft:stone");
        assert_eq!(
            parse_spec("Minecraft:Stone").unwrap().name,
            "minecraft:stone"
        );
        let s = parse_spec("oak_stairs[facing=east, half=top]").unwrap();
        assert_eq!(
            s.props,
            vec![
                ("facing".into(), "east".into()),
                ("half".into(), "top".into())
            ]
        );
        assert!(matches!(
            parse_spec("stone["),
            Err(PatternError::BadSyntax(_))
        ));
        assert!(matches!(
            parse_spec("st one"),
            Err(PatternError::BadSyntax(_))
        ));
        assert!(matches!(
            parse_spec("a:b:c"),
            Err(PatternError::BadSyntax(_))
        ));
        assert!(matches!(
            parse_spec("stone[facing]"),
            Err(PatternError::BadProperty(_))
        ));
        assert!(matches!(
            parse_spec("stone[x=$]"),
            Err(PatternError::BadProperty(_))
        ));
    }

    #[test]
    fn patterns_resolve_and_pick_by_weight() {
        let blocks = FakeBlocks::default();
        let p = Pattern::parse("stone", &blocks).unwrap();
        assert_eq!(p.pick(1, 2, 3, 9), 1);
        let p = Pattern::parse("oak_stairs[facing=east,half=top]", &blocks).unwrap();
        let (_, props) = blocks.info(p.pick(0, 0, 0, 0)).unwrap();
        assert!(props.contains(&("facing".into(), "east".into())));
        assert!(props.contains(&("shape".into(), "straight".into())));

        let p = Pattern::parse("75%stone,25%dirt", &blocks).unwrap();
        let mut stone = 0;
        for i in 0..4000 {
            let s = p.pick(i, 64, -i, 42);
            assert!(s == 1 || s == 2);
            stone += i32::from(s == 1);
            assert_eq!(s, p.pick(i, 64, -i, 42), "same position, same block");
        }
        assert!(
            (2800..3200).contains(&stone),
            "about 75 % stone, got {stone}"
        );
        assert_eq!(
            Pattern::parse("stone,dirt", &blocks)
                .unwrap()
                .states()
                .count(),
            2
        );

        assert_eq!(
            Pattern::parse("nope", &blocks),
            Err(PatternError::UnknownBlock("minecraft:nope".into()))
        );
        assert_eq!(
            Pattern::parse("stone[facing=up]", &blocks),
            Err(PatternError::BadProperty("facing=up".into()))
        );
        assert_eq!(Pattern::parse("  ", &blocks), Err(PatternError::Empty));
        assert!(matches!(
            Pattern::parse("0%stone", &blocks),
            Err(PatternError::BadSyntax(_))
        ));
        assert!(matches!(
            Pattern::parse("NaN%stone", &blocks),
            Err(PatternError::BadSyntax(_))
        ));
        let long = vec!["stone"; 40].join(",");
        assert_eq!(Pattern::parse(&long, &blocks), Err(PatternError::TooMany));
    }

    #[test]
    fn masks_match_blocks_and_states() {
        let blocks = FakeBlocks::default();
        let m = Mask::parse("oak_stairs", &blocks).unwrap();
        let stairs = blocks.states("minecraft:oak_stairs").unwrap();
        assert!(stairs.iter().all(|(id, _)| m.matches(*id)));
        assert!(!m.matches(1));
        let m = Mask::parse("oak_stairs[half=top],stone", &blocks).unwrap();
        assert!(m.matches(1));
        let top = stairs.iter().filter(|(id, _)| m.matches(*id)).count();
        assert_eq!(top, stairs.len() / 2);
        let m = Mask::parse("!stone", &blocks).unwrap();
        assert!(!m.matches(1) && m.matches(2) && m.matches(0));
        let m = Mask::existing(&[0]);
        assert!(!m.matches(0) && m.matches(1));
        assert!(matches!(
            Mask::parse("stone[facing=north]", &blocks),
            Err(PatternError::BadProperty(_))
        ));
        assert!(matches!(
            Mask::parse("nope", &blocks),
            Err(PatternError::UnknownBlock(_))
        ));
    }
}
