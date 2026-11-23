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
/// Current foods take 1.6 seconds of uninterrupted use to consume.
pub const EAT_TICKS: u8 = 32;
/// Max air, in ticks (Minecraft: 300 = 15 s of breath).
pub const MAX_AIR: u16 = 300;
/// Seconds before auto-respawn (a death screen would be nicer).
pub const RESPAWN_DELAY: Duration = Duration::from_secs(2);

/// One inventory slot: (item, count). `None` = empty.
pub type Slot = Option<(ItemId, u8)>;

/// Bedrock durability for the currently implemented wooden/stone/iron tools.
pub fn max_durability(item: ItemId) -> u16 {
    match item {
        ssc_world::WOODEN_PICKAXE
        | ssc_world::WOODEN_AXE
        | ssc_world::WOODEN_SHOVEL
        | ssc_world::WOODEN_SWORD => 60,
        ssc_world::STONE_PICKAXE
        | ssc_world::STONE_AXE
        | ssc_world::STONE_SHOVEL
        | ssc_world::STONE_SWORD => 132,
        ssc_world::IRON_PICKAXE
        | ssc_world::IRON_AXE
        | ssc_world::IRON_SHOVEL
        | ssc_world::IRON_SWORD => 251,
        _ => 0,
    }
}

/// Old saves and server-created tools without metadata start at full durability.
pub fn effective_durability(item: ItemId, remaining: u16) -> u16 {
    if remaining == 0 {
        max_durability(item)
    } else {
        remaining
    }
}

/// Bedrock melee damage (health points). Other items act as a bare hand.
pub fn attack_damage(held: Option<ItemId>) -> f32 {
    match held {
        Some(ssc_world::WOODEN_PICKAXE) => 3.0,
        Some(ssc_world::STONE_PICKAXE) => 4.0,
        Some(ssc_world::WOODEN_AXE) => 4.0,
        Some(ssc_world::STONE_AXE) => 5.0,
        Some(ssc_world::WOODEN_SHOVEL) => 2.0,
        Some(ssc_world::STONE_SHOVEL) => 3.0,
        Some(ssc_world::IRON_PICKAXE) => 5.0,
        Some(ssc_world::IRON_AXE) => 6.0,
        Some(ssc_world::IRON_SHOVEL) => 4.0,
        Some(ssc_world::WOODEN_SWORD) => 5.0,
        Some(ssc_world::STONE_SWORD) => 6.0,
        Some(ssc_world::IRON_SWORD) => 7.0,
        _ => 1.0,
    }
}

/// Short-lived combat protection, shared by players and mobs. A stronger
/// follow-up hit deals only its excess without extending the original window.
#[derive(Default)]
pub(crate) struct DamageImmunity {
    remaining: u8,
    strongest: f32,
}

impl DamageImmunity {
    pub fn tick(&mut self) {
        self.remaining = self.remaining.saturating_sub(1);
    }

    pub fn filter(&mut self, amount: f32) -> f32 {
        if !amount.is_finite() || amount <= 0.0 {
            return 0.0;
        }
        if self.remaining == 0 {
            self.remaining = 10;
            self.strongest = amount;
            return amount;
        }
        let extra = (amount - self.strongest).max(0.0);
        self.strongest = self.strongest.max(amount);
        extra
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
    combat_immunity: DamageImmunity,
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
            combat_immunity: DamageImmunity::default(),
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
        self.combat_immunity.tick();

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

    /// Melee damage uses a ten-tick protection window. Environmental damage
    /// remains on its own timers for now.
    pub fn apply_combat_damage(&mut self, amount: f32, fx: &mut TickEffects) {
        if self.dead {
            return;
        }
        let damage = self.combat_immunity.filter(amount);
        self.apply_damage(damage, fx);
    }

    /// Direct damage, also used by environmental hazards.
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
/// a proper tool is required. Bedrock/air: never.
pub fn dig_time(block: BlockId) -> Option<f32> {
    dig_time_with_tool(block, None)
}

/// Wooden/stone/iron tools mine their effective block classes at speed 2/4/6.
/// An insufficient mining tier uses the slower, non-harvestable timing.
pub fn dig_time_with_tool(block: BlockId, held: Option<ItemId>) -> Option<f32> {
    let bi = block_info(block);
    if bi.hardness.is_infinite() {
        return None;
    }
    let effective = match held {
        Some(ssc_world::WOODEN_PICKAXE | ssc_world::STONE_PICKAXE | ssc_world::IRON_PICKAXE) => {
            bi.needs_tool && block != ssc_world::SNOW_BLOCK
        }
        Some(ssc_world::WOODEN_AXE | ssc_world::STONE_AXE | ssc_world::IRON_AXE) => {
            matches!(
                block,
                ssc_world::OAK_LOG | ssc_world::STRIPPED_OAK_LOG | ssc_world::OAK_PLANKS
            )
        }
        Some(ssc_world::WOODEN_SHOVEL | ssc_world::STONE_SHOVEL | ssc_world::IRON_SHOVEL) => {
            matches!(
                block,
                ssc_world::DIRT
                    | ssc_world::GRASS_BLOCK
                    | ssc_world::SAND
                    | ssc_world::GRAVEL
                    | ssc_world::SNOW_BLOCK
            )
        }
        _ => false,
    };
    let speed = if held.is_some_and(ssc_world::is_sword) {
        1.5
    } else if !effective {
        1.0
    } else {
        match held {
            Some(ssc_world::WOODEN_PICKAXE | ssc_world::WOODEN_AXE | ssc_world::WOODEN_SHOVEL) => {
                2.0
            }
            Some(ssc_world::IRON_PICKAXE | ssc_world::IRON_AXE | ssc_world::IRON_SHOVEL) => 6.0,
            _ => 4.0,
        }
    };
    Some(bi.hardness * if can_harvest(block, held) { 1.5 } else { 5.0 } / speed)
}

/// Bedrock mining penalties: a submerged head and no supporting ground
/// each divide mining speed by five. They stack to 25× while swimming.
pub fn dig_time_with_penalties(
    block: BlockId,
    held: Option<ItemId>,
    eye_in_water: bool,
    on_ground: bool,
) -> Option<f32> {
    dig_time_with_tool(block, held)
        .map(|base| base * if eye_in_water { 5.0 } else { 1.0 } * if on_ground { 1.0 } else { 5.0 })
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

/// Interim inventory-wide equivalent of the two-tool crafting repair.
/// The selected tool is repaired using the first other tool of the exact
/// same item type. Failure leaves both inventory and wear untouched.
pub fn repair_tool(inv: &mut [Slot], durability: &mut [u16], selected: usize) -> bool {
    if inv.len() != durability.len() {
        return false;
    }
    let Some(Some((item, 1))) = inv.get(selected).copied() else {
        return false;
    };
    let max = max_durability(item);
    if max == 0 {
        return false;
    }
    let Some(other) = inv
        .iter()
        .enumerate()
        .position(|(slot, value)| slot != selected && *value == Some((item, 1)))
    else {
        return false;
    };
    let repaired = effective_durability(item, durability[selected])
        .saturating_add(effective_durability(item, durability[other]))
        .saturating_add(max / 20)
        .min(max);
    inv[other] = None;
    durability[other] = 0;
    durability[selected] = repaired;
    true
}

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
        4 => (
            &[(ssc_world::OAK_PLANKS, 3), (ssc_world::STICK, 2)][..],
            (ssc_world::WOODEN_AXE, 1),
        ),
        5 => (
            &[(ssc_world::COBBLESTONE, 3), (ssc_world::STICK, 2)][..],
            (ssc_world::STONE_AXE, 1),
        ),
        6 => (
            &[(ssc_world::OAK_PLANKS, 1), (ssc_world::STICK, 2)][..],
            (ssc_world::WOODEN_SHOVEL, 1),
        ),
        7 => (
            &[(ssc_world::COBBLESTONE, 1), (ssc_world::STICK, 2)][..],
            (ssc_world::STONE_SHOVEL, 1),
        ),
        9 => (&[(ssc_world::COBBLESTONE, 8)][..], (ssc_world::FURNACE, 1)),
        10 => (
            &[(ssc_world::IRON_INGOT, 3), (ssc_world::STICK, 2)][..],
            (ssc_world::IRON_PICKAXE, 1),
        ),
        11 => (
            &[(ssc_world::IRON_INGOT, 3), (ssc_world::STICK, 2)][..],
            (ssc_world::IRON_AXE, 1),
        ),
        12 => (
            &[(ssc_world::IRON_INGOT, 1), (ssc_world::STICK, 2)][..],
            (ssc_world::IRON_SHOVEL, 1),
        ),
        13 => (
            &[(ssc_world::OAK_PLANKS, 2), (ssc_world::STICK, 1)][..],
            (ssc_world::WOODEN_SWORD, 1),
        ),
        14 => (
            &[(ssc_world::COBBLESTONE, 2), (ssc_world::STICK, 1)][..],
            (ssc_world::STONE_SWORD, 1),
        ),
        15 => (
            &[(ssc_world::IRON_INGOT, 2), (ssc_world::STICK, 1)][..],
            (ssc_world::IRON_SWORD, 1),
        ),
        _ => return false,
    };
    let mut next = inv.to_vec();
    for &(item, count) in ingredients {
        let mut remaining = count;
        for slot in &mut next {
            if let Some((held, available)) = *slot {
                if held == item
                    || (item == ssc_world::OAK_LOG && held == ssc_world::STRIPPED_OAK_LOG)
                {
                    let taken = available.min(remaining);
                    remaining -= taken;
                    *slot = if taken == available {
                        None
                    } else {
                        Some((held, available - taken))
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
    if ssc_world::is_oak_leaves(block) {
        // Higher than vanilla so food is reachable without saplings.
        return (rng < ssc_world::LEAF_APPLE_CHANCE).then_some(ssc_world::APPLE);
    }
    if block == ssc_world::GRAVEL && rng < 0.10 {
        return Some(ssc_world::FLINT);
    }
    ssc_world::items::block_drop(block)
}

/// Shared harvesting rules for timing and drops.
fn can_harvest(block: BlockId, held: Option<ItemId>) -> bool {
    if block == ssc_world::SNOW_BLOCK {
        return matches!(
            held,
            Some(ssc_world::WOODEN_SHOVEL | ssc_world::STONE_SHOVEL | ssc_world::IRON_SHOVEL)
        );
    }
    if block_info(block).needs_tool {
        let required_stone = matches!(
            block,
            ssc_world::IRON_ORE
                | ssc_world::DEEPSLATE_IRON_ORE
                | ssc_world::COPPER_ORE
                | ssc_world::DEEPSLATE_COPPER_ORE
        );
        let valid = if required_stone {
            matches!(
                held,
                Some(ssc_world::STONE_PICKAXE | ssc_world::IRON_PICKAXE)
            )
        } else {
            matches!(
                held,
                Some(
                    ssc_world::WOODEN_PICKAXE | ssc_world::STONE_PICKAXE | ssc_world::IRON_PICKAXE
                )
            )
        };
        return valid;
    }
    true
}

pub fn drop_with_tool(block: BlockId, rng: f32, held: Option<ItemId>) -> Option<ItemId> {
    if !can_harvest(block, held) {
        return None;
    }
    drop_for(block, rng)
}

/// Full loot list. Leaf apple, sapling and stick rolls are independent,
/// so a single leaf can yield several item types.
pub fn block_drops(block: BlockId, roll: u64, held: Option<ItemId>) -> Vec<(ItemId, u8)> {
    if !can_harvest(block, held) {
        return Vec::new();
    }
    let unit = |shift: u32| ((roll >> shift) & 0xffffu64) as f32 / 65536.0;
    if ssc_world::is_oak_leaves(block) {
        let mut drops = Vec::new();
        if unit(0) < ssc_world::LEAF_APPLE_CHANCE {
            drops.push((ssc_world::APPLE, 1));
        }
        if unit(16) < 0.05 {
            drops.push((ssc_world::OAK_SAPLING, 1));
        }
        if unit(32) < 0.02 {
            drops.push((ssc_world::STICK, 1 + ((roll >> 48) & 1) as u8));
        }
        return drops;
    }
    drop_with_tool(block, unit(0), held)
        .map(|item| vec![(item, drop_count(block, unit(0)))])
        .unwrap_or_default()
}

/// Unenchanted copper ore yields 2–5 raw copper; other block drops are
/// single items. Called only once the mining tier has been validated.
pub fn drop_count(block: BlockId, rng: f32) -> u8 {
    if block == ssc_world::SNOW_BLOCK {
        return 4;
    }
    if matches!(
        block,
        ssc_world::COPPER_ORE | ssc_world::DEEPSLATE_COPPER_ORE
    ) {
        2 + ((rng * 4.0) as u8).min(3)
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn swords_mine_faster_but_cannot_harvest_pickaxe_blocks_and_can_repair() {
        for item in [
            ssc_world::WOODEN_SWORD,
            ssc_world::STONE_SWORD,
            ssc_world::IRON_SWORD,
        ] {
            assert!((dig_time_with_tool(ssc_world::DIRT, Some(item)).unwrap() - 0.5).abs() < 0.001);
            assert!(block_drops(ssc_world::IRON_ORE, 0, Some(item)).is_empty());
            let mut inv = vec![None; INVENTORY_SIZE];
            let mut wear = vec![0; INVENTORY_SIZE];
            inv[0] = Some((item, 1));
            inv[1] = Some((item, 1));
            wear[0] = 10;
            wear[1] = 20;
            assert!(repair_tool(&mut inv, &mut wear, 0));
            assert_eq!(wear[0], 30 + max_durability(item) / 20);
        }
    }
    #[test]
    fn glass_drops_nothing_and_smooth_stone_requires_a_pickaxe() {
        for tool in [
            None,
            Some(ssc_world::IRON_PICKAXE),
            Some(ssc_world::IRON_AXE),
        ] {
            assert!(block_drops(ssc_world::GLASS, 0, tool).is_empty());
            assert!((dig_time_with_tool(ssc_world::GLASS, tool).unwrap() - 0.45).abs() < 0.001);
        }
        assert!(block_drops(ssc_world::SMOOTH_STONE, 0, None).is_empty());
        assert_eq!(
            block_drops(ssc_world::SMOOTH_STONE, 0, Some(ssc_world::WOODEN_PICKAXE)),
            vec![(ssc_world::SMOOTH_STONE, 1)]
        );
        assert!(ssc_world::is_solid(ssc_world::GLASS));
        assert!(!ssc_world::is_opaque(ssc_world::GLASS));
    }
    #[test]
    fn iron_tools_craft_mine_harvest_and_repair_at_bedrock_values() {
        for (recipe, tool, ingots, block, damage) in [
            (10, ssc_world::IRON_PICKAXE, 3, ssc_world::IRON_ORE, 5.0),
            (11, ssc_world::IRON_AXE, 3, ssc_world::OAK_LOG, 6.0),
            (12, ssc_world::IRON_SHOVEL, 1, ssc_world::SNOW_BLOCK, 4.0),
        ] {
            let mut inv = vec![None; INVENTORY_SIZE];
            inv[0] = Some((ssc_world::IRON_INGOT, ingots));
            inv[1] = Some((ssc_world::STICK, 2));
            assert!(craft(&mut inv, recipe));
            assert_eq!(inv[0], Some((tool, 1)));
            assert_eq!(inv[1], None);
            assert_eq!(max_durability(tool), 251);
            assert_eq!(attack_damage(Some(tool)), damage);
            assert!(
                (dig_time_with_tool(block, Some(tool)).unwrap()
                    - block_info(block).hardness * 1.5 / 6.0)
                    .abs()
                    < 0.001
            );
            assert!(!block_drops(block, 0, Some(tool)).is_empty());
            let mut wear = vec![0; INVENTORY_SIZE];
            wear[0] = 100;
            inv[1] = Some((tool, 1));
            wear[1] = 50;
            assert!(repair_tool(&mut inv, &mut wear, 0));
            assert_eq!(wear[0], 162);
            assert_eq!(inv[1], None);
        }
        assert_eq!(
            drop_with_tool(ssc_world::FURNACE, 0.5, Some(ssc_world::IRON_PICKAXE)),
            Some(ssc_world::FURNACE)
        );
        assert_eq!(
            drop_with_tool(ssc_world::COPPER_ORE, 0.5, Some(ssc_world::IRON_AXE)),
            None
        );
    }
    use super::*;

    #[test]
    fn combat_immunity_ignores_weaker_hits_and_only_applies_stronger_difference() {
        let mut s = SurvivalState::default();
        let hit = |s: &mut SurvivalState, damage| {
            let mut fx = TickEffects::default();
            s.apply_combat_damage(damage, &mut fx);
            fx.hurt
        };
        assert!(hit(&mut s, 3.0));
        assert_eq!(s.health, 17.0);
        assert!(!hit(&mut s, 2.0));
        assert!(!hit(&mut s, 3.0));
        assert!(hit(&mut s, 5.0));
        assert_eq!(s.health, 15.0);
        for _ in 0..9 {
            s.tick(TickInput::default());
        }
        assert!(!hit(&mut s, 5.0));
        s.tick(TickInput::default());
        assert!(
            hit(&mut s, 3.0),
            "stronger follow-ups must not restart the timer"
        );
        assert_eq!(s.health, 12.0);
        s.respawn();
        assert!(hit(&mut s, 3.0), "respawn resets immunity");
    }

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
    fn water_and_airborne_mining_penalties_stack() {
        let held = Some(ssc_world::STONE_PICKAXE);
        let base = dig_time_with_tool(ssc_world::STONE, held).unwrap();
        assert_eq!(
            dig_time_with_penalties(ssc_world::STONE, held, false, true),
            Some(base)
        );
        assert_eq!(
            dig_time_with_penalties(ssc_world::STONE, held, true, true),
            Some(base * 5.0)
        );
        assert_eq!(
            dig_time_with_penalties(ssc_world::STONE, held, false, false),
            Some(base * 5.0)
        );
        assert_eq!(
            dig_time_with_penalties(ssc_world::STONE, held, true, false),
            Some(base * 25.0)
        );
        assert_eq!(
            dig_time_with_penalties(ssc_world::BEDROCK, held, true, false),
            None
        );
    }

    #[test]
    fn pickaxe_stats_use_bedrock_values() {
        assert_eq!(max_durability(ssc_world::WOODEN_PICKAXE), 60);
        assert_eq!(max_durability(ssc_world::STONE_PICKAXE), 132);
        assert_eq!(attack_damage(None), 1.0);
        assert_eq!(attack_damage(Some(ssc_world::WOODEN_PICKAXE)), 3.0);
        assert_eq!(attack_damage(Some(ssc_world::STONE_PICKAXE)), 4.0);
        for (wood, stone, wood_damage, stone_damage) in [
            (ssc_world::WOODEN_AXE, ssc_world::STONE_AXE, 4.0, 5.0),
            (ssc_world::WOODEN_SHOVEL, ssc_world::STONE_SHOVEL, 2.0, 3.0),
        ] {
            assert_eq!(max_durability(wood), 60);
            assert_eq!(max_durability(stone), 132);
            assert_eq!(attack_damage(Some(wood)), wood_damage);
            assert_eq!(attack_damage(Some(stone)), stone_damage);
        }
    }

    #[test]
    fn axes_and_shovels_speed_only_their_effective_block_classes() {
        for block in [ssc_world::OAK_LOG, ssc_world::OAK_PLANKS] {
            let hand = dig_time(block).unwrap();
            assert_eq!(
                dig_time_with_tool(block, Some(ssc_world::WOODEN_AXE)),
                Some(hand / 2.0)
            );
            assert_eq!(
                dig_time_with_tool(block, Some(ssc_world::STONE_AXE)),
                Some(hand / 4.0)
            );
            assert_eq!(
                dig_time_with_tool(block, Some(ssc_world::STONE_SHOVEL)),
                Some(hand)
            );
        }
        for block in [
            ssc_world::DIRT,
            ssc_world::GRASS_BLOCK,
            ssc_world::SAND,
            ssc_world::GRAVEL,
        ] {
            let hand = dig_time(block).unwrap();
            assert_eq!(
                dig_time_with_tool(block, Some(ssc_world::WOODEN_SHOVEL)),
                Some(hand / 2.0)
            );
            assert_eq!(
                dig_time_with_tool(block, Some(ssc_world::STONE_SHOVEL)),
                Some(hand / 4.0)
            );
            assert_eq!(
                dig_time_with_tool(block, Some(ssc_world::STONE_AXE)),
                Some(hand)
            );
        }
        assert_eq!(
            drop_with_tool(ssc_world::STONE, 0.5, Some(ssc_world::STONE_AXE)),
            None
        );
        for block in [ssc_world::IRON_ORE, ssc_world::COPPER_ORE] {
            assert_eq!(
                dig_time_with_tool(block, Some(ssc_world::WOODEN_PICKAXE)),
                Some(7.5)
            );
            assert_eq!(
                drop_with_tool(block, 0.5, Some(ssc_world::WOODEN_PICKAXE)),
                None
            );
        }
    }

    #[test]
    fn snow_requires_a_shovel_and_drops_four_snowballs() {
        let block = ssc_world::SNOW_BLOCK;
        assert_eq!(drop_with_tool(block, 0.5, None), None);
        assert_eq!(
            drop_with_tool(block, 0.5, Some(ssc_world::STONE_PICKAXE)),
            None
        );
        for tool in [ssc_world::WOODEN_SHOVEL, ssc_world::STONE_SHOVEL] {
            assert_eq!(
                drop_with_tool(block, 0.5, Some(tool)),
                Some(ssc_world::SNOWBALL)
            );
            assert_eq!(drop_count(block, 0.5), 4);
        }
    }

    #[test]
    fn crafting_axes_and_shovels_consumes_correct_materials_atomically() {
        for (recipe, material, amount, tool) in [
            (4, ssc_world::OAK_PLANKS, 3, ssc_world::WOODEN_AXE),
            (5, ssc_world::COBBLESTONE, 3, ssc_world::STONE_AXE),
            (6, ssc_world::OAK_PLANKS, 1, ssc_world::WOODEN_SHOVEL),
            (7, ssc_world::COBBLESTONE, 1, ssc_world::STONE_SHOVEL),
        ] {
            let mut inv = vec![None; INVENTORY_SIZE];
            inv[0] = Some((material, amount));
            let before = inv.clone();
            assert!(!craft(&mut inv, recipe), "sticks required");
            assert_eq!(inv, before);
            inv[35] = Some((ssc_world::STICK, 2));
            assert!(craft(&mut inv, recipe));
            assert_eq!(
                inv.iter().flatten().copied().collect::<Vec<_>>(),
                vec![(tool, 1)]
            );
        }
    }

    #[test]
    fn repair_combines_identical_tools_and_adds_five_percent_bonus() {
        for (item, max, bonus) in [
            (ssc_world::WOODEN_PICKAXE, 60, 3),
            (ssc_world::STONE_PICKAXE, 132, 6),
            (ssc_world::WOODEN_AXE, 60, 3),
            (ssc_world::STONE_AXE, 132, 6),
            (ssc_world::WOODEN_SHOVEL, 60, 3),
            (ssc_world::STONE_SHOVEL, 132, 6),
        ] {
            let mut inv = vec![Some((ssc_world::STONE, 64)); INVENTORY_SIZE];
            let mut wear = vec![0; INVENTORY_SIZE];
            inv[0] = Some((item, 1));
            inv[35] = Some((item, 1));
            wear[0] = 7;
            wear[35] = 11;
            assert!(repair_tool(&mut inv, &mut wear, 0));
            assert_eq!(inv[0], Some((item, 1)));
            assert_eq!(wear[0], 18 + bonus);
            assert_eq!(inv[35], None);
            assert_eq!(wear[35], 0);
            inv[35] = Some((item, 1));
            wear[35] = max;
            assert!(repair_tool(&mut inv, &mut wear, 0));
            assert_eq!(wear[0], max, "repair is capped");
        }
    }

    #[test]
    fn repair_rejects_non_tools_and_mixed_types_without_changes() {
        let mut inv = vec![None; INVENTORY_SIZE];
        let mut wear = vec![0; INVENTORY_SIZE];
        inv[0] = Some((ssc_world::WOODEN_AXE, 1));
        inv[1] = Some((ssc_world::STONE_AXE, 1));
        inv[2] = Some((ssc_world::WOODEN_PICKAXE, 1));
        inv[3] = Some((ssc_world::STICK, 12));
        wear[0] = 5;
        wear[1] = 17;
        wear[2] = 4;
        let before = (inv.clone(), wear.clone());
        for selected in [0, 1, 2, 3, 4, 36] {
            assert!(!repair_tool(&mut inv, &mut wear, selected));
            assert_eq!((&inv, &wear), (&before.0, &before.1));
        }
    }

    #[test]
    fn stripped_logs_drop_themselves_and_craft_into_planks() {
        let block = ssc_world::STRIPPED_OAK_LOG;
        assert_eq!(drop_with_tool(block, 0.5, None), Some(block));
        assert_eq!(
            dig_time_with_tool(block, Some(ssc_world::STONE_AXE)),
            Some(0.75)
        );
        let mut inv = vec![None; INVENTORY_SIZE];
        inv[35] = Some((block, 2));
        assert!(craft(&mut inv, 0));
        assert_eq!(
            inv[35],
            Some((block, 1)),
            "remaining material stays stripped"
        );
        assert_eq!(inv[0], Some((ssc_world::OAK_PLANKS, 4)));
    }

    #[test]
    fn leaf_sapling_stick_and_apple_rolls_are_independent() {
        for leaf in [ssc_world::OAK_LEAVES, ssc_world::GROWN_OAK_LEAVES] {
            assert_eq!(
                block_drops(leaf, 0, None),
                vec![
                    (ssc_world::APPLE, 1),
                    (ssc_world::OAK_SAPLING, 1),
                    (ssc_world::STICK, 1),
                ]
            );
            assert!(block_drops(leaf, u64::MAX, None).is_empty());
            assert_eq!(
                block_drops(leaf, 1 << 48, None).last(),
                Some(&(ssc_world::STICK, 2))
            );
            let sapling_only = 0xffff | (0xffff << 32);
            assert_eq!(
                block_drops(leaf, sapling_only, None),
                vec![(ssc_world::OAK_SAPLING, 1)]
            );
        }
        assert_eq!(
            block_drops(ssc_world::OAK_SAPLING_READY, 0, None),
            vec![(ssc_world::OAK_SAPLING, 1)]
        );
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
        for block in [ssc_world::COPPER_ORE, ssc_world::DEEPSLATE_COPPER_ORE] {
            assert_eq!(drop_with_tool(block, 0.5, None), None);
            assert_eq!(
                drop_with_tool(block, 0.5, Some(ssc_world::WOODEN_PICKAXE)),
                None
            );
            assert_eq!(
                drop_with_tool(block, 0.5, Some(ssc_world::STONE_PICKAXE)),
                Some(ssc_world::RAW_COPPER)
            );
            for (roll, count) in [(0.0, 2), (0.25, 3), (0.5, 4), (0.99, 5), (1.0, 5)] {
                assert_eq!(drop_count(block, roll), count);
            }
        }
        assert_eq!(drop_with_tool(ssc_world::STONE, 0.0, None), None);
        assert_eq!(
            drop_with_tool(ssc_world::COAL_ORE, 0.0, Some(ssc_world::WOODEN_PICKAXE)),
            Some(ssc_world::COAL)
        );
        assert_eq!(
            drop_with_tool(ssc_world::DEEPSLATE_COAL_ORE, 0.0, None),
            None
        );
        assert_eq!(
            drop_with_tool(
                ssc_world::DEEPSLATE_COAL_ORE,
                0.0,
                Some(ssc_world::WOODEN_PICKAXE)
            ),
            Some(ssc_world::COAL)
        );
        assert!(
            (dig_time_with_tool(
                ssc_world::DEEPSLATE_COAL_ORE,
                Some(ssc_world::STONE_PICKAXE)
            )
            .unwrap()
                - 1.6875)
                .abs()
                < 0.001
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
