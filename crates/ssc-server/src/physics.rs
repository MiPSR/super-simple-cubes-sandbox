//! Player physics — server-authoritative (`specs.md` §2).
//!
//! Minecraft-like values (see the constants in `ssc-math`): 0.6×1.8 AABB,
//! gravity 32 b/s², ~1.27-block jump apex, walk/sprint/sneak speeds.
//! Collision is a per-axis swept AABB against solid blocks with sub-stepping
//! so fast falls never tunnel.

use ssc_math::{Aabb, Vec3};
use ssc_world::{is_solid, World, MIN_Y, WATER};

/// Small collision epsilon to avoid floating-point re-penetration.
const EPS: f32 = 1e-4;

/// Result of one physics step.
#[derive(Debug, Clone, Copy, Default)]
pub struct MoveResult {
    pub on_ground: bool,
    /// Feet or eyes in water (swimming physics active).
    pub in_water: bool,
}

/// Player AABB for a feet-center position.
pub fn player_aabb(pos: Vec3) -> Aabb {
    let hw = ssc_math::PLAYER_HALF_WIDTH;
    Aabb::new(
        Vec3::new(pos.x - hw, pos.y, pos.z - hw),
        Vec3::new(pos.x + hw, pos.y + ssc_math::PLAYER_HEIGHT, pos.z + hw),
    )
}

/// AABB of a generic entity (feet-center, half-width, height).
pub fn entity_aabb(pos: Vec3, hw: f32, height: f32) -> Aabb {
    Aabb::new(
        Vec3::new(pos.x - hw, pos.y, pos.z - hw),
        Vec3::new(pos.x + hw, pos.y + height, pos.z + hw),
    )
}

/// Integrate one tick of player movement: applies `vel * dt` to `pos` with
/// block collision. `vel` is zeroed on the axes that get blocked.
pub fn move_player(world: &World, pos: &mut Vec3, vel: &mut Vec3, dt: f32) -> MoveResult {
    move_entity(
        world,
        pos,
        vel,
        dt,
        ssc_math::PLAYER_HALF_WIDTH,
        ssc_math::PLAYER_HEIGHT,
    )
}

/// Grounded sneaking keeps part of the player's footprint over support.
/// Jumping, flying and swimming callers use normal movement instead.
pub fn move_sneaking_player(world: &World, pos: &mut Vec3, vel: &mut Vec3, dt: f32) -> MoveResult {
    move_entity_inner(
        world,
        pos,
        vel,
        dt,
        ssc_math::PLAYER_HALF_WIDTH,
        ssc_math::PLAYER_HEIGHT,
        true,
    )
}

/// Generic entity movement with per-axis swept AABB collision and
/// sub-stepping (players, mobs).
pub fn move_entity(
    world: &World,
    pos: &mut Vec3,
    vel: &mut Vec3,
    dt: f32,
    hw: f32,
    height: f32,
) -> MoveResult {
    move_entity_inner(world, pos, vel, dt, hw, height, false)
}

fn move_entity_inner(
    world: &World,
    pos: &mut Vec3,
    vel: &mut Vec3,
    dt: f32,
    hw: f32,
    height: f32,
    sneak: bool,
) -> MoveResult {
    let mut on_ground = false;

    // Water is a trigger, not a collision surface. Sample every movement
    // sub-step so a fast fall cannot tunnel through it between ticks.
    let mut in_water = entity_in_water(world, *pos, height);

    // Sub-step so no axis moves more than 0.4 blocks per step (no tunneling).
    let disp = *vel * dt;
    let steps = ((disp.length() / 0.4).ceil() as usize).clamp(1, 64);
    let step_dt = dt / steps as f32;

    for _ in 0..steps {
        // Y first (so landing is exact), then X, then Z.
        if sweep_axis(
            world,
            pos,
            vel.y * step_dt,
            Axis::Y,
            &mut on_ground,
            hw,
            height,
        ) {
            vel.y = 0.0;
        }
        let (mut dx, mut dz) = (vel.x * step_dt, vel.z * step_dt);
        if sneak && vel.y <= 0.0 && has_foot_support(world, *pos, hw) {
            (dx, dz) = trim_sneak_motion(world, *pos, dx, dz, hw);
        }
        if sweep_axis(world, pos, dx, Axis::X, &mut on_ground, hw, height) {
            vel.x = 0.0;
        }
        if sweep_axis(world, pos, dz, Axis::Z, &mut on_ground, hw, height) {
            vel.z = 0.0;
        }
        in_water |= entity_in_water(world, *pos, height);
    }

    // Bedrock has an invisible one-way floor at about Y=-105 in the
    // Overworld. The build limit (-64) is not a floor: survival players must
    // be able to enter the void and take damage.
    if pos.y < (MIN_Y - 41) as f32 {
        pos.y = (MIN_Y - 41) as f32;
        vel.y = 0.0;
        on_ground = true;
    }

    MoveResult {
        on_ground,
        in_water,
    }
}

fn has_foot_support(world: &World, pos: Vec3, hw: f32) -> bool {
    // Only full cubes exist so far. Probe immediately below the feet, not
    // a whole block below (that would allow sneaking down one-block drops).
    let y = (pos.y - 0.05).floor() as i32;
    if (pos.y - (y + 1) as f32).abs() > 0.05 {
        return false;
    }
    let x0 = (pos.x - hw + EPS).floor() as i32;
    let x1 = (pos.x + hw - EPS).floor() as i32;
    let z0 = (pos.z - hw + EPS).floor() as i32;
    let z1 = (pos.z + hw - EPS).floor() as i32;
    (x0..=x1).any(|x| (z0..=z1).any(|z| is_solid(world.block_or_air(x, y, z))))
}

fn trim_sneak_motion(world: &World, pos: Vec3, mut dx: f32, mut dz: f32, hw: f32) -> (f32, f32) {
    let reduce = |d: f32| d.signum() * (d.abs() - 0.05).max(0.0);
    while dx != 0.0 && !has_foot_support(world, pos + Vec3::new(dx, 0.0, 0.0), hw) {
        dx = reduce(dx);
    }
    while dz != 0.0 && !has_foot_support(world, pos + Vec3::new(0.0, 0.0, dz), hw) {
        dz = reduce(dz);
    }
    // Both individual axes can be safe while their diagonal goes beyond
    // a corner. Check the combined destination as well.
    while (dx != 0.0 || dz != 0.0) && !has_foot_support(world, pos + Vec3::new(dx, 0.0, dz), hw) {
        dx = reduce(dx);
        dz = reduce(dz);
    }
    (dx, dz)
}

fn entity_in_water(world: &World, pos: Vec3, height: f32) -> bool {
    world.block_or_air(
        pos.x.floor() as i32,
        (pos.y + 0.4).floor() as i32,
        pos.z.floor() as i32,
    ) == WATER
        || world.block_or_air(
            pos.x.floor() as i32,
            (pos.y + height * 0.9).floor() as i32,
            pos.z.floor() as i32,
        ) == WATER
}

#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    fn get(self, v: Vec3) -> f32 {
        match self {
            Axis::X => v.x,
            Axis::Y => v.y,
            Axis::Z => v.z,
        }
    }

    fn set(self, v: &mut Vec3, value: f32) {
        match self {
            Axis::X => v.x = value,
            Axis::Y => v.y = value,
            Axis::Z => v.z = value,
        }
    }
}

/// Move an entity along one axis and clamp against the first solid block
/// hit. Returns whether the axis got blocked (velocity should be zeroed by
/// the caller). Sets `on_ground` when a downward Y move gets clamped.
fn sweep_axis(
    world: &World,
    pos: &mut Vec3,
    d: f32,
    axis: Axis,
    on_ground: &mut bool,
    hw: f32,
    height: f32,
) -> bool {
    if d == 0.0 {
        return false;
    }

    axis.set(pos, axis.get(*pos) + d);
    let aabb = entity_aabb(*pos, hw, height);

    // Cells overlapping the (moved) player AABB.
    let x0 = aabb.min.x.floor() as i32;
    let x1 = (aabb.max.x - EPS).floor() as i32;
    let y0 = aabb.min.y.floor() as i32;
    let y1 = (aabb.max.y - EPS).floor() as i32;
    let z0 = aabb.min.z.floor() as i32;
    let z1 = (aabb.max.z - EPS).floor() as i32;

    let mut blocked = false;
    for bx in x0..=x1 {
        for by in y0..=y1 {
            for bz in z0..=z1 {
                if !is_solid(world.block_or_air(bx, by, bz)) {
                    continue;
                }
                blocked = true;
                // Clamp this axis to the block face we penetrated.
                match axis {
                    Axis::X => {
                        if d > 0.0 {
                            pos.x = bx as f32 - hw - EPS;
                        } else {
                            pos.x = bx as f32 + 1.0 + hw + EPS;
                        }
                    }
                    Axis::Y => {
                        if d > 0.0 {
                            pos.y = by as f32 - height - EPS;
                        } else {
                            pos.y = by as f32 + 1.0 + EPS;
                            *on_ground = true;
                        }
                    }
                    Axis::Z => {
                        if d > 0.0 {
                            pos.z = bz as f32 - hw - EPS;
                        } else {
                            pos.z = bz as f32 + 1.0 + hw + EPS;
                        }
                    }
                }
                return blocked; // clamped; other overlaps on this axis vanish
            }
        }
    }
    blocked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sneaking_stops_at_edges_and_corners_including_negative_coordinates() {
        for (x, z) in [(0, 0), (-1, -1), (15, 15)] {
            let mut world = World::default();
            world.set_block(x, 54, z, ssc_world::STONE);
            for (vx, vz) in [(1.31, 0.0), (-1.31, 0.0), (0.0, 1.31), (1.0, 1.0)] {
                let mut pos = Vec3::new(x as f32 + 0.5, 55.0001, z as f32 + 0.5);
                for _ in 0..100 {
                    let mut vel = Vec3::new(vx, -1.6, vz);
                    let result = move_sneaking_player(&world, &mut pos, &mut vel, 0.05);
                    assert!(result.on_ground);
                    assert!(has_foot_support(&world, pos, ssc_math::PLAYER_HALF_WIDTH));
                    assert!((pos.y - 55.0001).abs() < 0.001);
                }
                assert!((pos.x - (x as f32 + 0.5)).abs() < 0.8);
                assert!((pos.z - (z as f32 + 0.5)).abs() < 0.8);
            }
        }
    }

    #[test]
    fn sneaking_does_not_suspend_players_when_support_is_removed() {
        let mut world = World::default();
        world.set_block(0, 54, 0, ssc_world::STONE);
        let mut pos = Vec3::new(0.5, 55.0001, 0.5);
        world.set_block(0, 54, 0, ssc_world::AIR);
        let mut vel = Vec3::new(1.31, -1.6, 0.0);
        let result = move_sneaking_player(&world, &mut pos, &mut vel, 0.05);
        assert!(!result.on_ground);
        assert!(pos.y < 55.0);
        assert!(pos.x > 0.5);
    }

    #[test]
    fn sneaking_crosses_flat_bridges_but_not_one_block_drops() {
        let mut world = World::default();
        for x in 0..6 {
            world.set_block(x, 54, 0, ssc_world::STONE);
        }
        world.set_block(6, 53, 0, ssc_world::STONE);
        let mut pos = Vec3::new(0.5, 55.0001, 0.5);
        for _ in 0..150 {
            let mut vel = Vec3::new(1.31, -1.6, 0.0);
            move_sneaking_player(&world, &mut pos, &mut vel, 0.05);
        }
        assert!(pos.x > 5.5 && pos.x < 6.3);
        assert!(pos.y > 55.0);
    }

    #[test]
    fn fast_fall_detects_water_crossed_between_ticks() {
        let mut world = World::default();
        world.set_block(0, 51, 0, WATER);
        let mut pos = Vec3::new(0.5, 52.1, 0.5);
        let mut vel = Vec3::new(0.0, -30.0, 0.0);

        let result = move_player(&world, &mut pos, &mut vel, 0.05);

        assert!(result.in_water, "the swept path crossed a water block");
        assert!(pos.y < 51.0, "the test should pass through the water cell");
    }

    #[test]
    fn void_floor_is_below_build_limit() {
        let world = World::default();
        let mut pos = Vec3::new(0.5, MIN_Y as f32 - 0.1, 0.5);
        let mut vel = Vec3::new(0.0, -10.0, 0.0);
        move_player(&world, &mut pos, &mut vel, 0.05);
        assert!(pos.y < MIN_Y as f32);

        pos.y = (MIN_Y - 41) as f32 + 0.1;
        vel.y = -10.0;
        let result = move_player(&world, &mut pos, &mut vel, 0.05);
        assert_eq!(pos.y, (MIN_Y - 41) as f32);
        assert!(result.on_ground);
        assert_eq!(vel.y, 0.0);
    }
}
