//! Headless map-data check: what the mounted paks contain, and whether every map
//! in them loads, counts its rooms and surfaces, and parses its entity list. No
//! window and no Vulkan, so it is safe to run over SSH or on a machine whose
//! display is in use.

use std::collections::BTreeSet;
use std::process::ExitCode;

use q3rust::assets::Assets;
use q3rust::bg_misc::{self, BG_NUM_ITEMS};
use q3rust::bsp::Bsp;
use q3rust::entities;
use q3rust::model::Md3Model;
use q3rust::shader::{Parser, ShaderScript, ShaderStage, ShaderTextIndex, TextureMap};
use q3rust::world::{self, MapAudit};

fn main() -> ExitCode {
    // `--strict` turns a dangling shader reference into a failure. The renderer
    // only falls back to its default shader, so the shipping data is allowed
    // to have them; a check that wants to gate on new ones can ask for it.
    let strict = std::env::args().any(|arg| arg == "--strict");

    let assets = match Assets::mount_default() {
        Ok(assets) => assets,
        Err(e) => {
            eprintln!("game data: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "game data: {} paks, {:.1} MB, from {}",
        assets.len(),
        assets.bytes() as f64 / (1024.0 * 1024.0),
        assets.source_directory().display()
    );
    for name in assets.names() {
        println!("  {name}");
    }
    for error in assets.errors() {
        eprintln!("  skipped {error}");
    }

    // `ScanAndLoadShaderFiles`'s table: a name resolves to a shader script
    // first and only falls back to an image file, so anything asking for a
    // name has to be checked against both.
    let scripts = shader_scripts(&assets);
    let script_texts: Vec<&str> = scripts.iter().map(String::as_str).collect();
    let shader_text = ShaderTextIndex::build(&script_texts);

    let mut maps = assets.files("maps/", ".bsp");
    maps.sort();
    if maps.is_empty() {
        eprintln!("no maps/*.bsp in the mounted paks");
        return ExitCode::FAILURE;
    }

    let mut totals = MapAudit::default();
    let mut failed = 0usize;
    let mut shader_names = 0usize;
    let mut missing_shaders = 0usize;
    let mut map_sounds = 0usize;
    let mut missing_sounds = 0usize;
    // The shader names the data actually asks for, so the stage pass below
    // reports art a map or model would draw instead of every stale definition
    // the retail scripts still carry.
    let mut used_shaders: BTreeSet<String> = BTreeSet::new();

    for map in &maps {
        let Some(bytes) = assets.read(map) else {
            eprintln!("{map}: unreadable");
            failed += 1;
            continue;
        };

        let bsp = match Bsp::load(bytes.into_owned()) {
            Ok(bsp) => bsp,
            Err(e) => {
                eprintln!("{map}: {e}");
                failed += 1;
                continue;
            }
        };

        let names = surface_shader_names(&bsp);
        shader_names += names.len();
        for name in &names {
            used_shaders.insert(strip_extension(name).to_string());
        }
        for name in names {
            if resolve(&assets, &shader_text, &name) == Resolution::Missing {
                eprintln!("{map}: shader {name} is neither a script nor an image");
                missing_shaders += 1;
            }
        }

        for name in map_sound_names(&bsp) {
            map_sounds += 1;
            if !sound_present(&assets, &name) {
                eprintln!("{map}: sound {name} is not in any pak");
                missing_sounds += 1;
            }
        }

        let audit = world::map_edges(&bsp);
        let notes = skipped_notes(&audit);
        println!(
            "{map:<30} {:>5} faces {:>4} patch faces {:>7} tris {:>7} tessellated {:>3} rooms {:>4} ents {:>3} spawns {:>3} items{}",
            audit.faces,
            audit.patch_faces,
            audit.triangles,
            audit.patch_triangles,
            audit.rooms,
            audit.entities,
            audit.player_spawns,
            audit.items,
            notes
        );

        totals.rooms += audit.rooms;
        totals.faces += audit.faces;
        totals.patch_faces += audit.patch_faces;
        totals.triangles += audit.triangles;
        totals.patch_triangles += audit.patch_triangles;
        totals.nodraw += audit.nodraw;
        totals.patch_failed += audit.patch_failed;
        totals.empty += audit.empty;
        totals.out_of_range += audit.out_of_range;
        totals.entities += audit.entities;
        totals.player_spawns += audit.player_spawns;
        totals.items += audit.items;
    }

    println!(
        "{} maps: {} rooms, {} faces ({} patch), {} triangles ({} tessellated), {} entities ({} player spawns, {} items)",
        maps.len(),
        totals.rooms,
        totals.faces,
        totals.patch_faces,
        totals.triangles,
        totals.patch_triangles,
        totals.entities,
        totals.player_spawns,
        totals.items
    );
    println!(
        "skipped on purpose or by fault: {} nodraw, {} patch failed, {} empty, {} out of range; {} maps failed",
        totals.nodraw, totals.patch_failed, totals.empty, totals.out_of_range, failed
    );
    println!(
        "surface shaders: {shader_names} distinct names, {missing_shaders} neither a script nor an image"
    );
    println!("map sounds: {map_sounds} distinct names, {missing_sounds} not in any pak");

    // The pickups' own models: the client draws these in place of the marker
    // boxes, so each one has to parse and every surface has to find its art.
    let mut parsed = 0usize;
    let mut unresolved = 0usize;
    let mut broken = 0usize;
    for index in 1..BG_NUM_ITEMS {
        let Some(path) = bg_misc::world_model(index) else {
            continue;
        };
        let Some(bytes) = assets.read(path) else {
            eprintln!("{path}: not in any pak");
            broken += 1;
            continue;
        };
        match Md3Model::load(&bytes) {
            Ok(model) => {
                parsed += 1;
                for surface in &model.surfaces {
                    for shader in &surface.shaders {
                        if shader.name.is_empty() {
                            continue;
                        }
                        used_shaders.insert(strip_extension(&shader.name).to_string());
                        if resolve(&assets, &shader_text, &shader.name) == Resolution::Missing {
                            eprintln!(
                                "{path}: shader {} is neither a script nor an image",
                                shader.name
                            );
                            unresolved += 1;
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("{path}: {e:?}");
                broken += 1;
            }
        }
    }
    println!(
        "item models: {parser} of {count} models parse, {unresolved} surfaces with unresolved shaders, {broken} broken",
        parser = parsed,
        count = BG_NUM_ITEMS - 1,
    );

    // Second pass over the item models: report any surface whose shader begins
    // with "maps/" even when the surface did find a texture. The C renderer's
    // default shader path falls back to "maps/<shader>.tga" only when the
    // shader has no parsed art, so a map texture used as a model shader is a
    // real mismatch worth surfacing on its own.
    let mut map_shaders = 0usize;
    for index in 1..BG_NUM_ITEMS {
        let Some(path) = bg_misc::world_model(index) else {
            continue;
        };
        let Some(bytes) = assets.read(path) else {
            continue;
        };
        if let Ok(model) = Md3Model::load(&bytes) {
            for surface in &model.surfaces {
                for shader in &surface.shaders {
                    if shader.name.starts_with("maps/") {
                        eprintln!("{path}: surface {} uses map shader {}", surface.name, shader.name);
                        map_shaders += 1;
                    }
                }
            }
        }
    }
    if map_shaders > 0 {
        println!("item models: {map_shaders} surfaces reference map shaders");
    } else {
        println!("item models: no surfaces reference map shaders");
    }

    // Player models: every directory with a lower.md3 or an animation table is
    // a playable model, so it has to carry its three body parts, and every
    // .skin beside them pairs a surface with the art that draws it.
    let players = audit_player_models(&assets, &shader_text, &mut used_shaders);
    println!(
        "player models: {} models ({} animation.cfg), {} parts parse, {} skins, {} unresolved skin shaders, {} broken",
        players.models,
        players.configs,
        players.parts,
        players.skins,
        players.unresolved,
        players.broken
    );

    // The shader scripts themselves: every stage that draws a file names an
    // image, and the renderer falls back to its default when that image is
    // missing, so a script can look fine while its stages draw nothing.
    let stages = audit_shader_stages(&assets, &used_shaders, &shader_text);
    println!(
        "shader stages: {} used shaders parse ({} fail), {} stages, {} images, {} missing from the paks",
        stages.shaders, stages.failed, stages.stages, stages.images, stages.missing
    );

    // Only a surface shader name that resolves to nothing is the dangling
    // reference `--strict` has always meant. The retail data ships stale stage
    // art and team-skin names on purpose or by neglect, and the renderer falls
    // back to its default art for each, so those stay informational.
    let dangling = missing_shaders + unresolved;
    if dangling > 0 && !strict {
        println!("dangling shaders: {dangling} (pass --strict to fail on them)");
    }

    if failed == 0 && players.broken == 0 && (!strict || dangling == 0) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// What the player-model pass found. `broken` covers a model that cannot be
/// drawn at all (a missing part or animation table, an MD3 that does not parse,
/// an unreadable skin); `unresolved` is a skin that names art nothing provides.
#[derive(Default)]
struct PlayerModelAudit {
    models: usize,
    configs: usize,
    parts: usize,
    skins: usize,
    unresolved: usize,
    broken: usize,
}

/// Every `models/players/<model>/` that holds a `lower.md3` or an
/// `animation.cfg`. The texture-only directories beside them (`light/`,
/// `medium/`) carry neither and are not models.
fn player_model_directories(assets: &Assets) -> Vec<String> {
    const PREFIX: &str = "models/players/";
    let mut directories = BTreeSet::new();
    for path in assets.files(PREFIX, ".md3") {
        let Some(rest) = path.strip_prefix(PREFIX) else {
            continue;
        };
        if let Some((directory, _)) = rest.split_once('/') {
            directories.insert(directory.to_string());
        }
    }
    directories.into_iter().collect()
}

/// Audits the stock player models. `R_RegisterMD3` resolves each surface's
/// shader, and a `.skin` file overrides those names per surface; both have to
/// name art an image or a shader script provides.
fn audit_player_models(
    assets: &Assets,
    index: &ShaderTextIndex,
    used: &mut BTreeSet<String>,
) -> PlayerModelAudit {
    let mut audit = PlayerModelAudit::default();

    for directory in player_model_directories(assets) {
        let prefix = format!("models/players/{directory}/");
        if !assets.exists(&format!("{prefix}lower.md3"))
            && !assets.exists(&format!("{prefix}animation.cfg"))
        {
            continue;
        }
        audit.models += 1;

        if assets.exists(&format!("{prefix}animation.cfg")) {
            audit.configs += 1;
        } else {
            eprintln!("{prefix}animation.cfg: missing");
            audit.broken += 1;
        }
        for part in ["lower.md3", "upper.md3", "head.md3"] {
            if !assets.exists(&format!("{prefix}{part}")) {
                eprintln!("{prefix}{part}: missing");
                audit.broken += 1;
            }
        }

        let mut parts = assets.files(&prefix, ".md3");
        parts.sort();
        for part in parts {
            let Some(bytes) = assets.read(&part) else {
                eprintln!("{part}: unreadable");
                audit.broken += 1;
                continue;
            };
            match Md3Model::load(&bytes) {
                Ok(model) => {
                    audit.parts += 1;
                    for surface in &model.surfaces {
                        for shader in &surface.shaders {
                            // An empty name asks for the default shader, which
                            // the renderer supplies without a lookup.
                            if shader.name.is_empty() {
                                continue;
                            }
                            used.insert(strip_extension(&shader.name).to_string());
                            if resolve(assets, index, &shader.name) == Resolution::Missing {
                                eprintln!(
                                    "{part}: shader {} is neither a script nor an image",
                                    shader.name
                                );
                                audit.unresolved += 1;
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{part}: {e:?}");
                    audit.broken += 1;
                }
            }
        }

        let mut skins = assets.files(&prefix, ".skin");
        skins.sort();
        for skin in skins {
            let Some(bytes) = assets.read(&skin) else {
                eprintln!("{skin}: unreadable");
                audit.broken += 1;
                continue;
            };
            audit.skins += 1;
            let text = String::from_utf8_lossy(&bytes);
            for (surface, shader) in skin_pairs(&text) {
                // A `tag_*` line pairs a tag with an empty shader; it selects
                // where a weapon attaches, not what is drawn.
                if shader.is_empty() {
                    continue;
                }
                used.insert(strip_extension(&shader).to_string());
                if resolve(assets, index, &shader) == Resolution::Missing {
                    eprintln!("{skin}: surface {surface} shader {shader} is neither a script nor an image");
                    audit.unresolved += 1;
                }
            }
        }
    }

    audit
}

/// `CommaParse`'s pairs from a `.skin` file: surface and shader separated by a
/// comma or a newline, with `//` comments. The C reads the whole file as one
/// token stream, so a line ending in a comma continues the pair list.
fn skin_pairs(text: &str) -> Vec<(String, String)> {
    let mut tokens = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        for token in line.split(',') {
            tokens.push(token.trim().to_string());
        }
    }
    tokens
        .chunks(2)
        .filter(|pair| pair.len() == 2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect()
}

/// What the shader-script pass found.
#[derive(Default)]
struct ShaderStageAudit {
    shaders: usize,
    stages: usize,
    images: usize,
    missing: usize,
    failed: usize,
}

/// Parses each used shader definition and checks the images its active stages
/// draw. This looks past the shader's own name: a map can ask for a shader
/// that resolves, while a stage in it points at art that is not in any pak.
fn audit_shader_stages(
    assets: &Assets,
    used: &BTreeSet<String>,
    index: &ShaderTextIndex,
) -> ShaderStageAudit {
    let mut audit = ShaderStageAudit::default();
    for name in used {
        let Some(text) = index.find(&name) else {
            continue;
        };
        let mut script = ShaderScript::default();
        script.name = name.clone();
        let mut parser = Parser::new(text);
        if let Err(e) = parser.parse_shader(&mut script) {
            eprintln!("shader {name}: {e:?}");
            audit.failed += 1;
            continue;
        }
        audit.shaders += 1;
        for stage in script.stages.iter().filter(|stage| stage.active) {
            audit.stages += 1;
            for image in stage_images(stage) {
                audit.images += 1;
                if !texture_present(assets, image) {
                    eprintln!("shader {name}: stage image {image} is not in any pak");
                    audit.missing += 1;
                }
            }
        }
    }

    audit
}

/// Every image the stage draws, in file order. `$lightmap`, `$whiteimage` and
/// video maps have nothing to find in a pak.
fn stage_images(stage: &ShaderStage) -> Vec<&str> {
    match stage.bundle.map.as_ref() {
        Some(TextureMap::File { name, .. }) => vec![name.as_str()],
        Some(TextureMap::Anim { frames, .. }) => frames.iter().map(String::as_str).collect(),
        _ => Vec::new(),
    }
}

/// The `noise` keys of a map's entities as sound paths. `SP_target_speaker`
/// appends `.wav` unless the name already has it, and a leading `*` marks a
/// player-specific sound that the client resolves from the player model.
fn map_sound_names(bsp: &Bsp) -> Vec<String> {
    let mut names: Vec<String> = entities::parse(bsp.entities())
        .iter()
        .filter_map(|entity| entity.get("noise"))
        .filter_map(sound_name)
        .collect();
    names.sort();
    names.dedup();
    names
}

fn sound_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('*') {
        return None;
    }
    if value.contains(".wav") {
        Some(value.to_string())
    } else {
        Some(format!("{value}.wav"))
    }
}

/// The paks are laid out in lowercase, but an entity string is hand-edited
/// and may not be; the engine's filesystem lookup is case-insensitive.
fn sound_present(assets: &Assets, name: &str) -> bool {
    if assets.exists(name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    lower != name && assets.exists(&lower)
}

/// The `scripts/*.shader` files, as text, in pak order.
fn shader_scripts(assets: &Assets) -> Vec<String> {
    assets
        .files("scripts/", ".shader")
        .into_iter()
        .filter_map(|path| assets.read(&path))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect()
}

/// How a name a surface asks for resolves. `R_FindShader` takes the shader
/// script's definition when one claims the name and only then loads it as an
/// image, so a name that is neither is a surface nothing can draw.
#[derive(PartialEq, Eq)]
enum Resolution {
    /// A `scripts/*.shader` block defines it.
    Shader,
    /// A pak holds it as an image.
    Image,
    /// Neither: the renderer would fall back to its default shader.
    Missing,
}

/// `COM_StripExtension`: the name without its file extension. `R_FindShader`
/// strips it before looking anything up, because an MD3 stores
/// `models/x/armor.tga` while the script that claims it is labelled
/// `models/x/armor`.
fn strip_extension(name: &str) -> &str {
    match (name.rfind('.'), name.rfind('/')) {
        (Some(dot), slash) if slash.map_or(true, |slash| dot > slash) => &name[..dot],
        _ => name,
    }
}

fn resolve(assets: &Assets, index: &ShaderTextIndex, name: &str) -> Resolution {
    let name = strip_extension(name);
    if index.find(name).is_some() {
        Resolution::Shader
    } else if texture_present(assets, name) {
        Resolution::Image
    } else {
        Resolution::Missing
    }
}

/// The distinct shader names the map's faces reference. `ShaderForShaderNum`
/// resolves a name per face, so a lump entry no face points at is never looked
/// up and is not a dangling reference.
fn surface_shader_names(bsp: &Bsp) -> Vec<String> {
    let (Ok(textures), Ok(faces)) = (bsp.textures(), bsp.faces()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = faces
        .iter()
        .filter_map(|face| textures.get(face.shader))
        .map(|texture| texture.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether a name resolves to an image, with the `.tga`→`.jpg` fallback
/// the renderer uses: this installation ships some of those as JPEGs.
fn texture_present(assets: &Assets, name: &str) -> bool {
    if assets.read(name).is_some() {
        return true;
    }
    if name.to_ascii_lowercase().ends_with(".tga") {
        let mut jpeg = name.to_string();
        jpeg.replace_range(name.len() - 3.., "jpg");
        if assets.read(&jpeg).is_some() {
            return true;
        }
    }
    assets.read(&format!("{name}.tga")).is_some() || assets.read(&format!("{name}.jpg")).is_some()
}

/// The skip counts behind a map's numbers, as one parenthesised tail.
fn skipped_notes(audit: &MapAudit) -> String {
    let mut notes = Vec::new();
    if audit.nodraw > 0 {
        notes.push(format!("{} nodraw", audit.nodraw));
    }
    if audit.patch_failed > 0 {
        notes.push(format!("{} patch failed", audit.patch_failed));
    }
    if audit.empty > 0 {
        notes.push(format!("{} empty", audit.empty));
    }
    if audit.out_of_range > 0 {
        notes.push(format!("{} out of range", audit.out_of_range));
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skin_pairs_follow_comma_parse() {
        let pairs = skin_pairs(
            "// comment\r\n\
             h_head,models/players/sarge/head_default.tga\r\n\
             tag_head,\r\n\
             \r\n\
             l_legs,models/players/sarge/lower_default.tga\r\n",
        );
        assert_eq!(
            pairs,
            vec![
                (
                    "h_head".to_string(),
                    "models/players/sarge/head_default.tga".to_string()
                ),
                ("tag_head".to_string(), String::new()),
                (
                    "l_legs".to_string(),
                    "models/players/sarge/lower_default.tga".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_speaker_names_its_wav() {
        assert_eq!(
            sound_name("sound/world/jumppad"),
            Some("sound/world/jumppad.wav".to_string())
        );
        assert_eq!(
            sound_name("sound/world/jumppad.wav"),
            Some("sound/world/jumppad.wav".to_string())
        );
        // `SP_target_speaker` leaves player-specific sounds to the client,
        // which resolves them from the player model.
        assert_eq!(sound_name("*falling1.wav"), None);
        assert_eq!(sound_name("   "), None);
    }
}
