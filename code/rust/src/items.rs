//! The items a map places: the pickups `q3map` wrote into the BSP's entity
//! lump, as something the renderer can draw.
//!
//! `G_SpawnItem` gives every pickup the same collision box and the game drops
//! the item's md3 model inside it. The md3 is not ported yet, so for now each
//! item is a plain box of that size, coloured by what kind of pickup it is.
//! The box is the marker; the model is what replaces it later.

use crate::bg_misc::{self, Item, ItemType};
use crate::bg_pmove::{PW_BLUEFLAG, PW_REDFLAG};
use crate::entities::{self, Entity};
use crate::vk::Vertex3D;

/// `G_SpawnItem`'s box, relative to the entity origin: a pickup is 30 units
/// square and 16 tall. The origin a map gives an item sits on the floor, so
/// the box stands on it rather than around it.
pub const ITEM_MINS: [f32; 3] = [-15.0, -15.0, 0.0];
/// The top of that box.
pub const ITEM_MAXS: [f32; 3] = [15.0, 15.0, 16.0];

/// One pickup the map places, at the origin the map gave it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ItemMark {
    /// The item's spawn name, which is also its row in `bg_itemlist`.
    pub class_name: &'static str,
    pub origin: [f32; 3],
    pub color: [u8; 4],
}

/// Every item the map's entity lump places. Blocks whose class is not in
/// `bg_itemlist`, and items the map left without an origin, are skipped: the
/// game would not spawn a pickup out of those either.
pub fn item_marks(bytes: &[u8]) -> Vec<ItemMark> {
    marks_in(&entities::parse(bytes))
}

/// The same list, for an entity lump something else already parsed.
pub fn marks_in(list: &[Entity]) -> Vec<ItemMark> {
    list.iter().filter_map(mark_of).collect()
}

fn mark_of(entity: &Entity) -> Option<ItemMark> {
    let class_name = entity.classname.as_deref()?;
    let item = bg_misc::find(class_name)?;
    let origin = entity.origin()?;
    let color = color_of(item)?;
    Some(ItemMark {
        class_name: item.class_name,
        origin,
        color,
    })
}

/// A colour per kind of pickup, so a weapon, a health pack and a flag read
/// apart at a glance. Team items take their team's colour; anything with no
/// kind of its own is not a pickup and gets no box.
fn color_of(item: &Item) -> Option<[u8; 4]> {
    Some(match item.item_type {
        ItemType::Weapon => [235, 205, 80, 255],
        ItemType::Ammo => [175, 175, 165, 255],
        ItemType::Armor => [95, 190, 110, 255],
        ItemType::Health => [205, 85, 85, 255],
        ItemType::Powerup => [155, 110, 235, 255],
        ItemType::Holdable => [90, 200, 210, 255],
        ItemType::Team => {
            if item.tag == PW_REDFLAG as i32 {
                [205, 60, 60, 255]
            } else {
                [70, 105, 215, 255]
            }
        }
        ItemType::Bad | ItemType::PersistantPowerup => return None,
    })
}

/// The triangles for every mark: twelve per box, six faces of two.
pub fn box_vertices(marks: &[ItemMark]) -> Vec<Vertex3D> {
    let mut vertices = Vec::with_capacity(marks.len() * 36);
    for mark in marks {
        push_box(&mut vertices, mark);
    }
    vertices
}

/// The eight corners of a face, then the two triangles that cover it. The
/// renderer draws both sides, so the winding only has to be consistent within
/// the face.
const FACES: [[usize; 4]; 6] = [
    [1, 3, 7, 5], // +X
    [0, 4, 6, 2], // -X
    [2, 6, 7, 3], // +Y
    [0, 1, 5, 4], // -Y
    [4, 5, 7, 6], // +Z
    [0, 2, 3, 1], // -Z
];

fn push_box(vertices: &mut Vec<Vertex3D>, mark: &ItemMark) {
    let min = [
        mark.origin[0] + ITEM_MINS[0],
        mark.origin[1] + ITEM_MINS[1],
        mark.origin[2] + ITEM_MINS[2],
    ];
    let max = [
        mark.origin[0] + ITEM_MAXS[0],
        mark.origin[1] + ITEM_MAXS[1],
        mark.origin[2] + ITEM_MAXS[2],
    ];
    // Bit 0 picks x, bit 1 y, bit 2 z: the corners in cube order.
    let corner = |index: usize| -> [f32; 3] {
        [
            if index & 1 == 0 { min[0] } else { max[0] },
            if index & 2 == 0 { min[1] } else { max[1] },
            if index & 4 == 0 { min[2] } else { max[2] },
        ]
    };

    for face in FACES {
        for index in [face[0], face[1], face[2], face[0], face[2], face[3]] {
            vertices.push(Vertex3D {
                pos: corner(index),
                // The renderer's white texture, tinted by the vertex colour.
                uv: [0.0, 0.0],
                color: mark.color,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEMS: &[u8] = b"{ \"classname\" \"worldspawn\" }\n\
        { \"classname\" \"weapon_rocketlauncher\" \"origin\" \"64 128 24\" }\n\
        { \"classname\" \"item_health\" \"origin\" \"-64 0 8\" }\n\
        { \"classname\" \"ammo_rockets\" \"origin\" \"32 32 16\" }\n\
        { \"classname\" \"info_player_deathmatch\" \"origin\" \"0 0 0\" }";

    #[test]
    fn only_the_items_the_table_knows_become_marks() {
        let marks = item_marks(ITEMS);
        assert_eq!(marks.len(), 3);
        let names: Vec<&str> = marks.iter().map(|mark| mark.class_name).collect();
        assert_eq!(names, ["weapon_rocketlauncher", "item_health", "ammo_rockets"]);
    }

    #[test]
    fn a_mark_keeps_the_origin_the_map_gave_it() {
        let marks = item_marks(ITEMS);
        assert_eq!(marks[0].origin, [64.0, 128.0, 24.0]);
        assert_eq!(marks[1].origin, [-64.0, 0.0, 8.0]);
    }

    #[test]
    fn an_item_without_an_origin_is_left_out() {
        let marks = item_marks(b"{ \"classname\" \"weapon_shotgun\" }");
        assert!(marks.is_empty());
    }

    #[test]
    fn a_map_with_no_items_has_no_boxes() {
        let marks = item_marks(b"{ \"classname\" \"worldspawn\" }");
        assert!(marks.is_empty());
        assert!(box_vertices(&marks).is_empty());
    }

    #[test]
    fn the_empty_table_row_never_matches() {
        assert!(bg_misc::find("").is_none());
        assert!(bg_misc::find("info_player_deathmatch").is_none());
        assert_eq!(bg_misc::find("item_health").unwrap().quantity, 25);
    }

    #[test]
    fn each_mark_is_a_box_standing_on_its_origin() {
        let marks = item_marks(ITEMS);
        let vertices = box_vertices(&marks);
        assert_eq!(vertices.len(), marks.len() * 36);

        let first = &marks[0];
        let (x, y, z) = (first.origin[0], first.origin[1], first.origin[2]);
        for vertex in &vertices[..36] {
            assert!((x + ITEM_MINS[0]..=x + ITEM_MAXS[0]).contains(&vertex.pos[0]));
            assert!((y + ITEM_MINS[1]..=y + ITEM_MAXS[1]).contains(&vertex.pos[1]));
            assert!((z + ITEM_MINS[2]..=z + ITEM_MAXS[2]).contains(&vertex.pos[2]));
            assert_eq!(vertex.color, first.color);
        }
        // Twelve triangles over eight corners, so the box is closed.
        let mut corners: Vec<[u32; 3]> = vertices[..36]
            .iter()
            .map(|vertex| {
                [
                    vertex.pos[0].to_bits(),
                    vertex.pos[1].to_bits(),
                    vertex.pos[2].to_bits(),
                ]
            })
            .collect();
        corners.sort_unstable();
        corners.dedup();
        assert_eq!(corners.len(), 8);
    }

    #[test]
    fn the_kinds_of_pickup_read_apart() {
        let marks = item_marks(ITEMS);
        let weapon = marks[0].color;
        let health = marks[1].color;
        let ammo = marks[2].color;
        assert_ne!(weapon, health);
        assert_ne!(weapon, ammo);
        assert_ne!(health, ammo);
    }

    #[test]
    fn the_two_flags_take_their_team_colour() {
        let marks = item_marks(
            b"{ \"classname\" \"team_CTF_redflag\" \"origin\" \"0 0 0\" }\n\
              { \"classname\" \"team_CTF_blueflag\" \"origin\" \"64 0 0\" }",
        );
        assert_eq!(marks.len(), 2);
        assert_ne!(marks[0].color, marks[1].color);
        assert_eq!(marks[0].class_name, "team_CTF_redflag");
        assert_eq!(marks[1].class_name, "team_CTF_blueflag");
    }
}
