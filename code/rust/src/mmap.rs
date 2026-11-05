//! Read-only memory maps, for game data.
//!
//! `FS_LoadZipFile` reads a whole pak into the heap, and three pak files from a
//! stock install are hundreds of megabytes. Mapping them instead keeps the
//! pages out of memory until a lookup touches them, which is what a pak reader
//! wants: the central directory is read once, and a file's bytes are only
//! resident while it is being decoded.
//!
//! A mapping has no alignment or encoding requirements, so its bytes are handed
//! out as an ordinary `&'static [u8]` by [`Mapping::leak`]: game data lives
//! until the process exits, and munmapping it was never going to happen.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// One read-only mapping of a file.
#[derive(Debug)]
pub struct Mapping {
    start: *mut libc::c_void,
    len: usize,
}

impl Mapping {
    /// Maps `path` read-only. An empty file is refused, since a mapping of
    /// zero bytes is not a mapping.
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path)
            .map_err(|e| format!("opening {}: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("reading the size of {}: {e}", path.display()))?
            .len() as usize;
        if len == 0 {
            return Err(format!("{} is empty", path.display()));
        }

        // SAFETY: a private read-only mapping of a file we hold open; the
        // pointer is only read through, and unmapped in `Drop`.
        let start = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if start == libc::MAP_FAILED {
            return Err(format!(
                "mapping {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(Mapping { start, len })
    }

    /// The mapped bytes.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `start` is a live mapping of `len` readable bytes.
        unsafe { std::slice::from_raw_parts(self.start as *const u8, self.len) }
    }

    /// Gives up the mapping without unmapping it, yielding bytes for the rest
    /// of the process. This is what a pak needs: [`crate::pak::Pak`] borrows
    /// the archive for as long as it lives, and it lives until exit.
    pub fn leak(self) -> &'static [u8] {
        let (start, len) = (self.start, self.len);
        std::mem::forget(self);
        // SAFETY: the mapping is now owned by the process, never unmapped.
        unsafe { std::slice::from_raw_parts(start as *const u8, len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the range came from `mmap` and is unmapped exactly once.
        unsafe {
            libc::munmap(self.start, self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_file_read_only() {
        let path = std::env::temp_dir().join("q3rust-mmap-test.bin");
        std::fs::write(&path, b"pak data").unwrap();

        let mapping = Mapping::open(&path).unwrap();
        assert_eq!(mapping.len(), 8);
        assert_eq!(mapping.bytes(), b"pak data");
        drop(mapping);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn leaked_bytes_outlive_the_mapping() {
        let path = std::env::temp_dir().join("q3rust-mmap-leak.bin");
        std::fs::write(&path, vec![7u8; 4096]).unwrap();

        let bytes: &'static [u8] = Mapping::open(&path).unwrap().leak();
        std::fs::remove_file(&path).unwrap();
        // Still readable after the file is gone: it was mapped, not read.
        assert_eq!(bytes.len(), 4096);
        assert_eq!(bytes[4095], 7);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_and_missing_files_are_refused() {
        let path = std::env::temp_dir().join("q3rust-mmap-empty.bin");
        std::fs::write(&path, b"").unwrap();
        assert!(Mapping::open(&path).is_err());
        std::fs::remove_file(&path).unwrap();

        assert!(Mapping::open(Path::new("/nonexistent/q3rust.pk3")).is_err());
    }
}
