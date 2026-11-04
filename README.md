# World Editor Lite

Building tools for Play Strawberry servers: selections with a wand or by position, region edits
(set, replace, walls, outline), a clipboard (copy, cut, paste, rotate, flip), undo and redo,
schematic files, simple brushes and biome edits.

Every edit runs in small steps over game ticks, so a large edit takes longer but never freezes the
server. You see a progress line above the hotbar and one chat line at the end. Nothing is read
from or written to a chunk that is not loaded; an edit that reaches one waits for it, then stops.

## Commands

Region commands start with two slashes. Each command has its own permission,
`worldeditor-lite:<name>` (for example `worldeditor-lite:set`).

| Command | What it does |
| --- | --- |
| `//wand` | Gives the selection wand (a wooden axe): left click sets the first corner, right click the second |
| `//pos1 [x y z]`, `//pos2 [x y z]` | Sets a corner where you stand, or at the coordinates |
| `//hpos1`, `//hpos2` | Sets a corner at the block you look at |
| `//desel`, `//size` | Clears the selection; shows its size |
| `//set <pattern>` | Fills the selection |
| `//replace [from] <to>` | Replaces `from` (every non-air block when left out) with `to` |
| `//walls <pattern>`, `//outline <pattern>` (`//faces`) | The four sides; all six faces |
| `//copy`, `//cut [pattern]` | Copies the selection, relative to you; cut also clears it |
| `//paste [-a] [-o]` | Pastes; `-a` skips air, `-o` pastes where it was copied |
| `//rotate <degrees>`, `//flip [direction]` | Turns or mirrors the clipboard, block states included |
| `//undo`, `//redo`, `//clearhistory` | Undo history |
| `//cancel` | Stops your running edit |
| `//schem save\|load\|delete <name>`, `//schem list` | Schematic files |
| `/brush sphere <pattern> [radius]`, `/brush cylinder <pattern> [radius] [height]`, `/brush none` (`/br`) | Binds a brush to the item in your hand |
| `//setbiome <biome>` | Sets the biome of the selection |

Patterns: a block (`stone`), a block with properties (`oak_stairs[facing=east,half=top]`) or a
weighted list (`70%stone,30%andesite`). A `!` in front of a mask matches every other block.

## Permissions

Every `worldeditor-lite:<command>` permission is given to operators (level 2) by default.
`worldeditor-lite:*` gives all of them. A player without a command's permission gets the server's
unknown-command answer.

`worldeditor-lite:block-entity-data` (operators by default) lets an edit carry container contents
and sign text. Without it, copy, paste, undo and schematics move blocks only, so nobody can
multiply what a chest holds.

## Settings

`plugins/data/worldeditor-lite/config.txt` is written with the defaults on the first start. A bad
value keeps its default and is named in the console.

| Key | Default | Meaning |
| --- | --- | --- |
| `max-blocks-per-edit` | 500000 | the largest edit |
| `history-mib-per-player`, `history-mib-total` | 8, 24 | undo memory |
| `history-steps-per-player` | 0 | edits each player can undo; 0 is no step limit |
| `session-keep-minutes` | 0 | how long a selection and clipboard are kept after leaving |
| `edit-log` | false | one console line per finished edit |
| `clipboard-max-blocks`, `clipboard-total-blocks` | 1000000, 4000000 | clipboard memory |
| `tick-budget-ms` | 3 | milliseconds of each tick for all edits together |
| `max-blocks-per-tick` | 20000 | blocks per tick for all edits together |
| `progress-ticks` | 20 | ticks between progress lines |
| `chunk-wait-seconds` | 30 | how long an edit waits for an unloaded chunk |
| `neighbor-updates` | true | edited blocks update their neighbours, as placed blocks do |
| `brush-max-radius` | 8 | the largest brush |
| `schematic-max-kib` | 8192 | the largest schematic file read |
| `wand-item` | `minecraft:wooden_axe` | the wand |

Undo history and clipboards live in memory: a restart or reload clears them.

## Schematics

Files are `plugins/data/worldeditor-lite/schematics/<name>.schem` (versions 1, 2 and 3 are read,
version 3 is written). A block the server does not know loads as air, and you are told which ones.
`//setbiome` works in the first world of each dimension.

Edits set blocks directly: no block place or break event fires for them, and protection plugins
are not asked.

## Languages

English, French, Spanish and Turkish, chosen by each player's game language (`lang/`).

## Install on a Play Strawberry server

1. Add World Editor Lite to your server in the Play Strawberry panel.
2. Restart the server. The plugin creates `plugins/data/worldeditor-lite/` on the first start.
3. Give the command permissions to your builders.

Play Strawberry servers load only plugins signed by Play Strawberry, so a file you build yourself
from this source does not load on them.

## Build from source

You need Rust with the `wasm32-wasip2` target, the Strawberry plugin SDK
(`strawberry-plugin-api`), which Play Strawberry gives to its plugin developers, and the
`block-edit` library. Put both in folders next to this one (`strawberry-plugin-api` and
`block-edit`), or change their paths in `Cargo.toml`.

```text
cargo test
cargo build --release --target wasm32-wasip2
```

The plugin file is `target/wasm32-wasip2/release/worldeditor_lite.wasm`.

## License

Copyright Strawberry Interactive LLC. The license text is not published yet; until it is, all
rights are reserved.
