# block-edit

A library crate, not a plugin: the pure part of block editing, shared so that no plugin carries a
second schematic parser or a second bounded-edit loop. It has no dependency and makes no SDK call.
The server comes in through two traits:

- `pattern::Blocks`: the block registry (resolve `name` + properties to a state, list a block's
  states, name a state). A plugin implements it once over the SDK (worldeditor-lite's
  `src/host.rs`).
- `job::WorldAccess`: get and set block states and block entity data, loaded-chunk check, biomes,
  a clock.

| Module | What |
| --- | --- |
| `geom` | positions, boxes, walls and outlines without overlaps, splits per chunk, sphere and cylinder shapes, a chunk-ordered cursor |
| `pattern` | block text (`oak_stairs[facing=east,half=top]`), weighted patterns (`70%stone,30%andesite`), masks (`!stone`), state matching from a block's own state list |
| `transform` | quarter turns and flips of positions and of block states (facing, axis, rotation, sides, stair shapes, hinges, rails, halves) |
| `clipboard`, `history` | copied boxes with a pending transform; undo and redo with per-player and total memory limits, a step limit, and forgetting a world |
| `job` | edits as jobs in bounded steps (time and block budget per call); a job never reads or writes an unloaded chunk; block entity data can be switched off per job |
| `nbt`, `gzip` | bounded readers and writers (depth, lengths, total size); never a panic on bad input |
| `schematic` | `.schem` schematics: `decode` (v1 to v3, no server needed: size, offset, origin, data version, palette text, cells, block entities), `read` into a clipboard, `write` v3 |

## Reading a schematic without a server

```rust
use block_edit::schematic::{decode, Limits};

let file = std::fs::read("data/house.schem")?;
let d = decode(&file, Limits { max_unpacked: 256 << 20, max_volume: 30_000_000 })?;
// d.size, d.offset, d.data_version, d.palette[i] is "minecraft:oak_stairs[facing=east,...]",
// d.cells[x + z * width + y * width * length] is a position in d.palette.
```

Every limit is the caller's. Cells are `u16` positions in the palette (a palette has at most
65,536 entries), so a file of n blocks needs 2n bytes. Block ids are not upgraded from older data
versions: that table belongs to the server. `decode` returns the file's `data_version` so a caller
can warn.

## Build and test

From the root of this repository: `cargo test -p block-edit` (47 tests). worldeditor-lite uses it
as `block-edit = { path = "libs/block-edit" }`.
