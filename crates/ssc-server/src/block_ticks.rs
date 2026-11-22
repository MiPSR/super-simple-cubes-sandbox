//! Random-ticked grass rules. Lighting is a direct-skylight approximation
//! until the server has propagated block/sky light.

use ssc_world::{BlockId, World, DIRT, GRASS_BLOCK, MAX_Y, MIN_Y, OAK_LEAVES, WATER};

pub type BlockChange = ((i32, i32, i32), BlockId);

fn mix(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// One deterministic sample per allocated section per tick. This is a
/// simplified scheduler, not Bedrock's exact relative randomTickSpeed.
pub fn sample(tick: u64, seed: u64, cx: i32, cz: i32, section: usize) -> ((i32, i32, i32), u64) {
    let roll = mix(tick.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ seed
        ^ (cx as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        ^ (cz as u64).wrapping_mul(0x1656_67b1_9e37_79f9)
        ^ (section as u64).wrapping_mul(0xd1b5_4a32_d192_ed05));
    (
        (
            cx * 16 + (roll & 15) as i32,
            MIN_Y + section as i32 * 16 + ((roll >> 4) & 15) as i32,
            cz * 16 + ((roll >> 8) & 15) as i32,
        ),
        mix(roll ^ 0xb10c_71c5),
    )
}

fn blocks_light(block: BlockId) -> bool {
    ssc_world::is_opaque(block) && block != OAK_LEAVES
}

fn skylight(world: &World, x: i32, y: i32, z: i32, daylight: bool) -> u8 {
    let mut light: u8 = if daylight { 15 } else { 4 };
    for above in y + 1..MAX_Y {
        let block = world.block_or_air(x, above, z);
        if blocks_light(block) {
            return 0;
        }
        if matches!(block, WATER | OAK_LEAVES) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_world::AIR;

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
}
