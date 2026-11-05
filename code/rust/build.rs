//! Compiles the renderer's GLSL to SPIR-V at build time.
//!
//! The shaders are Vulkan-only GLSL (`glslc --target-env=vulkan1.3`), so the
//! renderer never needs a shader compiler at runtime and the game ships the
//! same bytes that were built here.

use std::env;
use std::path::PathBuf;
use std::process::Command;

const SHADERS: &[(&str, &str)] = &[("q3_2d.vert", "vert"), ("q3_2d.frag", "frag")];

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    println!("cargo:rerun-if-changed=shaders");

    for (name, _) in SHADERS {
        println!("cargo:rerun-if-changed=shaders/{name}");
    }

    if env::var_os("CARGO_FEATURE_NO_SHADERS").is_some() {
        return;
    }

    for (name, _) in SHADERS {
        let source = PathBuf::from("shaders").join(name);
        let output = out_dir.join(format!("{name}.spv"));

        // Shaders are only needed by the Vulkan renderer, and that is not part
        // of every build of this crate (tests, the C-free utility binaries).
        // A missing glslc therefore warns instead of failing the build.
        match Command::new("glslc")
            .arg("--target-env=vulkan1.3")
            .arg("-O")
            .arg(&source)
            .arg("-o")
            .arg(&output)
            .output()
        {
            Ok(result) if result.status.success() => {}
            Ok(result) => {
                let stderr = String::from_utf8_lossy(&result.stderr);
                panic!("glslc failed for {}:\n{stderr}", source.display());
            }
            Err(e) => {
                println!(
                    "cargo:warning=glslc not found ({e}); the Vulkan renderer needs it to build shaders"
                );
            }
        }
    }
}
