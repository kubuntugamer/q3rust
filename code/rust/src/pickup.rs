//! `g_items.c`'s pickup rules and `bg_misc.c`'s `BG_CanItemBeGrabbed`, against
//! the predicted player.
//!
//! The server owns pickups in the real game; a client that predicts them makes
//! the item vanish and the number change the moment the player walks over it,
//! instead of a round trip later. The rules are the shared ones, so the two
//! sides cannot disagree about who may take what.

use crate::bg_misc::{self, ItemType, HI_NONE};
use crate::bg_pmove::{
    PlayerState, STAT_ARMOR, STAT_HEALTH, STAT_HOLDABLE_ITEM, STAT_MAX_HEALTH, STAT_WEAPONS,
    WP_GRAPPLING_HOOK,
};

/// `RESPAWN_ARMOR`.
pub const RESPAWN_ARMOR: u32 = 25_000;
/// `RESPAWN_HEALTH`.
pub const RESPAWN_HEALTH: u32 = 35_000;
/// `RESPAWN_MEGAHEALTH`.
pub const RESPAWN_MEGAHEALTH: u32 = 35_000;
/// `RESPAWN_AMMO`.
pub const RESPAWN_AMMO: u32 = 40_000;
/// `RESPAWN_HOLDABLE`.
pub const RESPAWN_HOLDABLE: u32 = 60_000;
/// `RESPAWN_POWERUP`.
pub const RESPAWN_POWERUP: u32 = 120_000;
/// `g_weaponRespawn` at its stock value.
pub const RESPAWN_WEAPON: u32 = 5_000;

/// The cap `Add_Ammo` puts on a clip.
pub const AMMO_MAX: i32 = 200;

/// `BG_PlayerTouchesItem`: items are grabbed from a box a little larger than
/// the player, so a pickup is not fiddly to walk into. The C compares the two
/// origins, not the bounding boxes, and it is deliberately lopsided in x.
pub fn touches(player: [f32; 3], item: [f32; 3]) -> bool {
    (player[0] - item[0]) <= 44.0
        && (player[0] - item[0]) >= -50.0
        && (player[1] - item[1]) <= 36.0
        && (player[1] - item[1]) >= -36.0
        && (player[2] - item[2]) <= 36.0
        && (player[2] - item[2]) >= -36.0
}

/// `BG_CanItemBeGrabbed`, baseq3 rules: may this player take this item at all?
///
/// Health and armour stop at their limits, ammo stops at 200, a player holds
/// one holdable at a time, and the flags need the team rules a match does not
/// have here yet.
pub fn can_grab(index: usize, ps: &PlayerState) -> bool {
    let Some(item) = bg_misc::item(index as i32) else {
        return false;
    };
    match item.item_type {
        ItemType::Weapon | ItemType::Powerup => true,
        ItemType::Ammo => ammo_of(ps, item.tag) < AMMO_MAX,
        ItemType::Armor => ps.stats[STAT_ARMOR] < ps.stats[STAT_MAX_HEALTH] * 2,
        ItemType::Health => {
            // Small and mega healths go over the normal maximum.
            if item.quantity == 5 || item.quantity == 100 {
                ps.stats[STAT_HEALTH] < ps.stats[STAT_MAX_HEALTH] * 2
            } else {
                ps.stats[STAT_HEALTH] < ps.stats[STAT_MAX_HEALTH]
            }
        }
        ItemType::Holdable => ps.stats[STAT_HOLDABLE_ITEM] == HI_NONE as i32,
        ItemType::Team => false,
        ItemType::Bad | ItemType::PersistantPowerup => false,
    }
}

/// `Touch_Item`: give the player what the item is, and say how many
/// milliseconds until it comes back. `None` when the rules refuse it, in which
/// case the item stays where it is.
pub fn take(index: usize, ps: &mut PlayerState, now: u32) -> Option<u32> {
    let item = *bg_misc::item(index as i32)?;
    if !can_grab(index, ps) {
        return None;
    }
    match item.item_type {
        ItemType::Weapon => {
            let tag = item.tag;
            // A weapon already in hand with a full clip still gives one shot,
            // so walking over it is never nothing.
            let quantity = if ammo_of(ps, tag) < item.quantity {
                item.quantity - ammo_of(ps, tag)
            } else {
                1
            };
            ps.stats[STAT_WEAPONS] |= 1 << tag;
            add_ammo(ps, tag, quantity);
            if tag == WP_GRAPPLING_HOOK {
                if let Some(clip) = ps.ammo.get_mut(tag as usize) {
                    *clip = -1; // unlimited
                }
            }
            Some(RESPAWN_WEAPON)
        }
        ItemType::Ammo => {
            add_ammo(ps, item.tag, item.quantity);
            Some(RESPAWN_AMMO)
        }
        ItemType::Armor => {
            ps.stats[STAT_ARMOR] = (ps.stats[STAT_ARMOR] + item.quantity)
                .min(ps.stats[STAT_MAX_HEALTH] * 2);
            Some(RESPAWN_ARMOR)
        }
        ItemType::Health => {
            let max = if item.quantity == 5 || item.quantity == 100 {
                ps.stats[STAT_MAX_HEALTH] * 2
            } else {
                ps.stats[STAT_MAX_HEALTH]
            };
            ps.stats[STAT_HEALTH] = (ps.stats[STAT_HEALTH] + item.quantity).min(max);
            if item.quantity == 100 {
                Some(RESPAWN_MEGAHEALTH)
            } else {
                Some(RESPAWN_HEALTH)
            }
        }
        ItemType::Powerup => {
            let tag = usize::try_from(item.tag).ok()?;
            let clip = ps.powerups.get_mut(tag)?;
            if *clip == 0 {
                // Round the start to the second, the way the C does, so two
                // powerups picked up together expire together.
                *clip = (now - now % 1000) as i32;
            }
            *clip += item.quantity * 1000;
            Some(RESPAWN_POWERUP)
        }
        ItemType::Holdable => {
            ps.stats[STAT_HOLDABLE_ITEM] = index as i32;
            Some(RESPAWN_HOLDABLE)
        }
        ItemType::Team | ItemType::Bad | ItemType::PersistantPowerup => None,
    }
}

fn ammo_of(ps: &PlayerState, weapon: i32) -> i32 {
    usize::try_from(weapon)
        .ok()
        .and_then(|slot| ps.ammo.get(slot).copied())
        .unwrap_or(0)
}

/// `Add_Ammo`: a clip never holds more than 200.
fn add_ammo(ps: &mut PlayerState, weapon: i32, count: i32) {
    if let Ok(slot) = usize::try_from(weapon) {
        if let Some(clip) = ps.ammo.get_mut(slot) {
            *clip = (*clip + count).min(AMMO_MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bg_misc::{BG_ITEMLIST, HI_MEDKIT};
    use crate::bg_pmove::{PW_QUAD, STAT_WEAPONS, WP_MACHINEGUN, WP_ROCKET_LAUNCHER, WP_SHOTGUN};

    fn player_at_full_health() -> PlayerState {
        let mut ps = PlayerState::new();
        ps.stats[STAT_HEALTH] = 100;
        ps.stats[STAT_MAX_HEALTH] = 100;
        ps
    }

    fn index_of(class_name: &str) -> usize {
        BG_ITEMLIST
            .iter()
            .position(|item| item.class_name == class_name)
            .unwrap_or_else(|| panic!("{class_name} is not in the table"))
    }

    #[test]
    fn a_health_pack_stops_at_the_maximum() {
        let mut ps = player_at_full_health();
        ps.stats[STAT_HEALTH] = 80;
        let pack = index_of("item_health");
        assert_eq!(take(pack, &mut ps, 0), Some(RESPAWN_HEALTH));
        assert_eq!(ps.stats[STAT_HEALTH], 100);
    }

    #[test]
    fn a_full_player_cannot_take_a_health_pack() {
        let mut ps = player_at_full_health();
        assert!(!can_grab(index_of("item_health"), &ps));
        assert_eq!(take(index_of("item_health"), &mut ps, 0), None);
        assert_eq!(ps.stats[STAT_HEALTH], 100);
    }

    #[test]
    fn small_and_mega_healths_go_over_the_maximum() {
        let mut ps = player_at_full_health();
        let small = index_of("item_health_small");
        assert!(can_grab(small, &ps));
        assert_eq!(take(small, &mut ps, 0), Some(RESPAWN_HEALTH));
        assert_eq!(ps.stats[STAT_HEALTH], 105);

        let mut ps = player_at_full_health();
        let mega = index_of("item_health_mega");
        assert_eq!(take(mega, &mut ps, 0), Some(RESPAWN_MEGAHEALTH));
        assert_eq!(ps.stats[STAT_HEALTH], 200);
        // And no further: 200 is the ceiling for mega health too.
        assert!(!can_grab(mega, &ps));
    }

    #[test]
    fn ammo_piles_up_but_stops_at_two_hundred() {
        let mut ps = player_at_full_health();
        let rockets = index_of("ammo_rockets");
        assert_eq!(take(rockets, &mut ps, 0), Some(RESPAWN_AMMO));
        assert_eq!(ps.ammo[WP_ROCKET_LAUNCHER as usize], 5);

        ps.ammo[WP_ROCKET_LAUNCHER as usize] = 198;
        assert_eq!(take(rockets, &mut ps, 0), Some(RESPAWN_AMMO));
        assert_eq!(ps.ammo[WP_ROCKET_LAUNCHER as usize], AMMO_MAX);

        assert!(!can_grab(rockets, &ps));
        assert_eq!(take(rockets, &mut ps, 0), None);
    }

    #[test]
    fn a_weapon_brings_its_clip_and_stays_in_hand() {
        let mut ps = player_at_full_health();
        let rocket = index_of("weapon_rocketlauncher");
        assert_eq!(take(rocket, &mut ps, 0), Some(RESPAWN_WEAPON));
        assert_ne!(ps.stats[STAT_WEAPONS] & (1 << WP_ROCKET_LAUNCHER), 0);
        assert_eq!(ps.ammo[WP_ROCKET_LAUNCHER as usize], 10);

        // A second one only tops the clip up by a single rocket.
        assert_eq!(take(rocket, &mut ps, 0), Some(RESPAWN_WEAPON));
        assert_eq!(ps.ammo[WP_ROCKET_LAUNCHER as usize], 11);
    }

    #[test]
    fn a_weapon_already_at_its_clip_still_gives_one_shot() {
        let mut ps = player_at_full_health();
        ps.stats[STAT_WEAPONS] |= 1 << WP_SHOTGUN;
        ps.ammo[WP_SHOTGUN as usize] = 50;
        let shotgun = index_of("weapon_shotgun");
        take(shotgun, &mut ps, 0);
        assert_eq!(ps.ammo[WP_SHOTGUN as usize], 51);
    }

    #[test]
    fn armour_piles_up_to_twice_the_max_health() {
        let mut ps = player_at_full_health();
        let body = index_of("item_armor_body");
        assert_eq!(take(body, &mut ps, 0), Some(RESPAWN_ARMOR));
        assert_eq!(ps.stats[STAT_ARMOR], 100);
        take(body, &mut ps, 0);
        assert_eq!(ps.stats[STAT_ARMOR], 200);
        assert!(!can_grab(body, &ps));
    }

    #[test]
    fn one_holdable_at_a_time() {
        let mut ps = player_at_full_health();
        let medkit = index_of("holdable_medkit");
        assert_eq!(take(medkit, &mut ps, 0), Some(RESPAWN_HOLDABLE));
        assert_eq!(ps.stats[STAT_HOLDABLE_ITEM], medkit as i32);
        assert_eq!(bg_misc::tag_of(ps.stats[STAT_HOLDABLE_ITEM]), HI_MEDKIT);
        assert!(!can_grab(index_of("holdable_teleporter"), &ps));
    }

    #[test]
    fn a_powerup_starts_its_timer_at_a_round_second() {
        let mut ps = player_at_full_health();
        let quad = index_of("item_quad");
        assert_eq!(take(quad, &mut ps, 1234), Some(RESPAWN_POWERUP));
        assert_eq!(ps.powerups[PW_QUAD as usize], 1000 + 30_000);
        // A second quad adds to the timer instead of restarting it.
        take(quad, &mut ps, 2000);
        assert_eq!(ps.powerups[PW_QUAD as usize], 1000 + 60_000);
    }

    #[test]
    fn the_flags_are_not_taken_in_a_match_without_teams() {
        let mut ps = player_at_full_health();
        assert!(!can_grab(index_of("team_CTF_redflag"), &ps));
        assert_eq!(take(index_of("team_CTF_redflag"), &mut ps, 0), None);
    }

    #[test]
    fn walking_into_an_item_is_a_generous_box() {
        let at = [100.0, 100.0, 100.0];
        assert!(touches(at, at));
        assert!(touches([at[0] + 44.0, at[1] + 36.0, at[2] - 36.0], at));
        assert!(!touches([at[0] + 45.0, at[1], at[2]], at));
        assert!(!touches([at[0] - 51.0, at[1], at[2]], at));
        assert!(!touches([at[0], at[1] + 37.0, at[2]], at));
        assert!(!touches([at[0], at[1], at[2] + 37.0], at));
    }

    #[test]
    fn a_table_row_that_does_not_exist_gives_nothing() {
        let mut ps = player_at_full_health();
        assert_eq!(take(BG_ITEMLIST.len(), &mut ps, 0), None);
        assert!(!can_grab(0, &ps), "the empty row is not a pickup");
    }

    #[test]
    fn a_fresh_player_has_no_clips_yet() {
        let ps = player_at_full_health();
        assert_eq!(ammo_of(&ps, WP_MACHINEGUN), 0);
        assert_eq!(ammo_of(&ps, -1), 0);
    }
}
