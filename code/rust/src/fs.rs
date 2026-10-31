//! Filesystem search path, ported from `FS_*` in `code/qcommon/files.c`.
//!
//! The engine keeps an ordered list of search paths, each either a pak or a
//! directory, and walks it until the file turns up. This module does the
//! same, with directories behind the [`DirSource`] trait so nothing here
//! touches the operating system:
//!
//! - [`hash_file_name`] is `FS_HashFileName`, the hash `FS_LoadZipFile`
//!   buckets pak entries by;
//! - [`filename_compare`] is `FS_FilenameCompare`, which ignores case and
//!   treats `\`, `:` and `/` as the same separator;
//! - [`FileSystem::read`] sanitises the query like `FS_FOpenFileRead` does
//!   (one leading separator is stripped, `..` and `::` are refused) and
//!   returns the bytes, or the length via [`FileSystem::exists`].
//!
//! Not ported: file handles and streaming, pure-server pak checks, pak
//! reference marking, the journal, and directory listing.

use crate::pak::{Entry, Pak, PakError};

/// Hash table size used by `FS_LoadZipFile` for pak entries.
const MAX_FILEHASH_SIZE: usize = 1024;

/// Why a file could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// The path was empty, which `FS_ReadFile` treats as fatal.
    EmptyName,
    /// The path contains `..` or `::`, or is otherwise not usable.
    IllegalPath,
    /// No search path holds the file.
    NotFound,
    /// The file was in a pak but could not be read from it.
    Pak(PakError),
}

/// A directory-backed search path. `name` uses forward slashes; the
/// implementation decides how to turn that into an OS path.
pub trait DirSource {
    /// Reads the whole file, or `None` when it does not exist.
    fn read(&self, name: &str) -> Option<Vec<u8>>;

    /// True when the file exists. The default asks [`DirSource::read`];
    /// an implementation that can stat cheaply should override it.
    fn exists(&self, name: &str) -> bool {
        self.read(name).is_some()
    }
}

/// Where a file was found.
#[derive(Clone, Copy)]
pub enum Source<'a> {
    /// Inside a pak: the archive and the central directory entry.
    Pack { pak: &'a Pak<'a>, entry: &'a Entry },
    /// Loose file in a directory search path.
    Dir(&'a dyn DirSource),
}

impl<'a> std::fmt::Debug for Source<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Pack { entry, .. } => write!(f, "Pack({:?})", entry.name()),
            Source::Dir(_) => write!(f, "Dir"),
        }
    }
}

/// A pak with its entries bucketed by [`hash_file_name`], the layout
/// `FS_LoadZipFile` builds with `hashTable`.
struct PackPath<'a> {
    pak: &'a Pak<'a>,
    buckets: Vec<Vec<&'a Entry>>,
}

impl<'a> PackPath<'a> {
    fn new(pak: &'a Pak<'a>) -> Self {
        // Same size rule as FS_LoadZipFile: the smallest power of two above
        // the file count, grown while it stays within the limit.
        let mut hash_size = 1usize;
        while hash_size <= MAX_FILEHASH_SIZE {
            if hash_size > pak.len() {
                break;
            }
            hash_size <<= 1;
        }

        let mut buckets = vec![Vec::new(); hash_size];
        for entry in pak.entries() {
            let index = hash_file_name(entry.name(), hash_size);
            buckets[index].push(entry);
        }
        PackPath { pak, buckets }
    }

    fn find(&self, name: &str) -> Option<(&'a Pak<'a>, &'a Entry)> {
        let bucket = &self.buckets[hash_file_name(name, self.buckets.len())];
        bucket
            .iter()
            .find(|entry| filename_compare(entry.name(), name))
            .copied()
            .map(|entry| (self.pak, entry))
    }
}

/// An ordered search path list, the stand-in for `fs_searchpaths`.
#[derive(Default)]
pub struct FileSystem<'a> {
    paths: Vec<PathEntry<'a>>,
}

enum PathEntry<'a> {
    Pack(PackPath<'a>),
    Dir(&'a dyn DirSource),
}

impl<'a> std::fmt::Debug for FileSystem<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSystem")
            .field("paths", &self.paths.len())
            .finish()
    }
}

impl<'a> FileSystem<'a> {
    /// An empty search path list, as before `FS_Startup`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a pak. Search order is the order of these calls.
    pub fn push_pack(&mut self, pak: &'a Pak<'a>) {
        self.paths.push(PathEntry::Pack(PackPath::new(pak)));
    }

    /// Appends a directory. Search order is the order of these calls.
    pub fn push_dir(&mut self, dir: &'a dyn DirSource) {
        self.paths.push(PathEntry::Dir(dir));
    }

    /// Number of search paths.
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// True when nothing can be found because no search path is registered.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Walks the search paths and reports where `qpath` lives, the walk
    /// `FS_FOpenFileRead` performs. Illegal paths simply do not match.
    pub fn locate(&self, qpath: &str) -> Option<Source<'a>> {
        let name = sanitize(qpath).ok()?;
        for path in &self.paths {
            match path {
                PathEntry::Pack(pack) => {
                    if let Some((pak, entry)) = pack.find(name) {
                        return Some(Source::Pack { pak, entry });
                    }
                }
                PathEntry::Dir(dir) => {
                    if dir.exists(name) {
                        return Some(Source::Dir(*dir));
                    }
                }
            }
        }
        None
    }

    /// True when the file exists somewhere in the search path, like the
    /// `FS_FOpenFileRead( ..., NULL )` existence probe.
    pub fn exists(&self, qpath: &str) -> bool {
        self.locate(qpath).is_some()
    }

    /// Reads a whole file, as `FS_ReadFile` does without the journal path.
    /// The caller owns the buffer instead of it living on the hunk.
    pub fn read(&self, qpath: &str) -> Result<Vec<u8>, FsError> {
        let name = sanitize(qpath)?;
        match self.locate(name) {
            None => Err(FsError::NotFound),
            Some(Source::Dir(dir)) => dir.read(name).ok_or(FsError::NotFound),
            Some(Source::Pack { pak, entry }) => {
                pak.read(entry).map(<[u8]>::to_vec).map_err(FsError::Pak)
            }
        }
    }
}

/// Strips one leading separator and refuses paths that could escape the
/// search path, matching the checks in `FS_FOpenFileRead`.
fn sanitize(qpath: &str) -> Result<&str, FsError> {
    if qpath.is_empty() {
        return Err(FsError::EmptyName);
    }
    let name = qpath
        .strip_prefix('/')
        .or_else(|| qpath.strip_prefix('\\'))
        .unwrap_or(qpath);
    if name.contains("..") || name.contains("::") {
        return Err(FsError::IllegalPath);
    }
    Ok(name)
}

/// `FS_FilenameCompare`: equal when the names match ignoring ASCII case and
/// treating `\`, `:` and `/` as the same separator.
pub fn filename_compare(s1: &str, s2: &str) -> bool {
    let (a, b) = (s1.as_bytes(), s2.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);

    loop {
        // Reading past the end yields the terminator, so a prefix cannot
        // compare equal, just as the C loop stops at the NUL.
        let c1 = normalize(*a.get(i).unwrap_or(&0));
        let c2 = normalize(*b.get(j).unwrap_or(&0));
        if c1 != c2 {
            return false;
        }
        if c1 == 0 {
            return true;
        }
        i += 1;
        j += 1;
    }
}

fn normalize(c: u8) -> u8 {
    let c = c.to_ascii_uppercase();
    if c == b'\\' || c == b':' {
        b'/'
    } else {
        c
    }
}

/// `FS_HashFileName`: hashes the lowercased path up to the first `.`, with
/// separators folded to `/`, then folds the wide sum down and masks it to
/// `hash_size - 1`. The fold uses 64-bit shifts, matching `long` on the
/// x86_64 Linux build.
///
/// # Panics
///
/// Panics when `hash_size` is not a power of two.
pub fn hash_file_name(fname: &str, hash_size: usize) -> usize {
    assert!(
        hash_size.is_power_of_two(),
        "hash size {hash_size} is not a power of two"
    );

    let mut hash: i64 = 0;
    for (i, letter) in fname.as_bytes().iter().enumerate() {
        let letter = letter.to_ascii_lowercase();
        if letter == b'.' {
            break; // don't include the extension
        }
        // PATH_SEP is '/' on this platform, so only '\\' needs folding.
        let letter = if letter == b'\\' { b'/' } else { letter };
        hash += letter as i64 * (i as i64 + 119);
    }

    hash ^= hash >> 10;
    hash ^= hash >> 20;
    (hash as u64 & (hash_size as u64 - 1)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pak::tests::build_zip;
    use crate::pak::{METHOD_DEFLATE, METHOD_STORED};
    use std::collections::HashMap;

    /// In-memory directory, so the tests never touch the OS.
    #[derive(Default)]
    struct MemDir {
        files: HashMap<String, Vec<u8>>,
    }

    impl MemDir {
        fn with(files: &[(&str, &[u8])]) -> Self {
            let files = files
                .iter()
                .map(|(name, data)| (name.to_string(), data.to_vec()))
                .collect();
            MemDir { files }
        }
    }

    impl DirSource for MemDir {
        fn read(&self, name: &str) -> Option<Vec<u8>> {
            self.files.get(name).cloned()
        }

        fn exists(&self, name: &str) -> bool {
            self.files.contains_key(name)
        }
    }

    #[test]
    fn hash_stops_at_the_extension() {
        let size = 16;
        assert_eq!(
            hash_file_name("scripts/foo.arena", size),
            hash_file_name("scripts/foo.other", size)
        );
        assert_ne!(
            hash_file_name("scripts/foo.arena", size),
            hash_file_name("scripts/bar.arena", size)
        );
    }

    #[test]
    fn hash_folds_case_and_separators() {
        let size = 32;
        assert_eq!(
            hash_file_name("maps\\Q3DM1.BSP", size),
            hash_file_name("MAPS/q3dm1.bsp", size)
        );
    }

    #[test]
    fn hash_stays_inside_the_table() {
        for size in [1usize, 2, 16, 1024] {
            for name in ["", "a", "maps/q3dm1.bsp", "....", "x\\y:z"] {
                assert!(hash_file_name(name, size) < size);
            }
        }
    }

    #[test]
    #[should_panic(expected = "not a power of two")]
    fn hash_rejects_a_bad_table_size() {
        hash_file_name("file", 3);
    }

    #[test]
    fn filename_compare_ignores_case_and_separators() {
        assert!(filename_compare("maps/q3dm1.bsp", "MAPS/q3dm1.bsp"));
        assert!(filename_compare("maps\\q3dm1.bsp", "maps/q3dm1.bsp"));
        assert!(filename_compare("c:/quake3", "C:\\quake3"));
        assert!(filename_compare("", ""));

        assert!(!filename_compare("maps/q3dm1.bsp", "maps/q3dm2.bsp"));
        assert!(!filename_compare("maps", "maps/"));
        assert!(!filename_compare("abc", "ab"));
    }

    #[test]
    fn paths_are_sanitised_like_the_c_check() {
        assert_eq!(sanitize("maps/q3dm1.bsp"), Ok("maps/q3dm1.bsp"));
        assert_eq!(sanitize("/maps/q3dm1.bsp"), Ok("maps/q3dm1.bsp"));
        assert_eq!(sanitize("\\maps\\q3dm1.bsp"), Ok("maps\\q3dm1.bsp"));
        assert_eq!(sanitize(""), Err(FsError::EmptyName));
        assert_eq!(sanitize("../secret"), Err(FsError::IllegalPath));
        assert_eq!(sanitize("maps/../../secret"), Err(FsError::IllegalPath));
        assert_eq!(sanitize("c::x"), Err(FsError::IllegalPath));
    }

    #[test]
    fn reads_from_a_pack_by_case_insensitive_name() {
        let zip = build_zip(
            b"",
            &[
                ("maps/q3dm1.bsp", b"MAPDATA", METHOD_STORED),
                ("scripts/foo.arena", b"ARENA", METHOD_STORED),
            ],
        );
        let pak = Pak::open(&zip).unwrap();
        let mut fs = FileSystem::new();
        fs.push_pack(&pak);

        assert!(fs.exists("MAPS\\Q3DM1.BSP"));
        assert_eq!(fs.read("Maps/Q3DM1.bsp").unwrap(), b"MAPDATA".to_vec());
        assert_eq!(fs.read("/scripts/foo.arena").unwrap(), b"ARENA".to_vec());
        assert_eq!(fs.read("scripts/foo.arena"), Ok(b"ARENA".to_vec()));
        assert!(!fs.exists("maps/q3dm9.bsp"));
        assert_eq!(fs.read("maps/q3dm9.bsp"), Err(FsError::NotFound));
    }

    #[test]
    fn the_first_search_path_wins() {
        let zip = build_zip(b"", &[("shared.txt", b"from pack", METHOD_STORED)]);
        let pak = Pak::open(&zip).unwrap();
        let dir = MemDir::with(&[("shared.txt", b"from dir")]);

        let mut fs = FileSystem::new();
        fs.push_dir(&dir);
        fs.push_pack(&pak);
        assert_eq!(fs.read("shared.txt").unwrap(), b"from dir".to_vec());

        let mut fs = FileSystem::new();
        fs.push_pack(&pak);
        fs.push_dir(&dir);
        assert_eq!(fs.read("shared.txt").unwrap(), b"from pack".to_vec());
    }

    #[test]
    fn later_search_paths_fill_the_gaps() {
        let zip = build_zip(b"", &[("only_in_pack.txt", b"pack", METHOD_STORED)]);
        let pak = Pak::open(&zip).unwrap();
        let dir = MemDir::with(&[("only_in_dir.txt", b"dir")]);

        let mut fs = FileSystem::new();
        fs.push_pack(&pak);
        fs.push_dir(&dir);

        assert_eq!(fs.read("only_in_dir.txt").unwrap(), b"dir".to_vec());
        assert_eq!(fs.read("only_in_pack.txt").unwrap(), b"pack".to_vec());
        assert_eq!(fs.read("nowhere.txt"), Err(FsError::NotFound));
        assert!(!fs.exists("nowhere.txt"));
    }

    #[test]
    fn illegal_paths_are_refused_before_searching() {
        let zip = build_zip(b"", &[("../escape.txt", b"bad", METHOD_STORED)]);
        let pak = Pak::open(&zip).unwrap();
        let mut fs = FileSystem::new();
        fs.push_pack(&pak);

        assert!(!fs.exists("../escape.txt"));
        assert_eq!(fs.read("../escape.txt"), Err(FsError::IllegalPath));
        assert_eq!(fs.read(""), Err(FsError::EmptyName));
    }

    #[test]
    fn deflated_pack_entries_report_the_pak_error() {
        let zip = build_zip(b"", &[("packed.cfg", b"x", METHOD_DEFLATE)]);
        let pak = Pak::open(&zip).unwrap();
        let mut fs = FileSystem::new();
        fs.push_pack(&pak);

        assert_eq!(
            fs.read("packed.cfg"),
            Err(FsError::Pak(PakError::Unsupported("compressed entry")))
        );
    }

    #[test]
    fn an_empty_filesystem_finds_nothing() {
        let fs = FileSystem::new();
        assert!(fs.is_empty());
        assert_eq!(fs.len(), 0);
        assert!(!fs.exists("anything"));
        assert_eq!(fs.read("anything"), Err(FsError::NotFound));
        assert_eq!(format!("{:?}", fs), "FileSystem { paths: 0 }");
    }

    #[test]
    fn bucket_sizes_follow_the_c_rule() {
        let zip = build_zip(
            b"",
            &[
                ("a.txt", b"1", METHOD_STORED),
                ("b.txt", b"2", METHOD_STORED),
                ("c.txt", b"3", METHOD_STORED),
            ],
        );
        let pak = Pak::open(&zip).unwrap();
        let packed = PackPath::new(&pak);
        // Three files: the smallest power of two above 3 is 4.
        assert_eq!(packed.buckets.len(), 4);

        // Files with the same base name land in the same bucket.
        let zip = build_zip(
            b"",
            &[
                ("scripts/foo.arena", b"1", METHOD_STORED),
                ("scripts/foo.menu", b"2", METHOD_STORED),
            ],
        );
        let pak = Pak::open(&zip).unwrap();
        let packed = PackPath::new(&pak);
        assert_eq!(
            hash_file_name("scripts/foo.arena", packed.buckets.len()),
            hash_file_name("scripts/foo.menu", packed.buckets.len())
        );
        assert_eq!(packed.buckets.len(), 4);
    }

    #[test]
    fn pack_lookup_survives_hash_collisions() {
        // Two entries in one bucket, one of them a directory entry.
        let zip = build_zip(
            b"",
            &[
                ("maps/q3dm1.bsp", b"one", METHOD_STORED),
                ("maps\\q3dm1.bsp", b"two", METHOD_STORED),
            ],
        );
        let pak = Pak::open(&zip).unwrap();
        let mut fs = FileSystem::new();
        fs.push_pack(&pak);

        let source = fs.locate("maps/q3dm1.bsp").unwrap();
        match source {
            Source::Pack { entry, .. } => assert_eq!(entry.name(), "maps/q3dm1.bsp"),
            Source::Dir(_) => panic!("expected a pack entry"),
        }
    }
}
