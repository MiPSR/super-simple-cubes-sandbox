//! Server-owned furnace slots, fuel and cooking. Container access and saves
//! are handled by the game/persistence layers.

use crate::survival::Slot;
use ssc_world::ItemId;

pub const INPUT: usize = 0;
pub const FUEL: usize = 1;
pub const OUTPUT: usize = 2;
pub const COOK_TICKS: u16 = 200;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Furnace {
    pub slots: [Slot; 3],
    pub burn_remaining: u16,
    pub burn_total: u16,
    pub cook_ticks: u16,
}

const RECIPES: &[(ItemId, ItemId)] = &[
    (ssc_world::RAW_IRON, ssc_world::IRON_INGOT),
    (ssc_world::RAW_COPPER, ssc_world::COPPER_INGOT),
    (ssc_world::PORKCHOP, ssc_world::COOKED_PORKCHOP),
    (ssc_world::MUTTON, ssc_world::COOKED_MUTTON),
    (ssc_world::OAK_LOG, ssc_world::CHARCOAL),
    (ssc_world::STRIPPED_OAK_LOG, ssc_world::CHARCOAL),
    (ssc_world::SAND, ssc_world::GLASS),
    (ssc_world::COBBLESTONE, ssc_world::STONE),
    (ssc_world::STONE, ssc_world::SMOOTH_STONE),
    (ssc_world::COBBLED_DEEPSLATE, ssc_world::DEEPSLATE),
];

pub fn recipe(item: ItemId) -> Option<ItemId> {
    RECIPES
        .iter()
        .find_map(|(input, output)| (*input == item).then_some(*output))
}

pub fn fuel_ticks(item: ItemId) -> u16 {
    match item {
        ssc_world::COAL | ssc_world::CHARCOAL => 1600,
        ssc_world::OAK_LOG | ssc_world::STRIPPED_OAK_LOG | ssc_world::OAK_PLANKS => 300,
        ssc_world::STICK => 100,
        _ => 0,
    }
}

pub fn accepts(slot: usize, item: ItemId) -> bool {
    match slot {
        INPUT => recipe(item).is_some(),
        FUEL => fuel_ticks(item) > 0,
        _ => false,
    }
}

impl Furnace {
    pub fn valid(&self) -> bool {
        self.burn_remaining <= self.burn_total
            && self.burn_total <= 1600
            && self.cook_ticks < COOK_TICKS
            && self.slots.iter().enumerate().all(|(slot, value)| {
                value.is_none_or(|(item, count)| {
                    ssc_world::valid_item_id(item)
                        && item != 0
                        && count > 0
                        && count <= ssc_world::item_info(item).max_stack
                        && if slot == OUTPUT {
                            RECIPES.iter().any(|(_, output)| *output == item)
                        } else {
                            accepts(slot, item)
                        }
                })
            })
    }

    /// Changes are reported for inventory/timer sync. A burning furnace
    /// consumes its remaining fuel even if input runs out or output fills.
    pub fn tick(&mut self) -> bool {
        let before = self.clone();
        let output = self.slots[INPUT].and_then(|(item, _)| recipe(item));
        let can_cook = output.is_some_and(|item| match self.slots[OUTPUT] {
            None => true,
            Some((held, count)) => held == item && count < ssc_world::item_info(item).max_stack,
        });
        if can_cook && self.burn_remaining == 0 {
            if let Some((fuel, count)) = self.slots[FUEL] {
                let ticks = fuel_ticks(fuel);
                if ticks > 0 {
                    self.slots[FUEL] = if count == 1 {
                        None
                    } else {
                        Some((fuel, count - 1))
                    };
                    self.burn_remaining = ticks;
                    self.burn_total = ticks;
                }
            }
        }
        if can_cook && self.burn_remaining > 0 {
            self.cook_ticks += 1;
            if self.cook_ticks == COOK_TICKS {
                self.cook_ticks = 0;
                let (item, count) = self.slots[INPUT].expect("cooking has input");
                self.slots[INPUT] = if count == 1 {
                    None
                } else {
                    Some((item, count - 1))
                };
                let output = output.expect("validated recipe");
                let count = self.slots[OUTPUT].map_or(1, |(_, count)| count + 1);
                self.slots[OUTPUT] = Some((output, count));
            }
        } else if !can_cook {
            self.cook_ticks = 0;
        } else {
            self.cook_ticks = self.cook_ticks.saturating_sub(2);
        }
        self.burn_remaining = self.burn_remaining.saturating_sub(1);
        *self != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_registered_recipes_cook_in_two_hundred_ticks_and_validate_output() {
        for &(input, output) in RECIPES {
            let mut furnace = Furnace::default();
            furnace.slots[INPUT] = Some((input, 1));
            furnace.slots[FUEL] = Some((ssc_world::COAL, 1));
            for _ in 0..199 {
                furnace.tick();
            }
            assert_eq!(furnace.slots[OUTPUT], None);
            furnace.tick();
            assert_eq!(furnace.slots[OUTPUT], Some((output, 1)));
            assert_eq!(furnace.slots[INPUT], None);
            assert!(
                furnace.valid(),
                "recipe output must survive saves: {output}"
            );
        }
    }

    #[test]
    fn coal_cooks_eight_items_at_two_hundred_ticks_each() {
        let mut furnace = Furnace::default();
        furnace.slots[INPUT] = Some((ssc_world::RAW_IRON, 8));
        furnace.slots[FUEL] = Some((ssc_world::COAL, 1));
        for _ in 0..199 {
            furnace.tick();
        }
        assert_eq!(furnace.slots[OUTPUT], None);
        furnace.tick();
        assert_eq!(furnace.slots[OUTPUT], Some((ssc_world::IRON_INGOT, 1)));
        for _ in 200..1600 {
            furnace.tick();
        }
        assert_eq!(furnace.slots[INPUT], None);
        assert_eq!(furnace.slots[FUEL], None);
        assert_eq!(furnace.slots[OUTPUT], Some((ssc_world::IRON_INGOT, 8)));
        assert_eq!(furnace.burn_remaining, 0);
    }

    #[test]
    fn blocked_output_does_not_consume_new_fuel_but_existing_fire_keeps_burning() {
        let mut furnace = Furnace::default();
        furnace.slots = [
            Some((ssc_world::RAW_COPPER, 1)),
            Some((ssc_world::COAL, 1)),
            Some((ssc_world::COPPER_INGOT, 64)),
        ];
        furnace.tick();
        assert_eq!(furnace.slots[FUEL], Some((ssc_world::COAL, 1)));
        furnace.slots[OUTPUT] = None;
        furnace.tick();
        assert_eq!(furnace.burn_remaining, 1599);
        furnace.slots[OUTPUT] = Some((ssc_world::IRON_INGOT, 1));
        furnace.tick();
        assert_eq!(furnace.burn_remaining, 1598);
        assert_eq!(furnace.cook_ticks, 0);
    }

    #[test]
    fn short_fuel_cools_progress_and_charcoal_is_renewable_fuel() {
        let mut furnace = Furnace::default();
        furnace.slots[INPUT] = Some((ssc_world::OAK_LOG, 1));
        furnace.slots[FUEL] = Some((ssc_world::STICK, 1));
        for _ in 0..100 {
            furnace.tick();
        }
        assert_eq!(furnace.cook_ticks, 100);
        furnace.tick();
        assert_eq!(furnace.cook_ticks, 98);
        furnace.slots[FUEL] = Some((ssc_world::OAK_PLANKS, 1));
        for _ in 0..102 {
            furnace.tick();
        }
        assert_eq!(furnace.slots[OUTPUT], Some((ssc_world::CHARCOAL, 1)));
        assert_eq!(fuel_ticks(ssc_world::CHARCOAL), 1600);
    }
}
