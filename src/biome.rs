//! Biomes: the contract's biome list with registry names, and the one place that sets biomes.
//!
//! The SDK has no call yet that sets a biome in a live world (`set-biome` and `fill-biome`
//! exist only on the generator's chunk buffer), so a biome edit runs the server's own
//! `/fillbiome` from the console, through `execute in <dimension>`, piece by piece. The
//! command text holds only numbers and a name from the table below.

use strawberry_plugin_api::server::CommandSender as ExecSender;
use strawberry_plugin_api::wit::strawberry::plugin::biomes::Biome;
use strawberry_plugin_api::{Server, World};

use crate::geom::Pos;

/// The most biome cells one `/fillbiome` takes (the server's own limit).
pub const MAX_CELLS: u64 = 32_768;
/// Every biome of the contract (`biomes.wit`) with its registry name, in contract order.
pub const BIOMES: [(Biome, &str); 67] = [
    (Biome::Badlands, "minecraft:badlands"),
    (Biome::BambooJungle, "minecraft:bamboo_jungle"),
    (Biome::BasaltDeltas, "minecraft:basalt_deltas"),
    (Biome::Beach, "minecraft:beach"),
    (Biome::BirchForest, "minecraft:birch_forest"),
    (Biome::CherryGrove, "minecraft:cherry_grove"),
    (Biome::ColdOcean, "minecraft:cold_ocean"),
    (Biome::CrimsonForest, "minecraft:crimson_forest"),
    (Biome::DappledForest, "minecraft:dappled_forest"),
    (Biome::DarkForest, "minecraft:dark_forest"),
    (Biome::DeepColdOcean, "minecraft:deep_cold_ocean"),
    (Biome::DeepDark, "minecraft:deep_dark"),
    (Biome::DeepFrozenOcean, "minecraft:deep_frozen_ocean"),
    (Biome::DeepLukewarmOcean, "minecraft:deep_lukewarm_ocean"),
    (Biome::DeepOcean, "minecraft:deep_ocean"),
    (Biome::Desert, "minecraft:desert"),
    (Biome::DripstoneCaves, "minecraft:dripstone_caves"),
    (Biome::EndBarrens, "minecraft:end_barrens"),
    (Biome::EndHighlands, "minecraft:end_highlands"),
    (Biome::EndMidlands, "minecraft:end_midlands"),
    (Biome::ErodedBadlands, "minecraft:eroded_badlands"),
    (Biome::FlowerForest, "minecraft:flower_forest"),
    (Biome::Forest, "minecraft:forest"),
    (Biome::FrozenOcean, "minecraft:frozen_ocean"),
    (Biome::FrozenPeaks, "minecraft:frozen_peaks"),
    (Biome::FrozenRiver, "minecraft:frozen_river"),
    (Biome::Grove, "minecraft:grove"),
    (Biome::IceSpikes, "minecraft:ice_spikes"),
    (Biome::JaggedPeaks, "minecraft:jagged_peaks"),
    (Biome::Jungle, "minecraft:jungle"),
    (Biome::LukewarmOcean, "minecraft:lukewarm_ocean"),
    (Biome::LushCaves, "minecraft:lush_caves"),
    (Biome::MangroveSwamp, "minecraft:mangrove_swamp"),
    (Biome::Meadow, "minecraft:meadow"),
    (Biome::MushroomFields, "minecraft:mushroom_fields"),
    (Biome::NetherWastes, "minecraft:nether_wastes"),
    (Biome::Ocean, "minecraft:ocean"),
    (
        Biome::OldGrowthBirchForest,
        "minecraft:old_growth_birch_forest",
    ),
    (Biome::OldGrowthPineTaiga, "minecraft:old_growth_pine_taiga"),
    (
        Biome::OldGrowthSpruceTaiga,
        "minecraft:old_growth_spruce_taiga",
    ),
    (Biome::PaleGarden, "minecraft:pale_garden"),
    (Biome::Plains, "minecraft:plains"),
    (Biome::River, "minecraft:river"),
    (Biome::Savanna, "minecraft:savanna"),
    (Biome::SavannaPlateau, "minecraft:savanna_plateau"),
    (Biome::SmallEndIslands, "minecraft:small_end_islands"),
    (Biome::SnowyBeach, "minecraft:snowy_beach"),
    (Biome::SnowyPlains, "minecraft:snowy_plains"),
    (Biome::SnowySlopes, "minecraft:snowy_slopes"),
    (Biome::SnowyTaiga, "minecraft:snowy_taiga"),
    (Biome::SoulSandValley, "minecraft:soul_sand_valley"),
    (Biome::SparseJungle, "minecraft:sparse_jungle"),
    (Biome::StonyPeaks, "minecraft:stony_peaks"),
    (Biome::StonyShore, "minecraft:stony_shore"),
    (Biome::SulfurCaves, "minecraft:sulfur_caves"),
    (Biome::SunflowerPlains, "minecraft:sunflower_plains"),
    (Biome::Swamp, "minecraft:swamp"),
    (Biome::Taiga, "minecraft:taiga"),
    (Biome::TheEnd, "minecraft:the_end"),
    (Biome::TheVoid, "minecraft:the_void"),
    (Biome::WarmOcean, "minecraft:warm_ocean"),
    (Biome::WarpedForest, "minecraft:warped_forest"),
    (Biome::WindsweptForest, "minecraft:windswept_forest"),
    (
        Biome::WindsweptGravellyHills,
        "minecraft:windswept_gravelly_hills",
    ),
    (Biome::WindsweptHills, "minecraft:windswept_hills"),
    (Biome::WindsweptSavanna, "minecraft:windswept_savanna"),
    (Biome::WoodedBadlands, "minecraft:wooded_badlands"),
];

/// The table index of a biome name (`desert` or `minecraft:desert`).
#[must_use]
pub fn index_of(name: &str) -> Option<u8> {
    let name = name.trim().to_ascii_lowercase();
    let full = if name.contains(':') {
        name
    } else {
        format!("minecraft:{name}")
    };
    BIOMES
        .iter()
        .position(|(_, n)| *n == full)
        .and_then(|i| u8::try_from(i).ok())
}

#[must_use]
pub fn name_of(index: u8) -> Option<&'static str> {
    BIOMES.get(usize::from(index)).map(|(_, n)| *n)
}

/// The biome of the cell that holds `cell` (cell coordinates).
#[must_use]
pub fn get(world: &World, cell: Pos) -> Option<u8> {
    let at = crate::host::wit(Pos::new(
        cell.x.wrapping_mul(4),
        cell.y.wrapping_mul(4),
        cell.z.wrapping_mul(4),
    ));
    let biome = world.get_biome(at);
    BIOMES
        .iter()
        .position(|(b, _)| *b == biome)
        .and_then(|i| u8::try_from(i).ok())
}

/// The `/fillbiome` line for a box of cells.
#[must_use]
pub fn command(dimension: &str, min: Pos, max: Pos, biome: u8) -> Option<String> {
    let name = name_of(biome)?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars().all(|c| {
                c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || matches!(c, '_' | ':' | '/' | '.' | '-')
            })
    };
    if !ok(dimension) {
        return None;
    }
    Some(format!(
        "execute in {dimension} run fillbiome {} {} {} {} {} {} {name}",
        min.x * 4,
        min.y * 4,
        min.z * 4,
        max.x * 4 + 3,
        max.y * 4 + 3,
        max.z * 4 + 3
    ))
}

pub fn fill(server: &Server, dimension: &str, min: Pos, max: Pos, biome: u8) {
    if let Some(line) = command(dimension, min, max, biome) {
        server.execute_command(&line, ExecSender::Console);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_commands() {
        assert_eq!(BIOMES.len(), 67);
        let desert = index_of("desert").unwrap();
        assert_eq!(index_of("minecraft:desert"), Some(desert));
        assert_eq!(index_of("DESERT"), Some(desert));
        assert_eq!(index_of("desert run say hi"), None);
        assert_eq!(index_of("nope"), None);
        assert_eq!(name_of(desert), Some("minecraft:desert"));
        assert_eq!(
            command(
                "minecraft:overworld",
                Pos::new(-1, 16, 0),
                Pos::new(2, 17, 3),
                desert
            )
            .as_deref(),
            Some("execute in minecraft:overworld run fillbiome -4 64 0 11 71 15 minecraft:desert")
        );
        assert_eq!(
            command("bad dim", Pos::new(0, 0, 0), Pos::new(0, 0, 0), desert),
            None
        );
    }
}
