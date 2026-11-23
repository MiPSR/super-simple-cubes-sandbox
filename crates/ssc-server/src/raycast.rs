//! Voxel raycast (Amanatides & Woo DDA) for block picking.
//!
//! Runs on the server (`specs.md` §2): the client only asks "dig" or
//! "place"; the server decides what block that means.

use ssc_math::Vec3;
use ssc_world::{is_solid, World};

/// A block the ray hit, and which face.
#[derive(Debug, Clone, Copy)]
pub struct RayHit {
    pub block: [i32; 3],
    /// Face normal pointing out of the hit block (the neighbor cell where
    /// a placed block would go).
    pub normal: [i32; 3],
}

/// March from `origin` along normalized `dir`, up to `max_dist` blocks.
/// Skips the starting cell; includes non-solid, selectable saplings.
pub fn raycast(world: &World, origin: Vec3, dir: Vec3, max_dist: f32) -> Option<RayHit> {
    raycast_matching(world, origin, dir, max_dist, |block| {
        is_solid(block)
            || ssc_world::is_oak_sapling(block)
            || ssc_world::is_wheat(block)
            || ssc_world::is_door(block)
            || block == ssc_world::LADDER
    })
}

/// Combat occlusion ignores plants that have no collision.
pub fn raycast_solids(world: &World, origin: Vec3, dir: Vec3, max_dist: f32) -> Option<RayHit> {
    raycast_matching(world, origin, dir, max_dist, is_solid)
}

/// Empty buckets select source water, but cannot reach through solid blocks.
pub fn raycast_water(world: &World, origin: Vec3, dir: Vec3, max_dist: f32) -> Option<RayHit> {
    let block = [
        origin.x.floor() as i32,
        origin.y.floor() as i32,
        origin.z.floor() as i32,
    ];
    if world.block_or_air(block[0], block[1], block[2]) == ssc_world::WATER {
        return Some(RayHit {
            block,
            normal: [0, 0, 0],
        });
    }
    raycast_matching(world, origin, dir, max_dist, |block| {
        is_solid(block) || block == ssc_world::WATER
    })
}

fn raycast_matching(
    world: &World,
    origin: Vec3,
    dir: Vec3,
    max_dist: f32,
    accepts: impl Fn(ssc_world::BlockId) -> bool,
) -> Option<RayHit> {
    let mut x = origin.x.floor() as i32;
    let mut y = origin.y.floor() as i32;
    let mut z = origin.z.floor() as i32;

    let step_x = if dir.x > 0.0 { 1 } else { -1 };
    let step_y = if dir.y > 0.0 { 1 } else { -1 };
    let step_z = if dir.z > 0.0 { 1 } else { -1 };

    let t_delta_x = if dir.x != 0.0 {
        (1.0 / dir.x).abs()
    } else {
        f32::INFINITY
    };
    let t_delta_y = if dir.y != 0.0 {
        (1.0 / dir.y).abs()
    } else {
        f32::INFINITY
    };
    let t_delta_z = if dir.z != 0.0 {
        (1.0 / dir.z).abs()
    } else {
        f32::INFINITY
    };

    // Distance until the ray crosses the next cell boundary per axis.
    let boundary_x = (if dir.x > 0.0 { x + 1 } else { x }) as f32;
    let boundary_y = (if dir.y > 0.0 { y + 1 } else { y }) as f32;
    let boundary_z = (if dir.z > 0.0 { z + 1 } else { z }) as f32;
    let mut t_max_x = if dir.x != 0.0 {
        ((boundary_x - origin.x) / dir.x).abs()
    } else {
        f32::INFINITY
    };
    let mut t_max_y = if dir.y != 0.0 {
        ((boundary_y - origin.y) / dir.y).abs()
    } else {
        f32::INFINITY
    };
    let mut t_max_z = if dir.z != 0.0 {
        ((boundary_z - origin.z) / dir.z).abs()
    } else {
        f32::INFINITY
    };

    let mut normal;
    let mut t;

    loop {
        // Advance to the next cell (skipping the starting cell).
        if t_max_x < t_max_y && t_max_x < t_max_z {
            x += step_x;
            t = t_max_x;
            t_max_x += t_delta_x;
            normal = [-step_x, 0, 0];
        } else if t_max_y < t_max_z {
            y += step_y;
            t = t_max_y;
            t_max_y += t_delta_y;
            normal = [0, -step_y, 0];
        } else {
            z += step_z;
            t = t_max_z;
            t_max_z += t_delta_z;
            normal = [0, 0, -step_z];
        }

        if t > max_dist {
            return None;
        }

        if accepts(world.block_or_air(x, y, z)) {
            return Some(RayHit {
                block: [x, y, z],
                normal,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_rays_include_water_containing_the_eye_and_stop_at_solids() {
        let mut world = World::default();
        let origin = Vec3::new(0.5, 64.5, 0.5);
        world.set_block(0, 64, 0, ssc_world::WATER);
        assert_eq!(
            raycast_water(&world, origin, Vec3::X, 4.5).unwrap().block,
            [0, 64, 0]
        );
        world.set_block(0, 64, 0, ssc_world::AIR);
        world.set_block(1, 64, 0, ssc_world::GLASS);
        world.set_block(2, 64, 0, ssc_world::WATER);
        assert_eq!(
            raycast_water(&world, origin, Vec3::X, 4.5).unwrap().block,
            [1, 64, 0]
        );
    }

    #[test]
    fn saplings_are_selectable_but_do_not_block_combat_rays() {
        let mut world = World::default();
        world.set_block(1, 64, 0, ssc_world::OAK_SAPLING);
        world.set_block(2, 64, 0, ssc_world::STONE);
        let origin = Vec3::new(0.5, 64.5, 0.5);
        assert_eq!(
            raycast(&world, origin, Vec3::X, 4.5).unwrap().block,
            [1, 64, 0]
        );
        assert_eq!(
            raycast_solids(&world, origin, Vec3::X, 4.5).unwrap().block,
            [2, 64, 0]
        );
        assert!(!ssc_world::is_solid(ssc_world::OAK_SAPLING));
    }
}
