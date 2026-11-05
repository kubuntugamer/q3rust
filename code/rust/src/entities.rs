//! The BSP entity string: the key/value blocks `G_SpawnEntitiesFromString`
//! turns into server entities.
//!
//! `COM_Parse` is the same tokenizer the shader scripts use, so this walks
//! [`crate::bsp::next_token`] and groups what it returns into blocks. Nothing is
//! spawned here: this is the list of things a map asks for, which the game
//! module and the client both need to agree on.

use crate::bsp::next_token;

/// One `{ ... }` block from the entity string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entity {
    /// `classname`, when the block has one.
    pub classname: Option<String>,
    /// Every key/value pair in the order q3map wrote them, classname included.
    pub pairs: Vec<(String, String)>,
}

impl Entity {
    /// The value for `key`. A repeated key resolves to the last one, which is
    /// what the spawn variables leave behind after copying them in order.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// `origin` as three floats, when it parses.
    pub fn origin(&self) -> Option<[f32; 3]> {
        let mut parts = self.get("origin")?.split_whitespace();
        let x = parts.next()?.parse().ok()?;
        let y = parts.next()?.parse().ok()?;
        let z = parts.next()?.parse().ok()?;
        Some([x, y, z])
    }
}

/// Every block in `text`. A block that is never closed still yields what it
/// had, which is what the server would have spawned up to that point; a key
/// with no value keeps an empty one rather than dropping the key.
///
/// Tokens pair up in order, and `COM_Parse` stops a bare word at whitespace, so
/// a value with spaces has to be quoted: unquoted `origin 1 2 3` is four tokens
/// and leaves `origin` holding just `1`, as the C parser reads it too.
pub fn parse(text: &[u8]) -> Vec<Entity> {
    let mut entities = Vec::new();
    let mut current: Option<Entity> = None;
    let mut pending_key: Option<String> = None;
    let mut at = 0;

    while let Some(token) = next_token(text, &mut at) {
        match token.as_str() {
            "{" => {
                current = Some(Entity::default());
                pending_key = None;
            }
            "}" => {
                if let Some(entity) = current.take() {
                    entities.push(close(entity, pending_key.take()));
                }
                pending_key = None;
            }
            _ => {
                // Tokens outside a block belong to nothing; the C parser only
                // reads between braces.
                let Some(entity) = current.as_mut() else {
                    continue;
                };
                match pending_key.take() {
                    Some(key) => {
                        if key == "classname" {
                            entity.classname = Some(token.clone());
                        }
                        entity.pairs.push((key, token));
                    }
                    None => pending_key = Some(token),
                }
            }
        }
    }

    if let Some(entity) = current {
        entities.push(close(entity, pending_key));
    }
    entities
}

/// Attaches a trailing key that never got its value, as an empty pair.
fn close(mut entity: Entity, pending_key: Option<String>) -> Entity {
    if let Some(key) = pending_key {
        entity.pairs.push((key, String::new()));
    }
    entity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_become_key_value_pairs() {
        let entities = parse(
            b"{ \"classname\" \"worldspawn\" \"message\" \"test map\" }\n\
              { \"classname\" \"info_player_deathmatch\" \"origin\" \"64 128 16\" }",
        );
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].classname.as_deref(), Some("worldspawn"));
        assert_eq!(entities[0].get("message"), Some("test map"));
        assert_eq!(entities[1].classname.as_deref(), Some("info_player_deathmatch"));
        assert_eq!(entities[1].origin(), Some([64.0, 128.0, 16.0]));
    }

    #[test]
    fn comments_and_bare_words_are_tokens_too() {
        let entities = parse(
            b"// a note\n{ classname info_player_deathmatch /* inline */ \"origin\" \"1 2 3\" }",
        );
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].classname.as_deref(), Some("info_player_deathmatch"));
        assert_eq!(entities[0].origin(), Some([1.0, 2.0, 3.0]));
    }

    #[test]
    fn an_unquoted_multi_word_value_pairs_one_token_at_a_time() {
        // Not a bug: this is how `COM_Parse` reads it, and q3map always quotes.
        let entities = parse(b"{ \"classname\" \"target_position\" origin 1 2 3 }");
        assert_eq!(entities[0].get("origin"), Some("1"));
        assert_eq!(entities[0].get("2"), Some("3"));
    }

    #[test]
    fn a_repeated_key_resolves_to_the_last_value() {
        let entities = parse(b"{ \"classname\" \"func_door\" \"classname\" \"func_button\" }");
        assert_eq!(entities[0].classname.as_deref(), Some("func_button"));
        assert_eq!(entities[0].get("classname"), Some("func_button"));
    }

    #[test]
    fn an_unclosed_block_still_counts() {
        let entities = parse(b"{ \"classname\" \"worldspawn\"");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].classname.as_deref(), Some("worldspawn"));
    }

    #[test]
    fn a_key_without_a_value_keeps_its_key() {
        let entities = parse(b"{ \"classname\" \"target_position\" \"targetname\" }");
        assert_eq!(entities[0].get("targetname"), Some(""));
    }

    #[test]
    fn tokens_outside_a_block_are_ignored_and_an_empty_map_has_none() {
        assert!(parse(b"").is_empty());
        assert!(parse(b"stray words // nothing to spawn\n").is_empty());
    }

    #[test]
    fn a_bad_origin_is_not_an_origin() {
        let entities = parse(b"{ \"classname\" \"info_player_deathmatch\" \"origin\" \"1 2\" }");
        assert_eq!(entities[0].origin(), None);
    }
}
