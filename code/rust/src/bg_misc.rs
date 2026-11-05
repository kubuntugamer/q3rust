//! `bg_misc.c`, as far as the movement code needs it: the item table.
//!
//! `playerState_t.stats[STAT_HOLDABLE_ITEM]` is an index into `bg_itemlist`,
//! and `PM_Weapon` reads the item's `giTag` to decide whether a medkit may be
//! used. That means the *order* of this table is part of the network state:
//! index 27 has to be the medkit, exactly as in the C build, so this is the
//! baseq3 table without the `#ifdef MISSIONPACK` entries that follow the blue
//! flag.
//!
//! Only the fields anything reads are ported. The world and view models, the
//! icon, the pickup sound, the precache list and the sound list belong to a
//! fuller `bg_misc` port; none of them affect movement, so they are omitted
//! rather than half-filled with the wrong strings.

use crate::bg_pmove::{
    PW_BATTLESUIT, PW_BLUEFLAG, PW_FLIGHT, PW_HASTE, PW_INVIS, PW_QUAD, PW_REDFLAG, PW_REGEN,
    WP_BFG, WP_GAUNTLET, WP_GRENADE_LAUNCHER, WP_GRAPPLING_HOOK, WP_LIGHTNING, WP_MACHINEGUN,
    WP_PLASMAGUN, WP_RAILGUN, WP_ROCKET_LAUNCHER, WP_SHOTGUN,
};

/// `itemType_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemType {
    Bad,
    Weapon,
    Ammo,
    Armor,
    Health,
    Powerup,
    Holdable,
    PersistantPowerup,
    Team,
}

/// `IT_BAD` and friends, spelled the way the C spells them.
pub const IT_BAD: ItemType = ItemType::Bad;
/// `IT_WEAPON`.
pub const IT_WEAPON: ItemType = ItemType::Weapon;
/// `IT_AMMO`.
pub const IT_AMMO: ItemType = ItemType::Ammo;
/// `IT_ARMOR`.
pub const IT_ARMOR: ItemType = ItemType::Armor;
/// `IT_HEALTH`.
pub const IT_HEALTH: ItemType = ItemType::Health;
/// `IT_POWERUP`.
pub const IT_POWERUP: ItemType = ItemType::Powerup;
/// `IT_HOLDABLE`.
pub const IT_HOLDABLE: ItemType = ItemType::Holdable;
/// `IT_PERSISTANT_POWERUP`.
pub const IT_PERSISTANT_POWERUP: ItemType = ItemType::PersistantPowerup;
/// `IT_TEAM`.
pub const IT_TEAM: ItemType = ItemType::Team;

/// `holdable_t`, numbered as `giTag` is for the holdable rows.
pub const HI_NONE: i32 = 0;
/// `HI_TELEPORTER`.
pub const HI_TELEPORTER: i32 = 1;
/// `HI_MEDKIT`.
pub const HI_MEDKIT: i32 = 2;
/// `HI_KAMIKAZE`.
pub const HI_KAMIKAZE: i32 = 3;
/// `HI_PORTAL`.
pub const HI_PORTAL: i32 = 4;
/// `HI_INVULNERABILITY`.
pub const HI_INVULNERABILITY: i32 = 5;

/// The part of `gitem_t` the movement code reads.
#[derive(Clone, Copy, Debug)]
pub struct Item {
    /// `classname`: the spawn name, which is also how a test names a row.
    pub class_name: &'static str,
    /// `quantity`: how much ammo or health the item gives.
    pub quantity: i32,
    /// `giType`.
    pub item_type: ItemType,
    /// `giTag`: the weapon, powerup or holdable number, or 0 when the row has
    /// no tag of its own.
    pub tag: i32,
}

const fn row(class_name: &'static str, quantity: i32, item_type: ItemType, tag: i32) -> Item {
    Item {
        class_name,
        quantity,
        item_type,
        tag,
    }
}

/// `bg_itemlist`, in the baseq3 order. Index 0 is the empty item the C keeps
/// clear, and every later index has to line up with the C table or the stat a
/// server sends would name the wrong item.
pub static BG_ITEMLIST: [Item; 36] = [
    row("", 0, IT_BAD, 0),
    // ARMOR
    row("item_armor_shard", 5, IT_ARMOR, 0),
    row("item_armor_combat", 50, IT_ARMOR, 0),
    row("item_armor_body", 100, IT_ARMOR, 0),
    // HEALTH
    row("item_health_small", 5, IT_HEALTH, 0),
    row("item_health", 25, IT_HEALTH, 0),
    row("item_health_large", 50, IT_HEALTH, 0),
    row("item_health_mega", 100, IT_HEALTH, 0),
    // WEAPONS
    row("weapon_gauntlet", 0, IT_WEAPON, WP_GAUNTLET),
    row("weapon_shotgun", 10, IT_WEAPON, WP_SHOTGUN),
    row("weapon_machinegun", 40, IT_WEAPON, WP_MACHINEGUN),
    row("weapon_grenadelauncher", 10, IT_WEAPON, WP_GRENADE_LAUNCHER),
    row("weapon_rocketlauncher", 10, IT_WEAPON, WP_ROCKET_LAUNCHER),
    row("weapon_lightning", 100, IT_WEAPON, WP_LIGHTNING),
    row("weapon_railgun", 10, IT_WEAPON, WP_RAILGUN),
    row("weapon_plasmagun", 50, IT_WEAPON, WP_PLASMAGUN),
    row("weapon_bfg", 20, IT_WEAPON, WP_BFG),
    row("weapon_grapplinghook", 0, IT_WEAPON, WP_GRAPPLING_HOOK),
    // AMMO
    row("ammo_shells", 10, IT_AMMO, WP_SHOTGUN),
    row("ammo_bullets", 50, IT_AMMO, WP_MACHINEGUN),
    row("ammo_grenades", 5, IT_AMMO, WP_GRENADE_LAUNCHER),
    row("ammo_cells", 30, IT_AMMO, WP_PLASMAGUN),
    row("ammo_lightning", 60, IT_AMMO, WP_LIGHTNING),
    row("ammo_rockets", 5, IT_AMMO, WP_ROCKET_LAUNCHER),
    row("ammo_slugs", 10, IT_AMMO, WP_RAILGUN),
    row("ammo_bfg", 15, IT_AMMO, WP_BFG),
    // HOLDABLES
    row("holdable_teleporter", 60, IT_HOLDABLE, HI_TELEPORTER),
    row("holdable_medkit", 60, IT_HOLDABLE, HI_MEDKIT),
    // POWERUPS
    row("item_quad", 30, IT_POWERUP, PW_QUAD as i32),
    row("item_enviro", 30, IT_POWERUP, PW_BATTLESUIT as i32),
    row("item_haste", 30, IT_POWERUP, PW_HASTE as i32),
    row("item_invis", 30, IT_POWERUP, PW_INVIS as i32),
    row("item_regen", 30, IT_POWERUP, PW_REGEN as i32),
    row("item_flight", 60, IT_POWERUP, PW_FLIGHT as i32),
    // TEAM
    row("team_CTF_redflag", 0, IT_TEAM, PW_REDFLAG as i32),
    row("team_CTF_blueflag", 0, IT_TEAM, PW_BLUEFLAG as i32),
];

/// `bg_numItems`, including the empty row at index 0.
pub const BG_NUM_ITEMS: usize = BG_ITEMLIST.len();

/// `bg_itemlist[i]`, or `None` for an index past the table. The C would read
/// past the array here; a bad stat from the network should not do that.
pub fn item(index: i32) -> Option<&'static Item> {
    if index < 0 {
        return None;
    }
    BG_ITEMLIST.get(index as usize)
}

/// `bg_itemlist[index].giTag`, with [`HI_NONE`] for an index the table does
/// not have. That is the lookup `PM_Weapon` makes on the holdable stat.
pub fn tag_of(index: i32) -> i32 {
    item(index).map(|item| item.tag).unwrap_or(HI_NONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_keeps_the_c_indices() {
        assert_eq!(BG_NUM_ITEMS, 36);
        assert_eq!(BG_ITEMLIST[0].item_type, IT_BAD);
        assert_eq!(BG_ITEMLIST[26].class_name, "holdable_teleporter");
        assert_eq!(BG_ITEMLIST[27].class_name, "holdable_medkit");
        assert_eq!(BG_ITEMLIST[35].class_name, "team_CTF_blueflag");
    }

    #[test]
    fn a_holdable_stat_names_its_tag() {
        assert_eq!(tag_of(26), HI_TELEPORTER);
        assert_eq!(tag_of(27), HI_MEDKIT);
        assert_eq!(tag_of(0), HI_NONE);
    }

    #[test]
    fn an_index_past_the_table_has_no_tag() {
        assert_eq!(tag_of(-1), HI_NONE);
        assert_eq!(tag_of(BG_NUM_ITEMS as i32), HI_NONE);
    }
}
