//! Survival-mode systems: inventory, vitals (health/hunger/air), fall
//! damage, and timed digging. Server-authoritative (`specs.md` §2).
//!
//! Pure state + logic functions, unit-tested; `game.rs` owns the wire
//! side (broadcasts, sounds, packet handling).

use std::time::{Duration, Instant};

use ssc_world::{block_info, BlockId, ItemId};

/// Inventory size: 9 hotbar slots + 27 storage.
pub const INVENTORY_SIZE: usize = 36;
/// Max health (Minecraft: 20 = 10 hearts).
pub const MAX_HEALTH: f32 = 20.0;
/// Max hunger (Minecraft: 20 = 10 drumsticks).
pub const MAX_HUNGER: u8 = 20;
/// A newly spawned player starts with 5 hidden saturation points.
pub const INITIAL_SATURATION: f32 = 5.0;
/// Sprinting requires more than three full drumsticks.
pub const MIN_SPRINT_HUNGER: u8 = 7;
/// Max air, in ticks (Minecraft: 300 = 15 s of breath).
pub const MAX_AIR: u16 = 300;
/// Seconds before auto-respawn (a death screen would be nicer).
pub const RESPAWN_DELAY: Duration = Duration::from_secs(2);

/// One inventory slot: (item, count). `None` = empty.
pub type Slot = Option<(ItemId, u8)>;

/// Maximum uses for the two currently implemented pickaxe tiers.
pub fn max_durability(item: ItemId) -> u16 {
    match item {
        ssc_world::WOODEN_PICKAXE => 60,
        ssc_world::STONE_PICKAXE => 132,
        _ => 0,
    }
}

/// Old saves and server-created pickaxes without metadata start at full wear.
pub fn effective_durability(item: ItemId, remaining: u16) -> u16 {
    if remaining == 0 {
        max_durability(item)
    } else {
        remaining
    }
}

/// Bedrock melee damage (health points): bare hand 2, wooden/stone
/// pickaxe 3/4. Other items currently act as a bare hand.
pub fn attack_damage(held: Option<ItemId>) -> f32 {
    match held {
        Some(ssc_world::WOODEN_PICKAXE) => 3.0,
        Some(ssc_world::STONE_PICKAXE) => 4.0,
        _ => 2.0,
    }
}

/// Survival-mode per-player state.
pub struct SurvivalState {
    pub health: f32,
    pub hunger: u8,
    /// Hidden food buffer, consumed before the visible hunger bar.
    pub saturation: f32,
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
            saturation: INITIAL_SATURATION,
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
    /// Actual horizontal distance traveled this tick (blocks).
    pub horizontal_distance: f32,
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
        // Exertion is based on distance actually traveled, not held input:
        // walking and sneaking cost nothing; swimming costs 0.01/m, sprinting
        // 0.1/m.
        self.exhaustion += input.horizontal_distance.max(0.0)
            * if input.in_water {
                0.01
            } else if input.sprinting {
                0.1
            } else {
                0.0
            };
        if input.jumped {
            self.exhaustion += if input.sprinting { 0.2 } else { 0.05 };
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
            if self.saturation > 0.0 {
                self.saturation = (self.saturation - 1.0).max(0.0);
            } else if self.hunger > 0 {
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
        self.dead && self.respawn_at.is_some_and(|at| Instant::now() >= at)
    }

    /// Reset vitals after respawn. The server owns the separate inventory and
    /// dropped-item state, so resetting this structure leaves both untouched.
    pub fn respawn(&mut self) {
        *self = SurvivalState::default();
    }

    /// Restore food and saturation. Bedrock apples can be eaten at full
    /// hunger; other available foods cannot.
    pub fn eat(&mut self, points: u8, saturation: f32, always_edible: bool) -> bool {
        if self.hunger >= MAX_HUNGER && !always_edible {
            return false;
        }
        self.hunger = (self.hunger + points).min(MAX_HUNGER);
        self.saturation = (self.saturation + saturation).min(self.hunger as f32);
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
    dig_time_with_tool(block, None)
}

/// Bedrock mining speed: a correct wooden/stone pickaxe mines at speed 2/4;
/// without the correct tool, tool-required blocks take hardness × 5.
pub fn dig_time_with_tool(block: BlockId, held: Option<ItemId>) -> Option<f32> {
    let bi = block_info(block);
    if bi.hardness.is_infinite() {
        return None;
    }
    let speed = match held {
        Some(ssc_world::WOODEN_PICKAXE) => 2.0,
        Some(ssc_world::STONE_PICKAXE) => 4.0,
        _ => 1.0,
    };
    Some(if bi.needs_tool && speed == 1.0 {
        bi.hardness * 5.0
    } else {
        bi.hardness * 1.5 / if bi.needs_tool { speed } else { 1.0 }
    })
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

/// Craft one output using materials anywhere in the 36-slot inventory.
/// Atomic on failure (missing ingredients, unknown recipe or no output room).
/// The recipe ID is chosen by the client, never the output or ingredients.
pub fn craft(inv: &mut [Slot], recipe: u8) -> bool {
    let (ingredients, output) = match recipe {
        0 => (&[(ssc_world::OAK_LOG, 1)][..], (ssc_world::OAK_PLANKS, 4)),
        1 => (&[(ssc_world::OAK_PLANKS, 2)][..], (ssc_world::STICK, 4)),
        2 => (
            &[(ssc_world::OAK_PLANKS, 3), (ssc_world::STICK, 2)][..],
            (ssc_world::WOODEN_PICKAXE, 1),
        ),
        3 => (
            &[(ssc_world::COBBLESTONE, 3), (ssc_world::STICK, 2)][..],
            (ssc_world::STONE_PICKAXE, 1),
        ),
        _ => return false,
    };
    let mut next = inv.to_vec();
    for &(item, count) in ingredients {
        let mut remaining = count;
        for slot in &mut next {
            if let Some((held, available)) = *slot {
                if held == item {
                    let taken = available.min(remaining);
                    remaining -= taken;
                    *slot = if taken == available {
                        None
                    } else {
                        Some((item, available - taken))
                    };
                    if remaining == 0 {
                        break;
                    }
                }
            }
        }
        if remaining != 0 {
            return false;
        }
    }
    if add_item(&mut next, output.0, output.1) != 0 {
        return false;
    }
    inv.copy_from_slice(&next);
    true
}

/// The item a broken block drops (with the leaf→apple chance).
pub fn drop_for(block: BlockId, rng: f32) -> Option<ItemId> {
    if block == ssc_world::OAK_LEAVES {
        // Higher than vanilla so food is reachable without saplings.
        return (rng < ssc_world::LEAF_APPLE_CHANCE).then_some(ssc_world::APPLE);
    }
    if block == ssc_world::GRAVEL && rng < 0.10 {
        return Some(ssc_world::FLINT);
    }
    ssc_world::items::block_drop(block)
}

/// Tool tiers currently represented in inventory. Iron ore requires stone
/// or better; stone, deepslate and coal accept any pickaxe.
pub fn drop_with_tool(block: BlockId, rng: f32, held: Option<ItemId>) -> Option<ItemId> {
    if block_info(block).needs_tool {
        let required_stone = matches!(block, ssc_world::IRON_ORE | ssc_world::DEEPSLATE_IRON_ORE);
        let valid = if required_stone {
            held == Some(ssc_world::STONE_PICKAXE)
        } else {
            matches!(
                held,
                Some(ssc_world::WOODEN_PICKAXE | ssc_world::STONE_PICKAXE)
            )
        };
        if !valid {
            return None;
        }
    }
    drop_for(block, rng)
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
        assert!(
            (dig_time_with_tool(ssc_world::STONE, Some(ssc_world::WOODEN_PICKAXE)).unwrap()
                - 1.125)
                .abs()
                < 0.01
        );
        assert!(
            (dig_time_with_tool(ssc_world::STONE, Some(ssc_world::STONE_PICKAXE)).unwrap()
                - 0.5625)
                .abs()
                < 0.01
        );
    }

    #[test]
    fn pickaxe_stats_use_bedrock_values() {
        assert_eq!(max_durability(ssc_world::WOODEN_PICKAXE), 60);
        assert_eq!(max_durability(ssc_world::STONE_PICKAXE), 132);
        assert_eq!(attack_damage(None), 2.0);
        assert_eq!(attack_damage(Some(ssc_world::WOODEN_PICKAXE)), 3.0);
        assert_eq!(attack_damage(Some(ssc_world::STONE_PICKAXE)), 4.0);
    }

    #[test]
    fn fall_damage_needs_three_blocks() {
        let mut s = SurvivalState::default();
        // Fall 2 blocks: no damage.
        let fx = s.tick(TickInput {
            landed: true,
            dy: -2.0,
            ..Default::default()
        });
        assert_eq!(fx.damage, 0.0);
        // Fall 6 blocks: 3 damage (6 − 3).
        let fx = s.tick(TickInput {
            landed: true,
            dy: -6.0,
            ..Default::default()
        });
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
            s.tick(TickInput {
                eye_in_water: true,
                ..Default::default()
            });
        }
        assert_eq!(s.air, 0);
        // Then ~1 heart per second.
        let mut damage = 0.0;
        for _ in 0..20 {
            damage += s
                .tick(TickInput {
                    eye_in_water: true,
                    ..Default::default()
                })
                .damage;
        }
        assert_eq!(damage, 2.0);
    }

    #[test]
    fn void_damage_starts_below_build_limit_and_resets_on_return() {
        let mut s = SurvivalState::default();
        for _ in 0..9 {
            assert_eq!(
                s.tick(TickInput {
                    below_world: true,
                    ..Default::default()
                })
                .damage,
                0.0
            );
        }
        let fx = s.tick(TickInput {
            below_world: true,
            ..Default::default()
        });
        assert_eq!(fx.damage, 4.0);
        assert_eq!(s.health, 16.0);
        for _ in 0..5 {
            s.tick(TickInput {
                below_world: true,
                ..Default::default()
            });
        }
        s.tick(TickInput::default());
        for _ in 0..9 {
            assert_eq!(
                s.tick(TickInput {
                    below_world: true,
                    ..Default::default()
                })
                .damage,
                0.0
            );
        }
        assert_eq!(
            s.tick(TickInput {
                below_world: true,
                ..Default::default()
            })
            .damage,
            4.0
        );
    }

    #[test]
    fn hunger_drains_from_sprinting_and_regen_heals() {
        let mut s = SurvivalState::default();
        // Six 40-block sprints consume the initial 5 saturation points,
        // then one visible hunger point.
        for _ in 0..5 {
            s.exhaustion = 4.0;
            s.tick(TickInput::default());
        }
        assert_eq!(s.saturation, 0.0);
        assert_eq!(s.hunger, MAX_HUNGER);
        // 40 more blocks sprinting uses one visible hunger point.
        for _ in 0..101 {
            s.tick(TickInput {
                horizontal_distance: 0.4,
                sprinting: true,
                ..Default::default()
            });
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
    fn food_replenishes_saturation_without_exceeding_hunger() {
        let mut s = SurvivalState::default();
        s.hunger = 1;
        s.saturation = 0.0;
        assert!(s.eat(3, 1.8, false));
        assert_eq!(s.hunger, 4);
        assert!((s.saturation - 1.8).abs() < 0.0001);
        assert!(s.eat(4, 2.4, true));
        assert_eq!(s.hunger, 8);
        assert!((s.saturation - 4.2).abs() < 0.0001);
        s.hunger = MAX_HUNGER;
        assert!(!s.eat(4, 0.8, false));
        assert!(s.eat(4, 2.4, true));
        assert!((s.saturation - 6.6).abs() < 0.0001);
        s.hunger = 1;
        s.saturation = 1.0;
        assert!(s.eat(1, 20.0, false));
        assert_eq!(s.saturation, 2.0);
    }

    #[test]
    fn exhaustion_uses_travel_and_sprint_jump_costs() {
        let mut s = SurvivalState::default();
        s.tick(TickInput {
            sprinting: true,
            ..Default::default()
        });
        assert_eq!(
            s.exhaustion, 0.0,
            "held sprint without movement costs nothing"
        );
        s.tick(TickInput {
            horizontal_distance: 2.0,
            ..Default::default()
        });
        assert_eq!(s.exhaustion, 0.0, "walking costs nothing");
        s.tick(TickInput {
            horizontal_distance: 2.0,
            in_water: true,
            ..Default::default()
        });
        assert!((s.exhaustion - 0.02).abs() < 0.0001);
        s.tick(TickInput {
            horizontal_distance: 2.0,
            sprinting: true,
            jumped: true,
            ..Default::default()
        });
        assert!((s.exhaustion - 0.42).abs() < 0.0001);
        s.tick(TickInput {
            jumped: true,
            ..Default::default()
        });
        assert!((s.exhaustion - 0.47).abs() < 0.0001);
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
        assert_eq!(
            add_item(&mut inv, ssc_world::items::for_block(ssc_world::DIRT), 130),
            0
        );
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
        assert_eq!(
            drop_for(ssc_world::OAK_LEAVES, 0.01),
            Some(ssc_world::APPLE)
        );
        assert_eq!(drop_for(ssc_world::OAK_LEAVES, 0.5), None);
        assert_eq!(
            drop_with_tool(ssc_world::GRAVEL, 0.099, None),
            Some(ssc_world::FLINT)
        );
        assert_eq!(
            drop_with_tool(ssc_world::GRAVEL, 0.1, None),
            Some(ssc_world::GRAVEL)
        );
    }

    #[test]
    fn mining_drops_require_the_right_pickaxe_tier() {
        assert_eq!(drop_with_tool(ssc_world::STONE, 0.0, None), None);
        assert_eq!(
            drop_with_tool(ssc_world::COAL_ORE, 0.0, Some(ssc_world::WOODEN_PICKAXE)),
            Some(ssc_world::COAL)
        );
        assert_eq!(
            drop_with_tool(ssc_world::IRON_ORE, 0.0, Some(ssc_world::WOODEN_PICKAXE)),
            None
        );
        assert_eq!(
            drop_with_tool(
                ssc_world::DEEPSLATE_IRON_ORE,
                0.0,
                Some(ssc_world::STONE_PICKAXE)
            ),
            Some(ssc_world::RAW_IRON)
        );
        assert_eq!(
            drop_with_tool(ssc_world::DEEPSLATE, 0.0, Some(ssc_world::WOODEN_PICKAXE)),
            Some(ssc_world::COBBLED_DEEPSLATE)
        );
    }

    #[test]
    fn crafting_is_atomic_and_progresses_from_logs_to_picks() {
        let mut inv = vec![None; INVENTORY_SIZE];
        inv[34] = Some((ssc_world::OAK_LOG, 2));
        assert!(!craft(&mut inv, 2));
        assert_eq!(inv[34], Some((ssc_world::OAK_LOG, 2)));
        for recipe in [0, 0, 1, 2] {
            assert!(craft(&mut inv, recipe));
        }
        assert!(inv.contains(&Some((ssc_world::WOODEN_PICKAXE, 1))));
        inv[35] = Some((ssc_world::COBBLESTONE, 3));
        assert!(craft(&mut inv, 3));
        assert!(inv.contains(&Some((ssc_world::STONE_PICKAXE, 1))));
        assert!(!craft(&mut inv, 255));
        let snapshot = inv.clone();
        assert!(!craft(&mut inv, 3));
        assert_eq!(inv, snapshot);
    }
}
