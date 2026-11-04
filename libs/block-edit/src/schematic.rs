//! Schematic files (`.schem`): versions 1, 2 and 3 are read, version 3 is written. The file is
//! gzip-compressed NBT.
//!
//! Version 3 (public specification of the format): `Schematic { Version, DataVersion, Width,
//! Height, Length, Offset, Metadata, Blocks { Palette, Data, BlockEntities } }` inside an unnamed
//! root. Block entries are indexed `x + z * Width + y * Width * Length`, varint palette indices.
//! `Offset` is where the first block goes relative to the paster. Versions 1 and 2 keep
//! `Palette`, `BlockData` and `BlockEntities` (`TileEntities` in 1) at the top.
//!
//! Every size is checked before it is used; a bad file is an error, never a panic.

use std::collections::{BTreeMap, HashMap};

use crate::clipboard::Clipboard;
use crate::geom::Pos;
use crate::gzip;
use crate::nbt::{self, Compound, Tag};
use crate::pattern::{Blocks, parse_spec};
use crate::transform::transform_state;

/// The data version this plugin writes: Minecraft 26.3.
pub const DATA_VERSION: i32 = 5023;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The most bytes the unpacked NBT may have.
    pub max_unpacked: usize,
    /// The most blocks a schematic may hold.
    pub max_volume: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchematicError {
    Gzip(gzip::GzipError),
    Nbt(nbt::NbtError),
    Missing(&'static str),
    Version(i64),
    TooBig(u64),
    BadData(&'static str),
}

impl std::fmt::Display for SchematicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gzip(e) => write!(f, "{e}"),
            Self::Nbt(e) => write!(f, "{e}"),
            Self::Missing(what) => write!(f, "the field {what} is missing"),
            Self::Version(v) => write!(f, "format version {v} is not supported"),
            Self::TooBig(v) => write!(f, "{v} blocks is over the limit"),
            Self::BadData(what) => write!(f, "the field {what} is not valid"),
        }
    }
}

/// A schematic as the file has it, before any block is looked up on a server: what a plugin
/// needs to place blocks itself (size, offset, block text per cell) without a player or a world.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    /// The format version (1 to 3).
    pub version: i64,
    /// The Minecraft data version the file was written with, when it says.
    pub data_version: Option<i64>,
    /// Width (x), height (y), length (z).
    pub size: (u32, u32, u32),
    /// Where the first block goes relative to the paster.
    pub offset: Pos,
    /// Where the box was in the world when it was saved (`Metadata.Origin`), or 0, 0, 0.
    pub origin: Pos,
    /// The block text of each palette entry (`minecraft:oak_stairs[facing=east]`), in file order.
    pub palette: Vec<String>,
    /// Palette positions per cell, index `x + z * width + y * width * length`.
    pub cells: Vec<u16>,
    /// Block entity data per cell index, unnamed NBT with its `id`, no position.
    pub block_entities: Vec<(usize, Compound)>,
}

/// Decodes the varint `Data` array into dense palette positions, without a second copy.
fn varints(
    data: &[u8],
    count: usize,
    dense: &HashMap<u32, u16>,
) -> Result<Vec<u16>, SchematicError> {
    let mut out = Vec::with_capacity(count);
    let mut i = 0;
    while out.len() < count {
        let mut value: u32 = 0;
        let mut shift = 0;
        loop {
            let b = *data.get(i).ok_or(SchematicError::BadData("Data"))?;
            i += 1;
            value |= u32::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 28 {
                return Err(SchematicError::BadData("Data"));
            }
        }
        out.push(*dense.get(&value).ok_or(SchematicError::BadData("Data"))?);
    }
    if i != data.len() {
        return Err(SchematicError::BadData("Data"));
    }
    Ok(out)
}

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn int3(tag: Option<&Tag>) -> Option<[i32; 3]> {
    match tag {
        Some(Tag::IntArray(v)) if v.len() == 3 => Some([v[0], v[1], v[2]]),
        _ => None,
    }
}

/// `minecraft:oak_stairs[facing=east,...]` to a state of this server.
fn state_of(text: &str, blocks: &dyn Blocks) -> Option<u16> {
    let spec = parse_spec(text).ok()?;
    blocks.resolve(&spec.name, &spec.props)
}

/// The text form of a state, properties sorted by name as Minecraft writes them.
fn text_of(state: u16, blocks: &dyn Blocks) -> String {
    let Some((name, mut props)) = blocks.info(state) else {
        return "minecraft:air".to_owned();
    };
    if props.is_empty() {
        return name;
    }
    props.sort();
    let inner: Vec<String> = props.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{name}[{}]", inner.join(","))
}

/// Reads a schematic file without a server: every size checked against `limits` first.
pub fn decode(file: &[u8], limits: Limits) -> Result<Decoded, SchematicError> {
    let raw = gzip::gunzip(file, limits.max_unpacked).map_err(SchematicError::Gzip)?;
    let (_, root) = nbt::read_named(&raw).map_err(SchematicError::Nbt)?;
    let s = root.compound("Schematic").unwrap_or(&root);
    let version = s.int("Version").ok_or(SchematicError::Missing("Version"))?;
    if !(1..=3).contains(&version) {
        return Err(SchematicError::Version(version));
    }
    let dim = |name: &'static str| -> Result<u32, SchematicError> {
        let v = s.int(name).ok_or(SchematicError::Missing(name))?;
        let v = (v as i16 as i32 & 0xFFFF) as u32;
        if v == 0 {
            Err(SchematicError::BadData(name))
        } else {
            Ok(v)
        }
    };
    let size = (dim("Width")?, dim("Height")?, dim("Length")?);
    let volume = u64::from(size.0) * u64::from(size.1) * u64::from(size.2);
    if volume > limits.max_volume {
        return Err(SchematicError::TooBig(volume));
    }
    let offset = int3(s.get("Offset")).unwrap_or([0, 0, 0]);
    let (container, palette_key, data_key, entities_key) = if version == 3 {
        (
            s.compound("Blocks")
                .ok_or(SchematicError::Missing("Blocks"))?,
            "Palette",
            "Data",
            "BlockEntities",
        )
    } else if version == 2 {
        (s, "Palette", "BlockData", "BlockEntities")
    } else {
        (s, "Palette", "BlockData", "TileEntities")
    };
    let palette_tag = container
        .compound(palette_key)
        .ok_or(SchematicError::Missing("Palette"))?;
    if palette_tag.0.len() > 65_536 {
        return Err(SchematicError::BadData("Palette"));
    }
    // The file's indices need not start at 0 or be dense: map them to positions in `palette`.
    let mut palette = Vec::with_capacity(palette_tag.0.len());
    let mut dense: HashMap<u32, u16> = HashMap::with_capacity(palette_tag.0.len());
    for (text, index) in &palette_tag.0 {
        let Tag::Int(index) = index else {
            return Err(SchematicError::BadData("Palette"));
        };
        let index = u32::try_from(*index).map_err(|_| SchematicError::BadData("Palette"))?;
        let position =
            u16::try_from(palette.len()).map_err(|_| SchematicError::BadData("Palette"))?;
        if dense.insert(index, position).is_some() {
            return Err(SchematicError::BadData("Palette"));
        }
        palette.push(text.clone());
    }
    let Some(Tag::ByteArray(data)) = container.get(data_key) else {
        return Err(SchematicError::Missing("Data"));
    };
    let count = usize::try_from(volume).map_err(|_| SchematicError::TooBig(volume))?;
    let cells = varints(data, count, &dense)?;
    let origin = s
        .compound("Metadata")
        .and_then(|m| int3(m.get("Origin")))
        .map_or(Pos::new(0, 0, 0), |o| Pos::new(o[0], o[1], o[2]));
    let mut block_entities = Vec::new();
    if let Some(Tag::List(_, list)) = container.get(entities_key) {
        for item in list {
            let Tag::Compound(entity) = item else {
                continue;
            };
            let Some(pos) = int3(entity.get("Pos")) else {
                continue;
            };
            let Some(id) = entity
                .string("Id")
                .or_else(|| entity.string("id"))
                .map(str::to_owned)
            else {
                continue;
            };
            let inside = |v: i32, n: u32| v >= 0 && (v as u32) < n;
            if !(inside(pos[0], size.0) && inside(pos[1], size.1) && inside(pos[2], size.2)) {
                continue;
            }
            let mut data = if version == 3 {
                entity.compound("Data").cloned().unwrap_or_default()
            } else {
                let mut inline = entity.clone();
                inline.remove("Pos");
                inline.remove("Id");
                inline
            };
            data.put("id", Tag::String(id));
            let index = (pos[1] as usize * size.2 as usize + pos[2] as usize) * size.0 as usize
                + pos[0] as usize;
            block_entities.push((index, data));
        }
    }
    Ok(Decoded {
        version,
        data_version: s.int("DataVersion"),
        size,
        offset: Pos::new(offset[0], offset[1], offset[2]),
        origin,
        palette,
        cells,
        block_entities,
    })
}

#[derive(Debug)]
pub struct Loaded {
    pub clipboard: Clipboard,
    /// How many palette entries this server does not know; they load as air.
    pub unknown_count: usize,
    /// The first 16 of them.
    pub unknown: Vec<String>,
    /// The file's data version, when it says.
    pub data_version: Option<i64>,
}

/// Reads a schematic into a clipboard of this server's block states. Palette entries the server
/// does not know become `air`, counted in [`Loaded::unknown_count`]. Block ids are not upgraded
/// from older data versions (that table belongs to the server; see the README).
pub fn read(
    file: &[u8],
    limits: Limits,
    blocks: &dyn Blocks,
    air: u16,
) -> Result<Loaded, SchematicError> {
    let decoded = decode(file, limits)?;
    let mut states = Vec::with_capacity(decoded.palette.len());
    let mut unknown = Vec::new();
    let mut unknown_count = 0;
    for text in &decoded.palette {
        states.push(state_of(text, blocks).unwrap_or_else(|| {
            unknown_count += 1;
            if unknown.len() < 16 {
                unknown.push(text.clone());
            }
            air
        }));
    }
    let volume = decoded.cells.len() as u64;
    let mut clipboard = Clipboard::new(decoded.size, decoded.offset, decoded.origin)
        .ok_or(SchematicError::TooBig(volume))?;
    for (slot, cell) in clipboard.blocks.iter_mut().zip(&decoded.cells) {
        *slot = states[usize::from(*cell)];
    }
    for (index, data) in &decoded.block_entities {
        clipboard.entities.insert(*index, nbt::write_unnamed(data));
    }
    Ok(Loaded {
        clipboard,
        unknown_count,
        unknown,
        data_version: decoded.data_version,
    })
}

/// Writes `clipboard` as a version 3 file, with its rotation and flips applied.
pub fn write(clipboard: &Clipboard, blocks: &dyn Blocks, date_ms: i64) -> Vec<u8> {
    let at = Pos::new(0, 0, 0);
    let target = clipboard.target(at);
    let (w, h, l) = (
        (target.max.x - target.min.x + 1) as u32,
        (target.max.y - target.min.y + 1) as u32,
        (target.max.z - target.min.z + 1) as u32,
    );
    let mut cache = HashMap::new();
    let mut palette: BTreeMap<String, i32> = BTreeMap::new();
    let mut by_state: HashMap<u16, i32> = HashMap::new();
    let mut data = Vec::with_capacity(clipboard.blocks.len());
    let mut entities = Vec::new();
    for y in 0..h {
        for z in 0..l {
            for x in 0..w {
                let p = target.min.offset(x as i32, y as i32, z as i32);
                let cell = clipboard.source(at, p);
                let state = cell.map_or(0, |c| {
                    transform_state(
                        &clipboard.transform,
                        blocks,
                        clipboard.blocks[c],
                        &mut cache,
                    )
                });
                let index = match by_state.get(&state) {
                    Some(i) => *i,
                    None => {
                        let next = palette.len() as i32;
                        let i = *palette.entry(text_of(state, blocks)).or_insert(next);
                        by_state.insert(state, i);
                        i
                    }
                };
                put_varint(&mut data, index as u32);
                if let Some(bytes) = cell.and_then(|c| clipboard.entities.get(&c)) {
                    let Ok(mut be) = nbt::read_unnamed(bytes) else {
                        continue;
                    };
                    let id = match be.remove("id") {
                        Some(Tag::String(id)) => id,
                        _ => continue,
                    };
                    for k in ["x", "y", "z"] {
                        be.remove(k);
                    }
                    let mut entry = Compound::default();
                    entry.put("Pos", Tag::IntArray(vec![x as i32, y as i32, z as i32]));
                    entry.put("Id", Tag::String(id));
                    entry.put("Data", Tag::Compound(be));
                    entities.push(Tag::Compound(entry));
                }
            }
        }
    }
    // Palette entries in index order.
    let mut entries: Vec<(String, i32)> = palette.into_iter().collect();
    entries.sort_by_key(|(_, i)| *i);
    let palette = Compound(entries.into_iter().map(|(k, i)| (k, Tag::Int(i))).collect());

    let mut blocks_tag = Compound::default();
    blocks_tag.put("Palette", Tag::Compound(palette));
    blocks_tag.put("Data", Tag::ByteArray(data));
    blocks_tag.put("BlockEntities", Tag::List(10, entities));

    let mut meta = Compound::default();
    meta.put("Date", Tag::Long(date_ms));
    // Where the written (transformed) box sat in the world: the copy point plus its new offset,
    // so that `//paste -o` after a load lands where it did before the save.
    let copy_point = clipboard.original_point();
    let origin = copy_point.offset(target.min.x, target.min.y, target.min.z);
    meta.put("Origin", Tag::IntArray(vec![origin.x, origin.y, origin.z]));

    let mut s = Compound::default();
    s.put("Version", Tag::Int(3));
    s.put("DataVersion", Tag::Int(DATA_VERSION));
    s.put("Metadata", Tag::Compound(meta));
    s.put("Width", Tag::Short(w as u16 as i16));
    s.put("Height", Tag::Short(h as u16 as i16));
    s.put("Length", Tag::Short(l as u16 as i16));
    s.put(
        "Offset",
        Tag::IntArray(vec![target.min.x, target.min.y, target.min.z]),
    );
    s.put("Blocks", Tag::Compound(blocks_tag));
    let mut root = Compound::default();
    root.put("Schematic", Tag::Compound(s));
    gzip::gzip(&nbt::write_named("", &root))
}

/// A schematic name a player may type: letters, digits, `_` and `-`, 1 to 64 characters.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::fake::FakeBlocks;
    use crate::transform::Transform;

    const LIMITS: Limits = Limits {
        max_unpacked: 1 << 24,
        max_volume: 1 << 20,
    };

    fn sample(blocks: &FakeBlocks) -> Clipboard {
        let stairs = blocks
            .resolve("minecraft:oak_stairs", &[("facing".into(), "north".into())])
            .unwrap();
        let mut c = Clipboard::new((3, 2, 4), Pos::new(-1, 0, 2), Pos::new(100, 64, -20)).unwrap();
        for (i, b) in c.blocks.iter_mut().enumerate() {
            *b = if i % 3 == 0 { stairs } else { (i % 3) as u16 };
        }
        let mut chest = Compound::default();
        chest.put("id", Tag::String("minecraft:chest".into()));
        chest.put("x", Tag::Int(5));
        chest.put("y", Tag::Int(6));
        chest.put("z", Tag::Int(7));
        chest.put("CustomName", Tag::String("Loot".into()));
        let index = c.index(2, 1, 3);
        c.entities.insert(index, nbt::write_unnamed(&chest));
        c
    }

    #[test]
    fn a_written_file_reads_back_the_same() {
        let blocks = FakeBlocks::default();
        let c = sample(&blocks);
        let file = write(&c, &blocks, 1_700_000_000_000);
        let loaded = read(&file, LIMITS, &blocks, 0).unwrap();
        assert!(loaded.unknown.is_empty());
        assert_eq!(loaded.clipboard.size, c.size);
        assert_eq!(loaded.clipboard.offset, c.offset);
        assert_eq!(loaded.clipboard.original_point(), c.original_point());
        assert_eq!(loaded.clipboard.blocks, c.blocks);
        let be = nbt::read_unnamed(&loaded.clipboard.entities[&c.index(2, 1, 3)]).unwrap();
        assert_eq!(be.string("id"), Some("minecraft:chest"));
        assert_eq!(be.string("CustomName"), Some("Loot"));
        assert!(
            be.get("x").is_none(),
            "positions are the paste's, not the file's"
        );
    }

    #[test]
    fn a_rotated_clipboard_is_written_rotated() {
        let blocks = FakeBlocks::default();
        let mut c = sample(&blocks);
        c.transform = Transform::rotate_y(1);
        let loaded = read(&write(&c, &blocks, 0), LIMITS, &blocks, 0)
            .unwrap()
            .clipboard;
        assert_eq!(loaded.size, (4, 2, 3), "width and length trade places");
        let east = blocks
            .resolve("minecraft:oak_stairs", &[("facing".into(), "east".into())])
            .unwrap();
        let north = blocks
            .resolve("minecraft:oak_stairs", &[("facing".into(), "north".into())])
            .unwrap();
        assert!(loaded.blocks.contains(&east));
        assert!(!loaded.blocks.contains(&north));
        assert_eq!(loaded.entities.len(), 1);
        // Pasting the loaded file lands where pasting the rotated clipboard does.
        assert_eq!(
            loaded.target(Pos::new(0, 0, 0)),
            c.target(Pos::new(0, 0, 0))
        );
        // And `//paste -o` lands in the same place before the save and after the load.
        assert_eq!(
            loaded.target(loaded.original_point()),
            c.target(c.original_point())
        );
    }

    #[test]
    fn version_two_files_and_unknown_blocks() {
        let blocks = FakeBlocks::default();
        let mut palette = Compound::default();
        palette.put("minecraft:stone", Tag::Int(0));
        palette.put("othermod:thing[x=1]", Tag::Int(1));
        let mut s = Compound::default();
        s.put("Version", Tag::Int(2));
        s.put("Width", Tag::Short(2));
        s.put("Height", Tag::Short(1));
        s.put("Length", Tag::Short(1));
        s.put("Palette", Tag::Compound(palette));
        s.put("BlockData", Tag::ByteArray(vec![0, 1]));
        let mut chest = Compound::default();
        chest.put("Pos", Tag::IntArray(vec![0, 0, 0]));
        chest.put("Id", Tag::String("minecraft:chest".into()));
        chest.put("Lock", Tag::String("".into()));
        s.put("BlockEntities", Tag::List(10, vec![Tag::Compound(chest)]));
        let file = gzip::gzip(&nbt::write_named("Schematic", &s));
        let loaded = read(&file, LIMITS, &blocks, 0).unwrap();
        assert_eq!(loaded.clipboard.blocks, vec![1, 0]);
        assert_eq!(loaded.unknown, vec!["othermod:thing[x=1]".to_owned()]);
        let be = nbt::read_unnamed(&loaded.clipboard.entities[&0]).unwrap();
        assert_eq!(be.string("id"), Some("minecraft:chest"));
        assert!(be.get("Lock").is_some());
    }

    #[test]
    fn bad_files_are_refused() {
        let blocks = FakeBlocks::default();
        let c = sample(&blocks);
        let file = write(&c, &blocks, 0);
        assert!(
            read(
                &file,
                Limits {
                    max_volume: 10,
                    ..LIMITS
                },
                &blocks,
                0
            )
            .is_err()
        );
        assert!(
            read(
                &file,
                Limits {
                    max_unpacked: 100,
                    ..LIMITS
                },
                &blocks,
                0
            )
            .is_err()
        );
        assert!(read(b"not a file", LIMITS, &blocks, 0).is_err());
        for cut in [0, 10, file.len() / 2, file.len() - 1] {
            assert!(read(&file[..cut], LIMITS, &blocks, 0).is_err());
        }
        // A data array that is too short, and an index outside the palette.
        let mk = |data: Vec<u8>| {
            let mut palette = Compound::default();
            palette.put("minecraft:stone", Tag::Int(0));
            let mut b = Compound::default();
            b.put("Palette", Tag::Compound(palette));
            b.put("Data", Tag::ByteArray(data));
            let mut s = Compound::default();
            s.put("Version", Tag::Int(3));
            s.put("Width", Tag::Short(2));
            s.put("Height", Tag::Short(1));
            s.put("Length", Tag::Short(1));
            s.put("Blocks", Tag::Compound(b));
            let mut root = Compound::default();
            root.put("Schematic", Tag::Compound(s));
            gzip::gzip(&nbt::write_named("", &root))
        };
        assert!(read(&mk(vec![0, 0]), LIMITS, &blocks, 0).is_ok());
        assert_eq!(
            read(&mk(vec![0]), LIMITS, &blocks, 0).unwrap_err(),
            SchematicError::BadData("Data")
        );
        assert_eq!(
            read(&mk(vec![0, 5]), LIMITS, &blocks, 0).unwrap_err(),
            SchematicError::BadData("Data")
        );
        assert_eq!(
            read(&mk(vec![0, 0x80]), LIMITS, &blocks, 0).unwrap_err(),
            SchematicError::BadData("Data")
        );
    }

    #[test]
    fn decode_needs_no_server_and_keeps_the_file_facts() {
        let blocks = FakeBlocks::default();
        let c = sample(&blocks);
        let file = write(&c, &blocks, 0);
        let d = decode(&file, LIMITS).unwrap();
        assert_eq!(d.version, 3);
        assert_eq!(d.data_version, Some(i64::from(DATA_VERSION)));
        assert_eq!(d.size, c.size);
        assert_eq!(d.cells.len(), 24);
        assert!(
            d.palette
                .iter()
                .any(|t| t == "minecraft:oak_stairs[facing=north,half=top,shape=straight]")
        );
        assert_eq!(d.block_entities.len(), 1);
        assert_eq!(d.block_entities[0].0, c.index(2, 1, 3));
        assert_eq!(d.block_entities[0].1.string("id"), Some("minecraft:chest"));
    }

    fn v3(palette: &[(&str, i32)], data: Vec<u8>, w: i16, data_version: Option<i32>) -> Vec<u8> {
        let mut p = Compound::default();
        for (text, i) in palette {
            p.put(text, Tag::Int(*i));
        }
        let mut b = Compound::default();
        b.put("Palette", Tag::Compound(p));
        b.put("Data", Tag::ByteArray(data));
        let mut s = Compound::default();
        s.put("Version", Tag::Int(3));
        if let Some(v) = data_version {
            s.put("DataVersion", Tag::Int(v));
        }
        s.put("Width", Tag::Short(w));
        s.put("Height", Tag::Short(1));
        s.put("Length", Tag::Short(1));
        s.put("Blocks", Tag::Compound(b));
        let mut root = Compound::default();
        root.put("Schematic", Tag::Compound(s));
        gzip::gzip(&nbt::write_named("", &root))
    }

    #[test]
    fn sparse_palette_indices_are_mapped_and_duplicates_refused() {
        let blocks = FakeBlocks::default();
        // Indices 7 and 300 (a two-byte varint), not 0 and 1.
        let file = v3(
            &[("minecraft:stone", 7), ("minecraft:dirt", 300)],
            vec![7, 0xAC, 0x02, 7],
            3,
            Some(3700),
        );
        let loaded = read(&file, LIMITS, &blocks, 0).unwrap();
        assert_eq!(loaded.clipboard.blocks, vec![1, 2, 1]);
        assert_eq!(loaded.data_version, Some(3700));
        let dup = v3(
            &[("minecraft:stone", 0), ("minecraft:dirt", 0)],
            vec![0],
            1,
            None,
        );
        assert_eq!(
            decode(&dup, LIMITS).unwrap_err(),
            SchematicError::BadData("Palette")
        );
    }

    #[test]
    fn every_unknown_entry_is_counted() {
        let blocks = FakeBlocks::default();
        let mut palette: Vec<(String, i32)> = (0..20)
            .map(|i| (format!("othermod:thing_{i}"), i))
            .collect();
        palette.push(("minecraft:stone".into(), 20));
        let refs: Vec<(&str, i32)> = palette.iter().map(|(t, i)| (t.as_str(), *i)).collect();
        let file = v3(&refs, (0..21).collect(), 21, None);
        let loaded = read(&file, LIMITS, &blocks, 0).unwrap();
        assert_eq!(loaded.unknown_count, 20);
        assert_eq!(loaded.unknown.len(), 16);
        assert_eq!(loaded.data_version, None);
        assert_eq!(loaded.clipboard.blocks[20], 1);
    }

    #[test]
    fn names_are_plain() {
        assert!(valid_name("castle_v2-final"));
        assert!(!valid_name("../escape"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"x".repeat(65)));
    }
}
