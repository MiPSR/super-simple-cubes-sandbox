//! Survival-mode systems: inventory, vitals (health/hunger/air), fall
//! damage, and timed digging. Server-authoritative (`specs.md` §2).
//!
//! Pure state + logic functions, unit-tested; `game.rs` owns the wire
//! side (broadcasts, sounds, packet handling).

use std::time::{Duration, Instant};

use ssc_world::{BlockId, ItemId, block_info};

/// Inventory size: 9 hotbar slots + 27 storage.
pub const INVENTORY_SIZE: usize = 36;
/// Max health (Minecraft: 20 = 10 hearts).
pub const MAX_HEALTH: f32 = 20.0;
/// Max hunger (Minecraft: 20 = 10 drumsticks).
pub const MAX_HUNGER: u8 = 20;
/// Max air, in ticks (Minecraft: 300 = 15 s of breath).
pub const MAX_AIR: u16 = 300;
/// Seconds before auto-respawn (a death screen would be nicer).
pub const RESPAWN_DELAY: Duration = Duration::from_secs(2);

/// One inventory slot: (item, count). `None` = empty.
pub type Slot = Option<(ItemId, u8)>;

/// Survival-mode per-player state.
pub struct SurvivalState {
    pub health: f32,
    pub hunger: u8,
    pub air: u16,
    /// Accumulated fall distance (blocks) since last ground/water.
    pub fall_distance: f32,
    pub dead: bool,
    pub respawn_at: Option<Instant>,
    pub digging: Option<DigState>,
    // Timers, in ticks.
    regen_timer: u32,
    starve_timer: u32,
    drown_timer: u32,
    void_timer: u8,
    /// Remaining ticks for Hunger I (rotten flesh).
    pub hunger_effect_ticks: u16,
    /// Hunger exhaustion (4.0 = one hunger point).
    pub exhaustion: f32,
}

/// A dig in progress.
pub struct DigState {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// Break progress in `0.0..1.0`.
    pub progress: f32,
}

impl Default for SurvivalState {
    fn default() -> Self {
        Self {
            health: MAX_HEALTH,
            hunger: MAX_HUNGER,
            air: MAX_AIR,
            fall_distance: 0.0,
            dead: false,
            respawn_at: None,
            digging: None,
            regen_timer: 0,
            starve_timer: 0,
            drown_timer: 0,
            void_timer: 0,
            hunger_effect_ticks: 0,
            exhaustion: 0.0,
        }
    }
}

/// Inputs for one survival tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct TickInput {
    /// Landed this tick (transition falling → on ground).
    pub landed: bool,
    /// Feet or eyes in water (cancels fall damage, enables swim physics).
    pub in_water: bool,
    /// Eyes submerged (drowning).
    pub eye_in_water: bool,
    /// Player feet crossed below the Overworld's minimum build height.
    pub below_world: bool,
    /// Moving horizontally this tick.
    pub moving: bool,
    pub sprinting: bool,
    /// Jumped this tick.
    pub jumped: bool,
    /// Vertical movement this tick (negative = falling).
    pub dy: f32,
}

/// What happened during the tick — `game.rs` turns these into packets.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TickEffects {
    /// Damage applied (0 = none). Includes its cause for logging.
    pub damage: f32,
    pub hurt: bool,
    pub health_changed: bool,
    pub hunger_changed: bool,
    pub effect_changed: bool,
    pub air_changed: bool,
    pub died: bool,
}

impl SurvivalState {
    /// Advance vitals by one tick. Returns the effects.
    pub fn tick(&mut self, input: TickInput) -> TickEffects {
        let mut fx = TickEffects::default();
        if self.dead {
            return fx;
        }

        // --- Fall damage -------------------------------------------------
        if input.in_water || input.below_world {
            self.fall_distance = 0.0;
        } else if input.dy < 0.0 {
            self.fall_distance += -input.dy;
        }
        if input.landed {
            // Minecraft: damage = fall distance − 3, floored.
            let dmg = (self.fall_distance - 3.0).floor();
            if dmg > 0.0 {
                fx.damage += dmg;
            }
            self.fall_distance = 0.0;
        }

        // --- Hunger (exhaustion model, simplified) ------------------------
        // Moving/sprinting/jumping build exhaustion; 4 exhaustion = 1
        // hunger point. Regen costs exhaustion too.
        if input.moving {
            self.exhaustion += if input.sprinting { 0.025 } else { 0.005 };
        }
        if input.jumped {
            self.exhaustion += 0.05;
        }
        if self.hunger_effect_ticks > 0 {
            self.hunger_effect_ticks -= 1;
            // Hunger I adds 0.005 exhaustion per tick.
            self.exhaustion += 0.005;
            // UI needs only one status refresh per displayed second, plus
            // the final packet that clears the effect.
            fx.effect_changed = self.hunger_effect_ticks % 20 == 0;
        }
        while self.exhaustion >= 4.0 {
            self.exhaustion -= 4.0;
            if self.hunger > 0 {
                self.hunger -= 1;
                fx.hunger_changed = true;
            }
        }

        // --- Regeneration (hunger ≥ 18) -----------------------------------
        if self.hunger >= 18 && self.health < MAX_HEALTH {
            self.regen_timer += 1;
            if self.regen_timer >= 80 {
                // 4 s per heart.
                self.regen_timer = 0;
                self.health = (self.health + 1.0).min(MAX_HEALTH);
                self.exhaustion += 6.0;
                fx.health_changed = true;
            }
        } else {
            self.regen_timer = 0;
        }

        // --- Starvation (normal difficulty: stops at half a heart) --------
        if self.hunger == 0 {
            self.starve_timer += 1;
            if self.starve_timer >= 80 {
                self.starve_timer = 0;
                if self.health > 1.0 {
                    fx.damage += 1.0;
                }
            }
        } else {
            self.starve_timer = 0;
        }

        // --- Air / drowning ------------------------------------------------
        if input.eye_in_water {
            let before = self.air;
            self.air = self.air.saturating_sub(1);
            if self.air != before {
                fx.air_changed = true;
            }
            if self.air == 0 {
                self.drown_timer += 1;
                if self.drown_timer >= 20 {
                    // 1 heart per second once out of air.
                    self.drown_timer = 0;
                    fx.damage += 2.0;
                }
            }
        } else if self.air != MAX_AIR {
            self.air = (self.air + 4).min(MAX_AIR);
            self.drown_timer = 0;
            fx.air_changed = true;
        }

        // Bedrock: below Y=-64, 4 health damage every 10 game ticks.
        // This continues even after landing on the invisible void floor.
        if input.below_world {
            self.void_timer += 1;
            if self.void_timer >= 10 {
                self.void_timer = 0;
                fx.damage += 4.0;
            }
        } else {
            self.void_timer = 0;
        }

        // --- Apply damage ---------------------------------------------------
        if fx.damage > 0.0 {
            self.apply_damage(fx.damage, &mut fx);
        }
        fx
    }

    /// External damage (fall handled above; future: mobs, combat).
    pub fn apply_damage(&mut self, amount: f32, fx: &mut TickEffects) {
        if self.dead || amount <= 0.0 {
            return;
        }
        self.health = (self.health - amount).max(0.0);
        fx.hurt = true;
        fx.health_changed = true;
        if self.health <= 0.0 {
            self.die();
            fx.died = true;
        }
    }

    pub fn die(&mut self) {
        self.dead = true;
        self.health = 0.0;
        self.digging = None;
        self.respawn_at = Some(Instant::now() + RESPAWN_DELAY);
    }

    /// Whether the respawn timer elapsed.
    pub fn should_respawn(&self) -> bool {
        self.dead
            && self
                .respawn_at
                .is_some_and(|at| Instant::now() >= at)
    }

    /// Reset vitals after respawn. The server owns the separate inventory and
    /// dropped-item state, so resetting this structure leaves both untouched.
    pub fn respawn(&mut self) {
        *self = SurvivalState::default();
    }

    /// Eat `points` of hunger.
    pub fn eat(&mut self, points: u8) -> bool {
        if self.hunger >= MAX_HUNGER {
            return false;
        }
        self.hunger = (self.hunger + points).min(MAX_HUNGER);
        true
    }

    /// Apply Hunger I for 30 seconds (600 server ticks).
    pub fn apply_rotten_flesh_effect(&mut self) {
        self.hunger_effect_ticks = 600;
    }
}

// ---------------------------------------------------------------------------
// Digging
// ---------------------------------------------------------------------------

/// Time (seconds) to break a block by hand: hardness × 1.5, or × 5 when
/// a proper tool is required (and none exists yet). Bedrock/air: never.
pub fn dig_time(block: BlockId) -> Option<f32> {
    let bi = block_info(block);
    if bi.hardness.is_infinite() {
        return None;
    }
    Some(bi.hardness * if bi.needs_tool { 5.0 } else { 1.5 })
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

/// Add `count` of `item` to the inventory. Returns the leftover count.
pub fn add_item(inv: &mut [Slot], item: ItemId, count: u8) -> u8 {
    if item == 0 {
        return 0;
    }
    let max_stack = ssc_world::item_info(item).max_stack;
    let mut left = count;
    // Top up existing stacks first, then empty slots (hotbar first).
    for slot in inv.iter_mut() {
        if left == 0 {
            break;
        }
        if let Some((s_item, s_count)) = slot {
            if *s_item == item && *s_count < max_stack {
                let add = (max_stack - *s_count).min(left);
                *s_count += add;
                left -= add;
            }
        }
    }
    for slot in inv.iter_mut() {
        if left == 0 {
            break;
        }
        if slot.is_none() {
            let add = max_stack.min(left);
            *slot = Some((item, add));
            left -= add;
        }
    }
    left
}

/// The item a broken block drops (with the leaf→apple chance).
pub fn drop_for(block: BlockId, rng: f32) -> Option<ItemId> {
    if block == ssc_world::OAK_LEAVES {
        // Higher than vanilla so food is reachable without saplings.
        return (rng < ssc_world::LEAF_APPLE_CHANCE).then_some(ssc_world::APPLE);
    }
    ssc_world::items::block_drop(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dig_times_match_wiki_approximations() {
        // Dirt by hand: 0.5 hardness → 0.75 s.
        assert!((dig_time(ssc_world::DIRT).unwrap() - 0.75).abs() < 0.01);
        // Stone by hand: 1.5 hardness, tool needed → 7.5 s.
        assert!((dig_time(ssc_world::STONE).unwrap() - 7.5).abs() < 0.01);
        // Bedrock: never.
        assert!(dig_time(ssc_world::BEDROCK).is_none());
        assert!(dig_time(ssc_world::AIR).is_none());
    }

    #[test]
    fn fall_damage_needs_three_blocks() {
        let mut s = SurvivalState::default();
        // Fall 2 blocks: no damage.
        let fx = s.tick(TickInput { landed: true, dy: -2.0, ..Default::default() });
        assert_eq!(fx.damage, 0.0);
        // Fall 6 blocks: 3 damage (6 − 3).
        let fx = s.tick(TickInput { landed: true, dy: -6.0, ..Default::default() });
        assert_eq!(fx.damage, 3.0);
        // Water cancels.
        let fx = s.tick(TickInput {
            landed: true,
            in_water: true,
            dy: -10.0,
            ..Default::default()
        });
        assert_eq!(fx.damage, 0.0);
    }

    #[test]
    fn drowning_drains_air_then_health() {
        let mut s = SurvivalState::default();
        for _ in 0..MAX_AIR {
            s.tick(TickInput { eye_in_water: true, ..Default::default() });
        }
        assert_eq!(s.air, 0);
        // Then ~1 heart per second.
        let mut damage = 0.0;
        for _ in 0..20 {
            damage += s.tick(TickInput { eye_in_water: true, ..Default::default() }).damage;
        }
        assert_eq!(damage, 2.0);
    }

    #[test]
    fn void_damage_starts_below_build_limit_and_resets_on_return() {
        let mut s = SurvivalState::default();
        for _ in 0..9 {
            assert_eq!(
                s.tick(TickInput { below_world: true, ..Default::default() }).damage,
                0.0
            );
        }
        let fx = s.tick(TickInput { below_world: true, ..Default::default() });
        assert_eq!(fx.damage, 4.0);
        assert_eq!(s.health, 16.0);
        for _ in 0..5 {
            s.tick(TickInput { below_world: true, ..Default::default() });
        }
        s.tick(TickInput::default());
        for _ in 0..9 {
            assert_eq!(
                s.tick(TickInput { below_world: true, ..Default::default() }).damage,
                0.0
            );
        }
        assert_eq!(
            s.tick(TickInput { below_world: true, ..Default::default() }).damage,
            4.0
        );
    }

    #[test]
    fn hunger_drains_from_sprinting_and_regen_heals() {
        let mut s = SurvivalState::default();
        // Sprint exhaustion: 0.025/tick → 1 hunger point per 160 ticks.
        for _ in 0..160 {
            s.tick(TickInput { moving: true, sprinting: true, ..Default::default() });
        }
        assert_eq!(s.hunger, MAX_HUNGER - 1);
        // Damage → regen path.
        let mut fx = TickEffects::default();
        s.apply_damage(10.0, &mut fx);
        assert_eq!(s.health, 10.0);
        s.hunger = MAX_HUNGER;
        for _ in 0..80 {
            s.tick(TickInput::default());
        }
        assert_eq!(s.health, 11.0, "one heart per 4 s at full hunger");
    }

    #[test]
    fn rotten_flesh_hunger_effect_runs_for_thirty_seconds() {
        let mut s = SurvivalState::default();
        s.apply_rotten_flesh_effect();
        for _ in 0..600 {
            s.tick(TickInput::default());
        }
        assert_eq!(s.hunger_effect_ticks, 0);
        assert!((s.exhaustion - 3.0).abs() < 0.001);
    }

    #[test]
    fn death_and_respawn_reset_vitals() {
        let mut s = SurvivalState::default();
        let mut fx = TickEffects::default();
        s.apply_damage(50.0, &mut fx);
        assert!(s.dead && s.health == 0.0 && fx.died);
        assert!(!s.should_respawn());
        assert!(s.digging.is_none(), "digging cancels on death");
        s.respawn();
        assert_eq!(s.health, MAX_HEALTH);
        assert_eq!(s.hunger, MAX_HUNGER);
        assert_eq!(s.air, MAX_AIR);
    }

    #[test]
    fn inventory_stacks_and_fills_hotbar_first() {
        let mut inv: Vec<Slot> = vec![None; INVENTORY_SIZE];
        // 130 dirt → 64 + 64 + 2.
        assert_eq!(add_item(&mut inv, ssc_world::items::for_block(ssc_world::DIRT), 130), 0);
        assert_eq!(inv[0], Some((4, 64)));
        assert_eq!(inv[1], Some((4, 64)));
        assert_eq!(inv[2], Some((4, 2)));
        assert_eq!(inv[3], None);
        // Overflow when full.
        let mut full: Vec<Slot> = vec![Some((4, 64)); INVENTORY_SIZE];
        assert_eq!(add_item(&mut full, 4, 10), 10);
    }

    #[test]
    fn drops_follow_the_mapping() {
        assert_eq!(
            drop_for(ssc_world::STONE, 1.0),
            Some(ssc_world::items::for_block(ssc_world::COBBLESTONE))
        );
        assert_eq!(
            drop_for(ssc_world::GRASS_BLOCK, 1.0),
            Some(ssc_world::items::for_block(ssc_world::DIRT))
        );
        // Leaves: apple at ~5%, nothing otherwise.
        assert_eq!(drop_for(ssc_world::OAK_LEAVES, 0.01), Some(ssc_world::APPLE));
        assert_eq!(drop_for(ssc_world::OAK_LEAVES, 0.5), None);
    }
}
