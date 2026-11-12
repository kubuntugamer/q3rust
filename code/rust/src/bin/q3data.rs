//! Headless map-data check: what the mounted paks contain, and whether every map
//! in them loads, counts its rooms and surfaces, and parses its entity list. No
//! window and no Vulkan, so it is safe to run over SSH or on a machine whose
//! display is in use.

use std::process::ExitCode;

use q3rust::assets::Assets;
use q3rust::bg_misc::{self, BG_NUM_ITEMS};
use q3rust::bsp::Bsp;
use q3rust::model::Md3Model;
use q3rust::shader::ShaderTextIndex;
use q3rust::world::{self, MapAudit};

fn main() -> ExitCode {
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
        for name in names {
            if resolve(&assets, &shader_text, &name) == Resolution::Missing {
                eprintln!("{map}: shader {name} is neither a script nor an image");
                missing_shaders += 1;
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

    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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
