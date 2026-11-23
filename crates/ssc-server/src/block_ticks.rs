//! Random-ticked grass rules. Lighting is a direct-skylight approximation
//! until the server has propagated block/sky light.

use std::collections::{BTreeMap, HashSet, VecDeque};

use ssc_world::{BlockId, World, DIRT, GRASS_BLOCK, MAX_Y, MIN_Y, WATER};

pub type BlockChange = ((i32, i32, i32), BlockId);

fn mix(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Approximate Bedrock random ticks: several deterministic positions per
/// allocated section per tick. The count is a simplification, not the exact
/// relative randomTickSpeed scheduler.
pub const SAMPLES_PER_SECTION: usize = 4;

fn sample_at(
    tick: u64,
    seed: u64,
    cx: i32,
    cz: i32,
    section: usize,
    index: usize,
) -> ((i32, i32, i32), u64) {
    let roll = mix(tick.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ seed
        ^ (cx as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        ^ (cz as u64).wrapping_mul(0x1656_67b1_9e37_79f9)
        ^ (section as u64).wrapping_mul(0xd1b5_4a32_d192_ed05)
        ^ (index as u64).wrapping_mul(0x2545_f491_4f6c_dd1d));
    (
        (
            cx * 16 + (roll & 15) as i32,
            MIN_Y + section as i32 * 16 + ((roll >> 4) & 15) as i32,
            cz * 16 + ((roll >> 8) & 15) as i32,
        ),
        mix(roll ^ 0xb10c_71c5),
    )
}

/// One deterministic sample per allocated section per tick.
pub fn sample(tick: u64, seed: u64, cx: i32, cz: i32, section: usize) -> ((i32, i32, i32), u64) {
    sample_at(tick, seed, cx, cz, section, 0)
}

pub fn samples(
    tick: u64,
    seed: u64,
    cx: i32,
    cz: i32,
    section: usize,
) -> [((i32, i32, i32), u64); SAMPLES_PER_SECTION] {
    std::array::from_fn(|index| sample_at(tick, seed, cx, cz, section, index))
}

fn blocks_light(block: BlockId) -> bool {
    ssc_world::is_opaque(block) && !ssc_world::is_oak_leaves(block)
}

fn skylight(world: &World, x: i32, y: i32, z: i32, daylight: bool) -> u8 {
    let mut light: u8 = if daylight { 15 } else { 4 };
    for above in y + 1..MAX_Y {
        let block = world.block_or_air(x, above, z);
        if blocks_light(block) {
            return 0;
        }
        if block == WATER || ssc_world::is_oak_leaves(block) {
            light = light.saturating_sub(1);
        }
    }
    light
}

/// Grass can spread one block horizontally, one upward or three down.
/// Four independently sampled targets are attempted on a random tick.
pub fn grass_changes(
    world: &World,
    x: i32,
    y: i32,
    z: i32,
    mut roll: u64,
    daylight: bool,
) -> Vec<BlockChange> {
    if world.block_or_air(x, y, z) != GRASS_BLOCK {
        return Vec::new();
    }
    let above = world.block_or_air(x, y + 1, z);
    if blocks_light(above) || above == WATER {
        return vec![((x, y, z), DIRT)];
    }
    let light = skylight(world, x, y, z, daylight);
    if light < 9 {
        return Vec::new();
    }
    let mut changes = Vec::new();
    for _ in 0..4 {
        roll = mix(roll.wrapping_add(0x9e37_79b9_7f4a_7c15));
        let dx = (roll % 3) as i32 - 1;
        let dy = ((roll >> 8) % 5) as i32 - 3;
        let dz = ((roll >> 16) % 3) as i32 - 1;
        let target = (x + dx, y + dy, z + dz);
        if !(MIN_Y..MAX_Y - 1).contains(&target.1)
            || world.get_block(target.0, target.1, target.2) != Some(DIRT)
        {
            continue;
        }
        let cover = world.block_or_air(target.0, target.1 + 1, target.2);
        if cover != WATER && !blocks_light(cover) {
            let change = (target, GRASS_BLOCK);
            if !changes.contains(&change) {
                changes.push(change);
            }
        }
    }
    changes
}

/// Bedrock oak leaves need a log within four connected leaf steps. Treat
/// missing neighbor columns as unknown support so loading edges cannot
/// destroy a canopy whose trunk has not arrived yet.
pub fn leaf_can_decay(world: &World, x: i32, y: i32, z: i32) -> bool {
    if !ssc_world::is_oak_leaves(world.block_or_air(x, y, z)) {
        return false;
    }
    for cx in (x - 4).div_euclid(16)..=(x + 4).div_euclid(16) {
        for cz in (z - 4).div_euclid(16)..=(z + 4).div_euclid(16) {
            if world.column(cx, cz).is_none() {
                return false;
            }
        }
    }
    let mut visited = HashSet::new();
    let mut queue = VecDeque::from([((x, y, z), 0)]);
    visited.insert((x, y, z));
    while let Some(((x, y, z), distance)) = queue.pop_front() {
        if distance >= 4 {
            continue;
        }
        for (dx, dy, dz) in [
            (1, 0, 0),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ] {
            let neighbor = (x + dx, y + dy, z + dz);
            let block = world.block_or_air(neighbor.0, neighbor.1, neighbor.2);
            if matches!(block, ssc_world::OAK_LOG | ssc_world::STRIPPED_OAK_LOG) {
                return false;
            }
            if ssc_world::is_oak_leaves(block) && visited.insert(neighbor) {
                queue.push_back((neighbor, distance + 1));
            }
        }
    }
    true
}

/// Farmland hydrates from water within four blocks horizontally and one
/// block vertically.
pub fn farmland_should_be_wet(world: &World, x: i32, y: i32, z: i32) -> bool {
    for dx in -4..=4 {
        for dz in -4..=4 {
            for dy in -1..=1 {
                if world.block_or_air(x + dx, y + dy, z + dz) == WATER {
                    return true;
                }
            }
        }
    }
    false
}

/// Wheat advances through eight saved growth ages. Growth needs daylight
/// light 9 (approximated as direct skylight) and hydrated farmland raises
/// the per-tick chance. Bedrock's exact growth-point formula is simplified.
pub fn wheat_changes(
    world: &World,
    x: i32,
    y: i32,
    z: i32,
    roll: u64,
    daylight: bool,
    hydrated: bool,
) -> Vec<BlockChange> {
    let block = world.block_or_air(x, y, z);
    let Some(age) = ssc_world::wheat_age(block) else {
        return Vec::new();
    };
    if !ssc_world::is_farmland(world.block_or_air(x, y - 1, z)) {
        return vec![((x, y, z), ssc_world::AIR)];
    }
    if skylight(world, x, y, z, daylight) < 9 {
        return Vec::new();
    }
    if age >= 7 || roll % 7 >= if hydrated { 3 } else { 1 } {
        return Vec::new();
    }
    vec![((x, y, z), block + 1)]
}

pub fn can_plant_sapling(world: &World, x: i32, y: i32, z: i32) -> bool {
    (MIN_Y + 1..MAX_Y).contains(&y) && matches!(world.block_or_air(x, y - 1, z), DIRT | GRASS_BLOCK)
}

/// Two saved growth stages, with a 1/7 advancement chance per eligible
/// random tick. Tree geometry and clearance are deliberately basic.
pub fn sapling_changes(
    world: &World,
    x: i32,
    y: i32,
    z: i32,
    roll: u64,
    daylight: bool,
) -> Vec<BlockChange> {
    let sapling = world.block_or_air(x, y, z);
    if !ssc_world::is_oak_sapling(sapling) {
        return Vec::new();
    }
    if !can_plant_sapling(world, x, y, z) {
        return vec![((x, y, z), ssc_world::AIR)];
    }
    if roll % 7 != 0 || skylight(world, x, y, z, daylight) < 9 {
        return Vec::new();
    }
    if sapling == ssc_world::OAK_SAPLING {
        return vec![((x, y, z), ssc_world::OAK_SAPLING_READY)];
    }
    let height = 4 + ((roll >> 8) % 3) as i32;
    if y + height >= MAX_Y {
        return Vec::new();
    }
    let mut tree = BTreeMap::new();
    for dy in height - 3..=height {
        let radius: i32 = if dy < height - 1 { 2 } else { 1 };
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                if radius == 2 && dx.abs() == 2 && dz.abs() == 2 {
                    continue;
                }
                tree.insert((x + dx, y + dy, z + dz), ssc_world::GROWN_OAK_LEAVES);
            }
        }
    }
    for dy in 0..height {
        tree.insert((x, y + dy, z), ssc_world::OAK_LOG);
    }
    // Validate the entire plan before changing anything. In particular,
    // never overwrite a building or generate into unloaded columns.
    for (&(tx, ty, tz), _) in &tree {
        let Some(existing) = world.get_block(tx, ty, tz) else {
            return Vec::new();
        };
        if existing != ssc_world::AIR
            && !ssc_world::is_oak_leaves(existing)
            && (tx, ty, tz) != (x, y, z)
        {
            return Vec::new();
        }
    }
    tree.insert((x, y - 1, z), DIRT);
    tree.into_iter()
        .filter(|(pos, block)| {
            let old = world.block_or_air(pos.0, pos.1, pos.2);
            // Canopies do not turn existing persistent leaf placements into
            // natural leaves. Trunks may replace leaves normally.
            old != *block
                && !(*block == ssc_world::GROWN_OAK_LEAVES && ssc_world::is_oak_leaves(old))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn glass_passes_the_direct_skylight_estimate() {
        let mut world = ssc_world::World::default();
        world.set_block(8, 64, 8, ssc_world::GRASS_BLOCK);
        world.set_block(8, 65, 8, ssc_world::GLASS);
        assert_eq!(skylight(&world, 8, 64, 8, true), 15);
        assert!(grass_changes(&world, 8, 64, 8, 0, false).is_empty());
    }
    use super::*;
    use ssc_world::{AIR, OAK_LEAVES};

    #[test]
    fn samples_are_deterministic_and_stay_in_their_section() {
        for (cx, cz) in [(-2, -1), (0, 0), (1, 2)] {
            for section in 0..ssc_world::SUBCHUNKS_PER_COLUMN {
                for tick in 0..32 {
                    let result = sample(tick, 42, cx, cz, section);
                    assert_eq!(result, sample(tick, 42, cx, cz, section));
                    let ((x, y, z), _) = result;
                    assert_eq!(x.div_euclid(16), cx);
                    assert_eq!(z.div_euclid(16), cz);
                    assert_eq!(((y - MIN_Y) / 16) as usize, section);
                }
            }
        }
    }

    #[test]
    fn grass_dies_under_solid_cover_but_not_in_open_night_air() {
        let mut world = World::default();
        world.set_block(0, 54, 0, GRASS_BLOCK);
        assert!(grass_changes(&world, 0, 54, 0, 0, false).is_empty());
        world.set_block(0, 55, 0, ssc_world::STONE);
        assert_eq!(
            grass_changes(&world, 0, 54, 0, 0, true),
            vec![((0, 54, 0), DIRT)]
        );
        world.set_block(0, 55, 0, AIR);
        world.set_block(0, 58, 0, ssc_world::STONE);
        assert!(grass_changes(&world, 0, 54, 0, 0, true).is_empty());
    }

    #[test]
    fn leaf_cover_does_not_kill_grass_at_night_but_water_does() {
        let mut world = World::default();
        world.set_block(0, 54, 0, GRASS_BLOCK);
        world.set_block(0, 55, 0, OAK_LEAVES);
        assert!(grass_changes(&world, 0, 54, 0, 0, false).is_empty());
        world.set_block(0, 55, 0, WATER);
        assert_eq!(
            grass_changes(&world, 0, 54, 0, 0, true),
            vec![((0, 54, 0), DIRT)]
        );
    }

    #[test]
    fn grass_spreads_across_columns_and_down_slopes_but_not_under_water() {
        let mut world = World::default();
        world.set_block(-1, 54, 0, GRASS_BLOCK);
        world.set_block(0, 54, 0, DIRT);
        world.set_block(-2, 51, 0, DIRT);
        world.set_block(-1, 54, 1, DIRT);
        world.set_block(-1, 55, 1, WATER);
        let mut flat = false;
        let mut downhill = false;
        for roll in 0..256 {
            let changes = grass_changes(&world, -1, 54, 0, roll, true);
            flat |= changes.contains(&((0, 54, 0), GRASS_BLOCK));
            downhill |= changes.contains(&((-2, 51, 0), GRASS_BLOCK));
            assert!(!changes.contains(&((-1, 54, 1), GRASS_BLOCK)));
            assert!(grass_changes(&world, -1, 54, 0, roll, false).is_empty());
        }
        assert!(flat && downhill);
    }

    #[test]
    fn leaves_need_connected_log_support_within_bedrocks_four_blocks() {
        let mut world = World::default();
        for x in 4..=8 {
            world.set_block(x, 64, 8, OAK_LEAVES);
        }
        world.set_block(4, 64, 8, ssc_world::OAK_LOG);
        assert!(!leaf_can_decay(&world, 8, 64, 8));
        world.set_block(4, 64, 8, ssc_world::STRIPPED_OAK_LOG);
        assert!(!leaf_can_decay(&world, 8, 64, 8));
        world.set_block(4, 64, 8, OAK_LEAVES);
        world.set_block(3, 64, 8, ssc_world::OAK_LOG);
        assert!(
            leaf_can_decay(&world, 8, 64, 8),
            "five blocks away is too far"
        );
        world.set_block(4, 64, 8, ssc_world::OAK_LOG);
        world.set_block(6, 64, 8, ssc_world::AIR);
        assert!(leaf_can_decay(&world, 8, 64, 8), "air gaps break support");
    }

    #[test]
    fn leaves_at_loading_edges_wait_for_neighbor_columns() {
        let mut world = World::default();
        world.set_block(15, 64, 8, OAK_LEAVES);
        assert!(!leaf_can_decay(&world, 15, 64, 8));
        world.column_mut(1, 0);
        assert!(leaf_can_decay(&world, 15, 64, 8));
        world.set_block(16, 64, 8, ssc_world::OAK_LOG);
        assert!(!leaf_can_decay(&world, 15, 64, 8));
    }

    #[test]
    fn saplings_use_two_growth_stages_and_need_light_and_soil() {
        let mut world = World::default();
        world.set_block(8, 63, 8, GRASS_BLOCK);
        world.set_block(8, 64, 8, ssc_world::OAK_SAPLING);
        assert!(sapling_changes(&world, 8, 64, 8, 0, false).is_empty());
        assert!(sapling_changes(&world, 8, 64, 8, 1, true).is_empty());
        assert_eq!(
            sapling_changes(&world, 8, 64, 8, 0, true),
            vec![((8, 64, 8), ssc_world::OAK_SAPLING_READY)]
        );
        world.set_block(8, 64, 8, ssc_world::OAK_SAPLING_READY);
        world.set_block(10, 65, 8, ssc_world::STONE);
        assert!(
            sapling_changes(&world, 8, 64, 8, 0, true).is_empty(),
            "canopy cannot overwrite buildings"
        );
        world.set_block(10, 65, 8, AIR);
        let changes = sapling_changes(&world, 8, 64, 8, 0, true);
        assert!(changes.contains(&((8, 64, 8), ssc_world::OAK_LOG)));
        assert!(changes.contains(&((8, 63, 8), DIRT)));
        assert!(changes
            .iter()
            .any(|(_, block)| *block == ssc_world::GROWN_OAK_LEAVES));
        for ((x, y, z), block) in changes {
            world.set_block(x, y, z, block);
        }
        assert!(!leaf_can_decay(&world, 10, 65, 8));
        world.set_block(8, 64, 8, ssc_world::OAK_SAPLING);
        world.set_block(8, 63, 8, ssc_world::SAND);
        assert_eq!(
            sapling_changes(&world, 8, 64, 8, 1, true),
            vec![((8, 64, 8), AIR)]
        );
    }

    #[test]
    fn tree_growth_waits_for_loaded_canopy_space_and_preserves_placed_leaves() {
        let mut world = World::default();
        world.set_block(15, 63, 8, DIRT);
        world.set_block(15, 64, 8, ssc_world::OAK_SAPLING_READY);
        assert!(sapling_changes(&world, 15, 64, 8, 0, true).is_empty());
        world.column_mut(1, 0);
        world.set_block(16, 65, 8, OAK_LEAVES);
        let changes = sapling_changes(&world, 15, 64, 8, 0, true);
        assert!(!changes.is_empty());
        assert!(!changes.iter().any(|(pos, _)| *pos == (16, 65, 8)));
    }

    #[test]
    fn wheat_advances_one_age_only_with_support_light_and_a_growth_roll() {
        let mut world = World::default();
        world.set_block(8, 63, 8, ssc_world::FARMLAND);
        world.set_block(8, 64, 8, ssc_world::WHEAT_0);
        assert_eq!(
            wheat_changes(&world, 8, 64, 8, 0, true, false),
            vec![((8, 64, 8), ssc_world::WHEAT_1)]
        );
        assert!(wheat_changes(&world, 8, 64, 8, 0, false, true).is_empty());
        assert!(wheat_changes(&world, 8, 64, 8, 1, true, false).is_empty());
        assert_eq!(
            wheat_changes(&world, 8, 64, 8, 2, true, true),
            vec![((8, 64, 8), ssc_world::WHEAT_1)],
            "wet farmland has a higher chance"
        );
        world.set_block(8, 64, 8, ssc_world::WHEAT_7);
        assert!(wheat_changes(&world, 8, 64, 8, 0, true, true).is_empty());
        world.set_block(8, 64, 8, ssc_world::WHEAT_3);
        world.set_block(8, 63, 8, ssc_world::DIRT);
        assert_eq!(
            wheat_changes(&world, 8, 64, 8, 0, true, true),
            vec![((8, 64, 8), ssc_world::AIR)]
        );
    }

    #[test]
    fn farmland_hydration_scans_four_blocks_and_adjacent_levels() {
        let mut world = World::default();
        world.set_block(8, 63, 8, ssc_world::FARMLAND);
        assert!(!farmland_should_be_wet(&world, 8, 63, 8));
        world.set_block(12, 62, 8, WATER);
        assert!(farmland_should_be_wet(&world, 8, 63, 8));
        world.set_block(12, 62, 8, ssc_world::AIR);
        world.set_block(13, 64, 8, WATER);
        assert!(!farmland_should_be_wet(&world, 8, 63, 8), "13 is too far");
        world.set_block(12, 64, 8, WATER);
        assert!(farmland_should_be_wet(&world, 8, 63, 8));
    }

    #[test]
    fn multiple_samples_per_section_are_distinct_and_stable() {
        let picks = samples(7, 99, 0, 0, 8);
        assert_eq!(picks.len(), SAMPLES_PER_SECTION);
        assert_eq!(picks[0], sample(7, 99, 0, 0, 8));
        for (index, (pos, roll)) in picks.iter().enumerate() {
            assert_eq!(*pos, samples(7, 99, 0, 0, 8)[index].0);
            assert!((0..16).contains(&pos.0));
            assert!((64..80).contains(&pos.1));
            assert!((0..16).contains(&pos.2));
            assert!(*roll != 0);
        }
        assert!(
            picks
                .iter()
                .map(|(pos, _)| *pos)
                .collect::<HashSet<_>>()
                .len()
                > 1
        );
    }
}
