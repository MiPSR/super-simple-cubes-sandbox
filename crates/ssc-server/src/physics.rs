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
    let mut on_ground = false;

    // Water check for the result (feet or head submerged).
    let in_water = world.block_or_air(
        pos.x.floor() as i32,
        (pos.y + 0.4).floor() as i32,
        pos.z.floor() as i32,
    ) == WATER
        || world.block_or_air(
            (pos.x).floor() as i32,
            (pos.y + height * 0.9).floor() as i32,
            pos.z.floor() as i32,
        ) == WATER;

    // Sub-step so no axis moves more than 0.4 blocks per step (no tunneling).
    let disp = *vel * dt;
    let steps = ((disp.length() / 0.4).ceil() as usize).clamp(1, 64);
    let step_dt = dt / steps as f32;

    for _ in 0..steps {
        // Y first (so landing is exact), then X, then Z.
        if sweep_axis(world, pos, vel.y * step_dt, Axis::Y, &mut on_ground, hw, height) {
            vel.y = 0.0;
        }
        if sweep_axis(world, pos, vel.x * step_dt, Axis::X, &mut on_ground, hw, height) {
            vel.x = 0.0;
        }
        if sweep_axis(world, pos, vel.z * step_dt, Axis::Z, &mut on_ground, hw, height) {
            vel.z = 0.0;
        }
    }

    // Void safety: never fall out of the bottom of the world.
    if pos.y < MIN_Y as f32 {
        pos.y = MIN_Y as f32;
        vel.y = 0.0;
        on_ground = true;
    }

    MoveResult { on_ground, in_water }
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
