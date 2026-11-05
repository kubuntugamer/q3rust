//! Headless game-data check: what the mounted paks contain, and whether every
//! map in them parses and tessellates. No window, no Vulkan, so this is safe to
//! run over SSH or on a machine whose display is in use.

use std::process::ExitCode;

use q3rust::assets::Assets;
use q3rust::world;

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

    let mut maps = assets.files("maps/", ".bsp");
    maps.sort();
    if maps.is_empty() {
        eprintln!("no maps/*.bsp in the mounted paks");
        return ExitCode::FAILURE;
    }

    let mut totals = world::MapAudit::default();
    let mut failed = 0usize;
    for map in &maps {
        let Some(bytes) = assets.read(map) else {
            eprintln!("{map}: unreadable");
            failed += 1;
            continue;
        };
        match world::audit_map(bytes.as_ref()) {
            Ok(audit) => {
                let mut notes = Vec::new();
                if audit.nodraw > 0 {
                    notes.push(format!("{} nodraw", audit.nodraw));
                }
                if audit.patch_failed > 0 {
                    notes.push(format!("{} patches failed", audit.patch_failed));
                }
                if audit.empty > 0 {
                    notes.push(format!("{} empty", audit.empty));
                }
                if audit.out_of_range > 0 {
                    notes.push(format!("{} out of range", audit.out_of_range));
                }
                println!(
                    "{map:<30} {:>5} faces {:>4} patches {:>7} tris ({:>7} tessellated){}",
                    audit.faces,
                    audit.patches,
                    audit.triangles,
                    audit.patch_triangles,
                    if notes.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", notes.join(", "))
                    }
                );
                totals.faces += audit.faces;
                totals.patches += audit.patches;
                totals.triangles += audit.triangles;
                totals.patch_triangles += audit.patch_triangles;
                totals.nodraw += audit.nodraw;
                totals.patch_failed += audit.patch_failed;
                totals.empty += audit.empty;
                totals.out_of_range += audit.out_of_range;
            }
            Err(e) => {
                eprintln!("{map}: {e}");
                failed += 1;
            }
        }
    }

    println!(
        "{} maps: {} faces ({} patches), {} triangles ({} tessellated), \
         {} nodraw, {} patches failed, {} empty, {} out of range, {} maps failed",
        maps.len(),
        totals.faces,
        totals.patches,
        totals.triangles,
        totals.patch_triangles,
        totals.nodraw,
        totals.patch_failed,
        totals.empty,
        totals.out_of_range,
        failed
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
