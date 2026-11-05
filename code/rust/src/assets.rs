//! Game data: the `baseq3` paks, mounted through the pak reader.
//!
//! `FS_InitFilesystem` builds a search path from the game directory's `*.pk3`
//! files, highest pak first, so an update pak shadows the base one. This does
//! the same for one directory, and exposes reads by Quake path
//! (`gfx/2d/bigchars.tga`), which is what the media loaders ask for.
//!
//! The paks are mapped rather than read: a stock `pak0.pk3` is 479 MB, and
//! nothing is paged in until a lookup actually decodes a file.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::mmap::Mapping;
use crate::pak::Pak;

/// Mounted pak files, in search order.
#[derive(Debug)]
pub struct Assets {
    paks: Vec<Pak<'static>>,
    names: Vec<String>,
    /// Paks that could not be opened, kept for the console.
    errors: Vec<String>,
    bytes: u64,
    /// The directory the paks came from, for the console.
    directory: PathBuf,
}

impl Assets {
    /// Mounts every `*.pk3` in `directory`, highest name first.
    ///
    /// A pak that will not open is recorded in [`Assets::errors`] and skipped:
    /// a broken third-party pak should not stop the game from starting, which
    /// is how `FS_AddGameDirectory` behaves.
    pub fn mount(directory: &Path) -> Result<Self, String> {
        let entries = std::fs::read_dir(directory)
            .map_err(|e| format!("reading {}: {e}", directory.display()))?;

        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let is_pak = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pk3"));
            if is_pak && path.is_file() {
                paths.push(path);
            }
        }
        // Reverse name order: pak8 shadows pak0, as the C search path does.
        paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

        let mut assets = Assets {
            paks: Vec::new(),
            names: Vec::new(),
            errors: Vec::new(),
            bytes: 0,
            directory: directory.to_path_buf(),
        };

        for path in paths {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("?")
                .to_string();
            let opened = match Mapping::open(&path) {
                Ok(mapping) => {
                    let bytes = mapping.len() as u64;
                    match Pak::open(mapping.leak()) {
                        Ok(pak) => Ok((pak, bytes)),
                        Err(e) => Err(format!("{name}: {e:?}")),
                    }
                }
                Err(e) => Err(format!("{name}: {e}")),
            };
            match opened {
                Ok((pak, bytes)) => {
                    assets.bytes += bytes;
                    assets.paks.push(pak);
                    assets.names.push(name);
                }
                Err(e) => assets.errors.push(e),
            }
        }

        if assets.paks.is_empty() {
            return Err(format!(
                "no usable pk3 files in {}{}",
                directory.display(),
                if assets.errors.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", assets.errors.join("; "))
                }
            ));
        }
        Ok(assets)
    }

    /// The stock install location: `$Q3_BASEQ3` if set, else
    /// `$HOME/.q3a/baseq3`.
    pub fn default_directory() -> PathBuf {
        if let Some(directory) = std::env::var_os("Q3_BASEQ3") {
            return PathBuf::from(directory);
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".q3a").join("baseq3")
    }

    /// Every place a stock install may live, most specific first: an explicit
    /// override, the classic home directory, the XDG data directory, the
    /// distribution install roots where `game-data-packager` output lands, and
    /// the Steam library a player may already own the game in. Nothing here is
    /// written to, and no asset is ever copied into the tree.
    pub fn candidates() -> Vec<PathBuf> {
        let mut found = Vec::new();
        if let Some(directory) = std::env::var_os("Q3_BASEQ3") {
            found.push(PathBuf::from(directory));
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        found.push(home.join(".q3a").join("baseq3"));

        let xdg = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share"));
        found.push(xdg.join("quake3").join("baseq3"));

        for root in [
            "/usr/share/games/quake3",
            "/usr/share/quake3",
            "/usr/local/share/games/quake3",
        ] {
            found.push(Path::new(root).join("baseq3"));
        }

        for library in [
            home.join(".steam")
                .join("steam")
                .join("steamapps")
                .join("common"),
            home.join(".local")
                .join("share")
                .join("Steam")
                .join("steamapps")
                .join("common"),
        ] {
            found.push(library.join("Quake 3 Arena").join("baseq3"));
        }
        found
    }

    /// The first candidate that actually holds a pak. A directory that exists
    /// but has none (an empty `~/.q3a`, say) is skipped rather than reported.
    pub fn discover_in(candidates: &[PathBuf]) -> Result<PathBuf, String> {
        for directory in candidates {
            if has_pak(directory) {
                return Ok(directory.clone());
            }
        }
        let tried: Vec<String> = candidates
            .iter()
            .map(|directory| directory.display().to_string())
            .collect();
        Err(format!(
            "no baseq3 holding a pak was found; looked in {}",
            tried.join(", ")
        ))
    }

    /// Mounts the install the player already has. `$Q3_BASEQ3` is an explicit
    /// choice, so when it is set it is used as-is and a failure is reported
    /// instead of silently falling back to another copy. Otherwise the first
    /// of [`Assets::candidates`] that holds a pak is mounted.
    pub fn mount_default() -> Result<Self, String> {
        if let Some(directory) = std::env::var_os("Q3_BASEQ3") {
            return Assets::mount(Path::new(&directory));
        }
        let directory = Assets::discover_in(&Assets::candidates())?;
        Assets::mount(&directory)
    }

    /// The directory the mounted paks came from.
    pub fn source_directory(&self) -> &Path {
        &self.directory
    }

    /// Every mounted file whose path starts with `prefix` and ends with
    /// `suffix`, first pak wins and in pak order. This stands in for the
    /// directory listing the C uses to find `scripts/*.shader`.
    pub fn files(&self, prefix: &str, suffix: &str) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        for pak in &self.paks {
            for name in pak.entry_names() {
                if name.starts_with(prefix)
                    && name.ends_with(suffix)
                    && !found.iter().any(|seen| seen == name)
                {
                    found.push(name.to_string());
                }
            }
        }
        found
    }

    /// Reads a file by Quake path, first pak wins. `FS_ReadFile` strips one
    /// leading separator and takes backslashes as separators, so this does too.
    ///
    /// A stored entry borrows from the mapping; a deflated one is inflated, so
    /// callers get a `Cow` and a stock pak's files stay zero-copy only when
    /// they were stored. `Pak::open` runs on a leaked mapping, hence `'static`.
    pub fn read(&self, qpath: &str) -> Option<Cow<'static, [u8]>> {
        let path = normalise(qpath);
        for pak in &self.paks {
            if let Ok(bytes) = pak.read_file(&path) {
                return Some(bytes);
            }
        }
        None
    }

    pub fn exists(&self, qpath: &str) -> bool {
        let path = normalise(qpath);
        self.paks.iter().any(|pak| pak.find(&path).is_some())
    }

    /// Paks that were found but not mounted.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn len(&self) -> usize {
        self.paks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paks.is_empty()
    }

    /// Total size of the mounted paks, in bytes.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Whether `directory` holds at least one pak. Discovery uses this rather
/// than mounting, so a broken install is not opened twice.
fn has_pak(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pk3"))
    })
}

/// Strips leading separators and turns backslashes into slashes.
fn normalise(qpath: &str) -> String {
    qpath
        .trim_start_matches(['/', '\\'])
        .replace('\\', "/")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pak::tests::build_zip;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("q3rust-assets-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn higher_paks_shadow_lower_ones() {
        let dir = temp_dir("shadow");
        std::fs::write(
            dir.join("pak0.pk3"),
            build_zip(b"", &[("gfx/2d/bigchars.tga", b"base", 0)]),
        )
        .unwrap();
        std::fs::write(
            dir.join("pak1.pk3"),
            build_zip(b"", &[("gfx/2d/bigchars.tga", b"update", 0)]),
        )
        .unwrap();

        let assets = Assets::mount(&dir).unwrap();
        assert_eq!(assets.len(), 2);
        assert_eq!(assets.names(), ["pak1.pk3", "pak0.pk3"]);
        assert_eq!(
            assets.read("gfx/2d/bigchars.tga").as_deref(),
            Some(&b"update"[..])
        );
        // A file only the base pak has is still found.
        assert!(assets.errors().is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn queries_are_sanitised_like_fs_read_file() {
        let dir = temp_dir("paths");
        std::fs::write(
            dir.join("pak0.pk3"),
            build_zip(b"", &[("scripts/base.shader", b"// shaders", 0)]),
        )
        .unwrap();

        let assets = Assets::mount(&dir).unwrap();
        assert!(assets.read("/scripts/base.shader").is_some());
        assert!(assets.read("\\scripts\\base.shader").is_some());
        assert!(assets.read("SCRIPTS/BASE.SHADER").is_some());
        assert!(assets.exists("scripts/base.shader"));
        assert!(!assets.exists("scripts/missing.shader"));
        assert_eq!(assets.read("scripts/missing.shader"), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_pak_is_reported_and_skipped() {
        let dir = temp_dir("broken");
        std::fs::write(dir.join("pak0.pk3"), build_zip(b"", &[("a.txt", b"ok", 0)])).unwrap();
        std::fs::write(dir.join("pak1.pk3"), b"not a zip at all").unwrap();

        let assets = Assets::mount(&dir).unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets.errors().len(), 1);
        assert!(assets.errors()[0].starts_with("pak1.pk3"), "{:?}", assets.errors());
        assert_eq!(assets.read("a.txt").as_deref(), Some(&b"ok"[..]));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_without_paks_is_an_error() {
        let dir = temp_dir("empty");
        std::fs::write(dir.join("readme.txt"), b"nothing here").unwrap();
        let err = Assets::mount(&dir).unwrap_err();
        assert!(err.contains("no usable pk3 files"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(Assets::mount(Path::new("/nonexistent/q3rust/baseq3")).is_err());
    }

    #[test]
    fn discovery_skips_directories_without_paks() {
        let empty = temp_dir("discover-empty");
        let full = temp_dir("discover-full");
        std::fs::write(
            full.join("pak0.pk3"),
            build_zip(b"", &[("a.txt", b"ok", 0)]),
        )
        .unwrap();

        let candidates = vec![empty.clone(), full.clone()];
        assert_eq!(Assets::discover_in(&candidates).unwrap(), full);

        std::fs::remove_dir_all(&empty).unwrap();
        std::fs::remove_dir_all(&full).unwrap();
    }

    #[test]
    fn discovery_reports_every_place_it_looked() {
        let missing = std::env::temp_dir().join("q3rust-assets-nowhere");
        let err = Assets::discover_in(&[missing.clone()]).unwrap_err();
        assert!(err.contains("no baseq3 holding a pak"), "{err}");
        assert!(err.contains(&missing.display().to_string()), "{err}");
    }

    #[test]
    fn the_mounted_directory_is_recorded() {
        let dir = temp_dir("source");
        std::fs::write(
            dir.join("pak0.pk3"),
            build_zip(b"", &[("a.txt", b"ok", 0)]),
        )
        .unwrap();

        let assets = Assets::mount(&dir).unwrap();
        assert_eq!(assets.source_directory(), dir.as_path());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_default_directory_follows_the_environment() {
        // `$Q3_BASEQ3` wins when it is set; this is how the client is pointed
        // at an install that is not in the home directory.
        let saved = std::env::var_os("Q3_BASEQ3");
        std::env::set_var("Q3_BASEQ3", "/tmp/q3a-baseq3");
        assert_eq!(Assets::default_directory(), PathBuf::from("/tmp/q3a-baseq3"));
        match saved {
            Some(value) => std::env::set_var("Q3_BASEQ3", value),
            None => std::env::remove_var("Q3_BASEQ3"),
        }

        std::env::remove_var("Q3_BASEQ3");
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        assert_eq!(
            Assets::default_directory(),
            home.join(".q3a").join("baseq3")
        );
    }
}
