//! Mobs: server-authoritative entities with simple AI (`specs.md` §2 —
//! all logic here, clients only render what the server broadcasts).
//!
//! Current mobs: **pig** (passive, wanders, drops porkchop) and **zombie**
//! (spawns at night, chases and hits players, burns in daylight, drops rotten
//! flesh).
//! Placeholder visuals on the client (colored boxes); the entity-state
//! broadcast carries the kind.

use ssc_math::{Vec2, Vec3};
use ssc_world::World;

use crate::physics::move_entity;
use crate::raycast::raycast;

/// Mob kinds and their stats (wiki-approximated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobKind {
    Pig,
    Zombie,
}

impl MobKind {
    /// Wire value in `EntityState::kind`.
    pub fn as_u8(self) -> u8 {
        match self {
            MobKind::Pig => 1,
            MobKind::Zombie => 2,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(MobKind::Pig),
            2 => Some(MobKind::Zombie),
            _ => None,
        }
    }

    pub fn max_health(self) -> f32 {
        match self {
            MobKind::Pig => 10.0,
            MobKind::Zombie => 20.0,
        }
    }

    /// Movement speed (blocks/s).
    pub fn speed(self) -> f32 {
        match self {
            MobKind::Pig => 1.2,
            MobKind::Zombie => 2.3,
        }
    }

    /// Attack damage (zombies only; pigs are passive).
    pub fn attack_damage(self) -> f32 {
        match self {
            MobKind::Zombie => 3.0,
            MobKind::Pig => 0.0,
        }
    }

    /// Body dimensions (half-width, height) for physics + hit boxes.
    pub fn dims(self) -> (f32, f32) {
        match self {
            MobKind::Pig => (0.45, 0.9),
            MobKind::Zombie => (0.3, 1.9),
        }
    }
}

/// Deterministic loot roll with vanilla-sized count ranges. The server
/// supplies a per-entity seed; gameplay does not depend on client state.
pub fn drop_for(kind: MobKind, roll: u64) -> Option<(ssc_world::ItemId, u8)> {
    match kind {
        MobKind::Pig => Some((ssc_world::PORKCHOP, 1 + (roll % 3) as u8)),
        MobKind::Zombie => {
            let count = (roll % 3) as u8;
            (count > 0).then_some((ssc_world::ROTTEN_FLESH, count))
        }
    }
}

/// One mob.
pub struct Mob {
    pub kind: MobKind,
    pub pos: Vec3,
    pub vel: Vec3,
    pub yaw: f32,
    pub health: f32,
    /// Ticks until the next wander decision.
    pub wander_timer: u32,
    /// Wander direction (None = idle).
    pub wander_dir: Option<Vec2>,
    /// Ticks until the next allowed attack.
    pub attack_cooldown: u32,
    /// Daylight burn interval counter.
    pub burn_timer: u8,
    /// Marked for removal (death or despawn).
    pub dead: bool,
    /// Set by the physics step, read by the AI next tick.
    pub on_ground_hint: bool,
    combat_immunity: crate::survival::DamageImmunity,
}

impl Mob {
    pub fn new(kind: MobKind, pos: Vec3) -> Self {
        Self {
            kind,
            pos,
            vel: Vec3::ZERO,
            yaw: 0.0,
            health: kind.max_health(),
            wander_timer: 0,
            wander_dir: None,
            attack_cooldown: 0,
            burn_timer: 0,
            dead: false,
            on_ground_hint: false,
            combat_immunity: crate::survival::DamageImmunity::default(),
        }
    }

    /// Advance one tick: AI + physics. `players` are nearby player feet
    /// positions (for zombie chasing). Returns the **index** into
    /// `players` of a player this mob hit this tick, if any.
    pub fn tick(
        &mut self,
        world: &World,
        players: &[Vec3],
        dt: f32,
        daylight: bool,
    ) -> Option<usize> {
        if self.dead {
            return None;
        }
        self.combat_immunity.tick();
        if self.kind == MobKind::Zombie
            && daylight
            && has_open_sky(world, self.pos, self.kind.dims().1)
            && !touches_water(world, self.pos, self.kind.dims().1)
        {
            self.burn_timer += 1;
            if self.burn_timer >= 20 {
                self.burn_timer = 0;
                self.health -= 1.0;
                if self.health <= 0.0 {
                    self.dead = true;
                    return None;
                }
            }
        } else {
            self.burn_timer = 0;
        }
        if self.attack_cooldown > 0 {
            self.attack_cooldown -= 1;
        }

        let mut hit_player = None;
        let speed = self.kind.speed();
        let (hw, height) = self.kind.dims();

        // --- AI ---------------------------------------------------------
        if self.kind == MobKind::Zombie {
            // Chase the nearest player within 16 blocks.
            let nearest = players
                .iter()
                .enumerate()
                .filter(|(_, p)| (**p - self.pos).length() < 16.0)
                .min_by_key(|(_, p)| (**p - self.pos).length() as u32);
            if let Some((idx, target)) = nearest {
                let delta = *target - self.pos;
                let dist2d = Vec2::new(delta.x, delta.z).length();
                self.yaw = delta.x.atan2(delta.z);
                self.vel.x = (delta.x / dist2d.max(0.001)) * speed;
                self.vel.z = (delta.z / dist2d.max(0.001)) * speed;
                // Attack when close enough (and cooldown ready).
                if delta.length() < 1.5 && self.attack_cooldown == 0 {
                    let eye = self.pos + Vec3::new(0.0, 1.5, 0.0);
                    let target_eye = *target + Vec3::new(0.0, 1.5, 0.0);
                    let to_target = target_eye - eye;
                    let distance = to_target.length();
                    if distance > 0.001
                        && raycast(world, eye, to_target / distance, distance).is_none()
                    {
                        self.attack_cooldown = 20; // 1 hit/second
                        hit_player = Some(idx);
                    }
                }
                // Hop over 1-block obstacles when blocked and on ground.
                if self.on_ground_hint && self.is_blocked(world, hw) {
                    self.vel.y = 8.4;
                }
            }
        } else {
            // Passive wander: pick a direction or pause.
            if self.wander_timer == 0 {
                self.wander_timer = 40 + (self.pos.x as u32 * 31 + self.pos.z as u32) % 80;
                self.wander_dir = if self.wander_timer % 2 == 0 {
                    let yaw = ((self.pos.x * 7.0 + self.pos.z * 13.0).fract() * 6.283) - 3.1416;
                    Some(Vec2::new(yaw.cos(), yaw.sin()))
                } else {
                    None
                };
            }
            self.wander_timer -= 1;
            match self.wander_dir {
                Some(dir) => {
                    self.vel.x = dir.x * speed;
                    self.vel.z = dir.y * speed;
                    self.yaw = dir.x.atan2(dir.y);
                }
                None => {
                    self.vel.x = 0.0;
                    self.vel.z = 0.0;
                }
            }
        }

        // --- Physics -----------------------------------------------------
        // Mobs fall; no swimming AI yet (they bob to the surface because
        // water is non-solid and they walk on the bed — noted).
        self.vel.y -= ssc_math::GRAVITY * dt;
        if self.vel.y < -40.0 {
            self.vel.y = -40.0;
        }
        let res = move_entity(world, &mut self.pos, &mut self.vel, dt, hw, height);
        self.on_ground_hint = res.on_ground;
        hit_player
    }

    /// Whether horizontal movement is blocked by a solid block ahead.
    fn is_blocked(&self, world: &World, hw: f32) -> bool {
        let ahead = Vec3::new(
            self.pos.x + self.vel.x * 0.2 + self.vel.x.signum() * hw,
            self.pos.y + 0.5,
            self.pos.z + self.vel.z * 0.2 + self.vel.z.signum() * hw,
        );
        ssc_world::is_solid(world.block_or_air(
            ahead.x.floor() as i32,
            ahead.y.floor() as i32,
            ahead.z.floor() as i32,
        ))
    }

    /// External damage (player punch). Knockback along `from`.
    pub fn hurt(&mut self, amount: f32, from: Vec3) -> bool {
        if self.dead {
            return false;
        }
        let damage = self.combat_immunity.filter(amount);
        if damage == 0.0 {
            return false;
        }
        self.health = (self.health - damage).max(0.0);
        // Knockback away from the attacker.
        let delta = (self.pos - from).normalize_or_zero();
        self.vel.x += delta.x * 6.0;
        self.vel.z += delta.z * 6.0;
        self.vel.y = 4.0;
        if self.health <= 0.0 {
            self.dead = true;
            true
        } else {
            false
        }
    }
}

/// Water anywhere along the mob's vertical body extinguishes daylight
/// burning, even when its head is above the water surface.
fn touches_water(world: &World, pos: Vec3, height: f32) -> bool {
    let x = pos.x.floor() as i32;
    let z = pos.z.floor() as i32;
    (pos.y.floor() as i32..=(pos.y + height - 0.001).floor() as i32)
        .any(|y| world.block_or_air(x, y, z) == ssc_world::WATER)
}

/// Whether an opaque block covers the mob. Starts above its head so its
/// supporting terrain does not incorrectly count as a roof.
fn has_open_sky(world: &World, pos: Vec3, height: f32) -> bool {
    let x = pos.x.floor() as i32;
    let z = pos.z.floor() as i32;
    let start_y = (pos.y + height).ceil() as i32;
    (start_y..ssc_world::MAX_Y).all(|y| !ssc_world::is_opaque(world.block_or_air(x, y, z)))
}

/// Spawn eligibility for a mob at a world column: solid ground with
/// two dry, non-solid blocks above.
pub fn spawn_column_ok(world: &World, x: i32, z: i32) -> Option<i32> {
    for y in (ssc_world::MIN_Y + 8..ssc_world::MAX_Y - 40).rev() {
        let ground = world.block_or_air(x, y - 1, z);
        let here = world.block_or_air(x, y, z);
        let above = world.block_or_air(x, y + 1, z);
        if ssc_world::is_solid(ground)
            && !ssc_world::is_solid(here)
            && !ssc_world::is_solid(above)
            && here != ssc_world::WATER
            && above != ssc_world::WATER
        {
            return Some(y);
        }
    }
    None
}

/// Natural Bedrock pig spawns require a grass block. In particular, desert
/// sand, snow caps and player-built stone floors are not pig spawn surfaces.
pub fn pig_spawn_column_ok(world: &World, x: i32, z: i32) -> Option<i32> {
    let y = spawn_column_ok(world, x, z)?;
    (world.block_or_air(x, y - 1, z) == ssc_world::GRASS_BLOCK).then_some(y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_world() -> World {
        let mut world = World::default();
        let mut col = ssc_world::Column::default();
        let mut sc = Box::new(ssc_world::SubChunk::default());
        for z in 0..16 {
            for x in 0..16 {
                sc.set(x, 0, z, ssc_world::STONE);
            }
        }
        col.subchunks[7] = Some(sc);
        world.columns.insert((0, 0), col);
        world
    }

    #[test]
    fn zombie_chases_and_hits_players() {
        let world = flat_world();
        let mut zombie = Mob::new(MobKind::Zombie, Vec3::new(4.5, 48.02, 8.5));
        let players = [Vec3::new(2.5, 48.02, 8.5)];
        // Tick 20 times: it should close distance and land a hit.
        let mut hits = 0;
        for _ in 0..20 {
            if zombie.tick(&world, &players, 0.05, false).is_some() {
                hits += 1;
            }
        }
        let dist = (players[0] - zombie.pos).length();
        assert!(dist < 3.0, "zombie should approach, dist {dist}");
        assert!(hits >= 1, "zombie should land at least one hit");
    }

    #[test]
    fn pig_wanders_but_never_attacks() {
        let world = flat_world();
        let mut pig = Mob::new(MobKind::Pig, Vec3::new(8.5, 48.02, 8.5));
        let players = [Vec3::new(8.5, 48.02, 8.5)];
        for _ in 0..200 {
            assert!(pig.tick(&world, &players, 0.05, false).is_none());
        }
    }

    #[test]
    fn zombie_cannot_melee_through_a_wall() {
        let mut world = flat_world();
        world.set_block(9, 49, 8, ssc_world::STONE);
        world.set_block(9, 50, 8, ssc_world::STONE);
        let mut zombie = Mob::new(MobKind::Zombie, Vec3::new(8.69, 49.02, 8.5));
        let players = [Vec3::new(10.05, 49.02, 8.5)];

        assert_eq!(zombie.tick(&world, &players, 0.05, false), None);
        assert_eq!(zombie.attack_cooldown, 0);

        world.set_block(9, 49, 8, ssc_world::AIR);
        world.set_block(9, 50, 8, ssc_world::AIR);
        assert_eq!(zombie.tick(&world, &players, 0.05, false), Some(0));
    }

    #[test]
    fn hurt_deals_knockback_and_death() {
        let mut pig = Mob::new(MobKind::Pig, Vec3::new(8.5, 48.02, 8.5));
        let died = pig.hurt(6.0, Vec3::new(8.5, 48.0, 6.5));
        assert!(!died);
        assert!(pig.health < 4.1);
        assert!(pig.vel.y > 0.0, "knockback should pop the mob up");
        assert!(pig.hurt(10.0, Vec3::new(8.5, 48.0, 6.5)));
        assert!(pig.dead);
    }

    #[test]
    fn immune_mob_hits_do_not_repeat_damage_or_knockback() {
        let mut mob = Mob::new(MobKind::Zombie, Vec3::new(8.5, 49.02, 8.5));
        let from = Vec3::new(6.5, 49.02, 8.5);
        mob.hurt(3.0, from);
        let velocity = mob.vel;
        mob.hurt(3.0, from);
        assert_eq!(mob.health, 17.0);
        assert_eq!(mob.vel, velocity);
        mob.hurt(5.0, from);
        assert_eq!(mob.health, 15.0);
        let world = flat_world();
        for _ in 0..10 {
            mob.tick(&world, &[], 0.05, false);
        }
        mob.hurt(3.0, from);
        assert_eq!(mob.health, 12.0);
    }

    #[test]
    fn spawn_column_finds_surface() {
        let world = flat_world();
        // Ground cells at y 48; the standing surface is the top face = 49.
        assert_eq!(spawn_column_ok(&world, 8, 8), Some(49));
    }

    #[test]
    fn pigs_need_grass_and_two_dry_blocks_of_clearance() {
        let mut world = flat_world();
        assert_eq!(spawn_column_ok(&world, 8, 8), Some(49));
        assert_eq!(pig_spawn_column_ok(&world, 8, 8), None);

        world.set_block(8, 48, 8, ssc_world::GRASS_BLOCK);
        assert_eq!(pig_spawn_column_ok(&world, 8, 8), Some(49));

        world.set_block(8, 50, 8, ssc_world::WATER);
        assert_eq!(pig_spawn_column_ok(&world, 8, 8), None);
        assert_eq!(spawn_column_ok(&world, 8, 8), None);
        world.set_block(8, 50, 8, ssc_world::AIR);
        world.set_block(8, 49, 8, ssc_world::WATER);
        assert_eq!(pig_spawn_column_ok(&world, 8, 8), None);
        assert_eq!(spawn_column_ok(&world, 8, 8), None);
        world.set_block(8, 49, 8, ssc_world::STONE);
        assert_eq!(pig_spawn_column_ok(&world, 8, 8), None);

        world.set_block(8, 49, 8, ssc_world::AIR);
        world.set_block(8, 48, 8, ssc_world::SAND);
        assert_eq!(spawn_column_ok(&world, 8, 8), Some(49));
        assert_eq!(pig_spawn_column_ok(&world, 8, 8), None);
    }

    #[test]
    fn mob_loot_counts_match_documented_ranges() {
        for roll in 0..12 {
            let (item, count) = drop_for(MobKind::Pig, roll).unwrap();
            assert_eq!(item, ssc_world::PORKCHOP);
            assert!((1..=3).contains(&count));
            if let Some((item, count)) = drop_for(MobKind::Zombie, roll) {
                assert_eq!(item, ssc_world::ROTTEN_FLESH);
                assert!((1..=2).contains(&count));
            }
        }
        assert_eq!(drop_for(MobKind::Zombie, 0), None);
    }

    #[test]
    fn zombie_burns_in_daylight_but_not_under_a_roof() {
        let mut exposed_world = flat_world();
        let mut exposed = Mob::new(MobKind::Zombie, Vec3::new(8.5, 49.0, 8.5));
        for _ in 0..20 {
            exposed.tick(&exposed_world, &[], 0.05, true);
        }
        assert_eq!(exposed.health, 19.0);

        exposed_world.set_block(8, 54, 8, ssc_world::STONE);
        let mut sheltered = Mob::new(MobKind::Zombie, Vec3::new(8.5, 49.0, 8.5));
        for _ in 0..40 {
            sheltered.tick(&exposed_world, &[], 0.05, true);
        }
        assert_eq!(sheltered.health, 20.0);
    }

    #[test]
    fn water_extinguishes_daylight_burning_and_resets_timer() {
        let mut world = flat_world();
        let mut zombie = Mob::new(MobKind::Zombie, Vec3::new(8.5, 49.02, 8.5));
        for _ in 0..10 {
            zombie.tick(&world, &[], 0.05, true);
        }
        assert_eq!(zombie.burn_timer, 10);
        world.set_block(8, 49, 8, ssc_world::WATER);
        for _ in 0..40 {
            zombie.tick(&world, &[], 0.05, true);
        }
        assert_eq!(zombie.health, 20.0);
        assert_eq!(zombie.burn_timer, 0);
        world.set_block(8, 49, 8, ssc_world::AIR);
        for _ in 0..20 {
            zombie.tick(&world, &[], 0.05, true);
        }
        assert_eq!(
            zombie.health, 19.0,
            "sunlight burns again after leaving water"
        );
    }
}
