//! Pak (pk3) directory reader, ported from `unzip.c` and `FS_LoadZipFile`
//! in `code/qcommon/files.c`.
//!
//! The engine opens a zip, walks the central directory, lowercases every
//! entry name and keeps the position of its central directory record, so a
//! later `FS_FindFile` can open the file from that position. This module
//! does the same over an in-memory archive:
//!
//! - the end-of-central-directory record is found by scanning backwards,
//!   which tolerates trailing comments and data prepended to the archive;
//! - entry names are stored lowercased and lookups ignore ASCII case;
//! - stored (uncompressed) entries can be read; deflated entries are listed
//!   with their method but reading them returns [`PakError::Unsupported`],
//!   because the inflate code has not been ported.
//!
//! Not supported: ZIP64, multi-disk archives and encrypted entries. The C
//! reader accepts the first two; they are rejected here with a clear error.

const EOCD_SIG: u32 = 0x0605_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;

const EOCD_LEN: usize = 22;
const CENTRAL_LEN: usize = 46;
const LOCAL_LEN: usize = 30;
/// Largest zip comment, used to bound the backwards EOCD scan.
const MAX_COMMENT: usize = 0xffff;

const ZIP64_SENTINEL: u32 = 0xffff_ffff;

/// Declared compression method for an uncompressed entry.
pub const METHOD_STORED: u16 = 0;
/// Declared compression method for a deflate entry.
pub const METHOD_DEFLATE: u16 = 8;

/// Why a pak could not be opened or an entry could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PakError {
    /// No end-of-central-directory record was found.
    NotAZip,
    /// A record, offset or size ran past the end of the archive.
    Truncated,
    /// The archive is valid zip but uses a feature this reader lacks.
    Unsupported(&'static str),
    /// A central directory record was malformed.
    BadEntry,
    /// The requested entry is not in the pak.
    NotFound,
}

/// One file in the pak, corresponding to `fileInPack_t` plus the parts of
/// `unz_file_info` that `FS_LoadZipFile` uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    name: String,
    method: u16,
    crc: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    local_offset: u32,
    central_offset: u32,
}

impl Entry {
    /// Entry name, lowercased as `FS_LoadZipFile` stores it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Zip compression method (`METHOD_STORED`, `METHOD_DEFLATE`, ...).
    pub fn method(&self) -> u16 {
        self.method
    }

    /// CRC-32 recorded in the central directory.
    pub fn crc(&self) -> u32 {
        self.crc
    }

    /// Compressed size in bytes.
    pub fn compressed_size(&self) -> u32 {
        self.compressed_size
    }

    /// Uncompressed size in bytes.
    pub fn uncompressed_size(&self) -> u32 {
        self.uncompressed_size
    }

    /// Offset of the local file header from the start of the archive.
    pub fn local_offset(&self) -> u32 {
        self.local_offset
    }

    /// Position of this entry's central directory record, the value
    /// `fileInPack_t.pos` keeps so the file can be reopened later.
    pub fn central_offset(&self) -> u32 {
        self.central_offset
    }

    /// True when the name ends with `/`, the convention for directories.
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

/// An opened pak: the archive bytes plus its parsed central directory.
#[derive(Debug)]
pub struct Pak<'a> {
    data: &'a [u8],
    /// Bytes before the start of the archive proper, as
    /// `byte_before_the_zipfile` in `unzip.c` tracks for sfx archives.
    prefix_len: usize,
    entries: Vec<Entry>,
}

impl<'a> Pak<'a> {
    /// Parses the central directory of a zip/pk3 held in `data`.
    pub fn open(data: &'a [u8]) -> Result<Self, PakError> {
        let eocd = find_eocd(data).ok_or(PakError::NotAZip)?;

        let disk = u16_at(data, eocd + 4)?;
        let central_disk = u16_at(data, eocd + 6)?;
        let entries_on_disk = u16_at(data, eocd + 8)?;
        let entries_total = u16_at(data, eocd + 10)?;
        let central_size = u32_at(data, eocd + 12)?;
        let central_offset = u32_at(data, eocd + 16)?;

        if disk != 0 || central_disk != 0 || entries_on_disk != entries_total {
            return Err(PakError::Unsupported("multi-disk archive"));
        }
        if entries_total == u16::MAX
            || central_size == ZIP64_SENTINEL
            || central_offset == ZIP64_SENTINEL
        {
            return Err(PakError::Unsupported("zip64 archive"));
        }

        let central_offset = central_offset as usize;
        let central_size = central_size as usize;
        let central_end = central_offset
            .checked_add(central_size)
            .ok_or(PakError::Truncated)?;
        if central_end > eocd {
            return Err(PakError::Truncated);
        }
        // Anything between the archive and the EOCD record means data was
        // prepended to the archive; local headers are shifted by that much.
        let prefix_len = eocd - central_end;

        let mut entries = Vec::with_capacity(entries_total as usize);
        let mut pos = prefix_len + central_offset;
        for _ in 0..entries_total {
            let header = data
                .get(pos..pos + CENTRAL_LEN)
                .ok_or(PakError::Truncated)?;
            if u32_from(&header[0..4]) != CENTRAL_SIG {
                return Err(PakError::BadEntry);
            }

            let flags = u16_from(&header[8..10]);
            let method = u16_from(&header[10..12]);
            let crc = u32_from(&header[16..20]);
            let compressed_size = u32_from(&header[20..24]);
            let uncompressed_size = u32_from(&header[24..28]);
            let name_len = u16_from(&header[28..30]) as usize;
            let extra_len = u16_from(&header[30..32]) as usize;
            let comment_len = u16_from(&header[32..34]) as usize;
            let disk_start = u16_from(&header[34..36]);
            let local_offset = u32_from(&header[42..46]);

            if flags & 1 != 0 {
                return Err(PakError::Unsupported("encrypted entry"));
            }
            if disk_start != 0 {
                return Err(PakError::Unsupported("multi-disk archive"));
            }
            if compressed_size == ZIP64_SENTINEL
                || uncompressed_size == ZIP64_SENTINEL
                || local_offset == ZIP64_SENTINEL
            {
                return Err(PakError::Unsupported("zip64 archive"));
            }

            let name_start = pos + CENTRAL_LEN;
            let name_end = name_start
                .checked_add(name_len)
                .ok_or(PakError::Truncated)?;
            let raw_name = data.get(name_start..name_end).ok_or(PakError::Truncated)?;

            let record_len = CENTRAL_LEN
                .checked_add(name_len)
                .and_then(|n| n.checked_add(extra_len))
                .and_then(|n| n.checked_add(comment_len))
                .ok_or(PakError::Truncated)?;
            if pos.checked_add(record_len).ok_or(PakError::Truncated)? > data.len() {
                return Err(PakError::Truncated);
            }

            entries.push(Entry {
                name: String::from_utf8_lossy(raw_name).to_ascii_lowercase(),
                method,
                crc,
                compressed_size,
                uncompressed_size,
                local_offset,
                central_offset: pos as u32,
            });
            pos += record_len;
        }

        Ok(Pak {
            data,
            prefix_len,
            entries,
        })
    }

    /// All entries, in central directory order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Number of entries, as `pack->numfiles`.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the pak has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes before the archive proper, non-zero for self-extracting zips.
    pub fn prefix_len(&self) -> usize {
        self.prefix_len
    }

    /// Finds an entry by name, ignoring ASCII case, as `FS_FindFile` does
    /// after lowercasing the query.
    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// Reads the bytes of a stored entry. Deflated entries are refused
    /// because the inflate code is not ported yet.
    pub fn read(&self, entry: &Entry) -> Result<&'a [u8], PakError> {
        if entry.method != METHOD_STORED {
            return Err(PakError::Unsupported("compressed entry"));
        }
        if entry.compressed_size != entry.uncompressed_size {
            return Err(PakError::BadEntry);
        }

        let local = self
            .prefix_len
            .checked_add(entry.local_offset as usize)
            .ok_or(PakError::Truncated)?;
        let header = self
            .data
            .get(local..local + LOCAL_LEN)
            .ok_or(PakError::Truncated)?;
        if u32_from(&header[0..4]) != LOCAL_SIG {
            return Err(PakError::BadEntry);
        }

        let name_len = u16_from(&header[26..28]) as usize;
        let extra_len = u16_from(&header[28..30]) as usize;
        let start = local
            .checked_add(LOCAL_LEN)
            .and_then(|n| n.checked_add(name_len))
            .and_then(|n| n.checked_add(extra_len))
            .ok_or(PakError::Truncated)?;
        let end = start
            .checked_add(entry.compressed_size as usize)
            .ok_or(PakError::Truncated)?;
        self.data.get(start..end).ok_or(PakError::Truncated)
    }

    /// Finds and reads an entry in one step.
    pub fn read_file(&self, name: &str) -> Result<&'a [u8], PakError> {
        let entry = self.find(name).ok_or(PakError::NotFound)?;
        self.read(entry)
    }
}

/// Scans backwards for the end-of-central-directory signature, allowing for
/// a trailing comment of up to 64 KiB, as `unzlocal_SearchCentralDir` does.
fn find_eocd(data: &[u8]) -> Option<usize> {
    if data.len() < EOCD_LEN {
        return None;
    }
    let first = data.len().saturating_sub(EOCD_LEN + MAX_COMMENT);
    (first..=data.len() - EOCD_LEN)
        .rev()
        .find(|&i| u32_from(&data[i..i + 4]) == EOCD_SIG)
}

fn u16_at(data: &[u8], at: usize) -> Result<u16, PakError> {
    let bytes = data.get(at..at + 2).ok_or(PakError::Truncated)?;
    Ok(u16_from(bytes))
}

fn u32_at(data: &[u8], at: usize) -> Result<u32, PakError> {
    let bytes = data.get(at..at + 4).ok_or(PakError::Truncated)?;
    Ok(u32_from(bytes))
}

fn u16_from(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().unwrap())
}

fn u32_from(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn u16v(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn u32v(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    /// Builds a classic single-disk zip with the given entries. `method` is
    /// written to the headers but the payload is always the raw bytes, which
    /// is only meaningful for stored entries.
    pub(crate) fn build_zip(prefix: &[u8], files: &[(&str, &[u8], u16)]) -> Vec<u8> {
        let mut out = prefix.to_vec();
        let mut central = Vec::new();

        for (name, payload, method) in files {
            let local_offset = (out.len() - prefix.len()) as u32;
            u32v(&mut out, LOCAL_SIG);
            u16v(&mut out, 20);
            u16v(&mut out, 0);
            u16v(&mut out, *method);
            u16v(&mut out, 0);
            u16v(&mut out, 0);
            u32v(&mut out, 0);
            u32v(&mut out, payload.len() as u32);
            u32v(&mut out, payload.len() as u32);
            u16v(&mut out, name.len() as u16);
            u16v(&mut out, 0);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(payload);

            u32v(&mut central, CENTRAL_SIG);
            u16v(&mut central, 20);
            u16v(&mut central, 20);
            u16v(&mut central, 0);
            u16v(&mut central, *method);
            u16v(&mut central, 0);
            u16v(&mut central, 0);
            u32v(&mut central, 0);
            u32v(&mut central, payload.len() as u32);
            u32v(&mut central, payload.len() as u32);
            u16v(&mut central, name.len() as u16);
            u16v(&mut central, 0);
            u16v(&mut central, 0);
            u16v(&mut central, 0);
            u16v(&mut central, 0);
            u32v(&mut central, 0);
            u32v(&mut central, local_offset);
            central.extend_from_slice(name.as_bytes());
        }

        let central_offset = (out.len() - prefix.len()) as u32;
        out.extend_from_slice(&central);

        u32v(&mut out, EOCD_SIG);
        u16v(&mut out, 0);
        u16v(&mut out, 0);
        u16v(&mut out, files.len() as u16);
        u16v(&mut out, files.len() as u16);
        u32v(&mut out, central.len() as u32);
        u32v(&mut out, central_offset);
        u16v(&mut out, 0);
        out
    }

    #[test]
    fn opens_and_lists_lowercased_entries() {
        let zip = build_zip(
            b"",
            &[
                ("Maps/Q3DM1.BSP", b"map", METHOD_STORED),
                ("scripts/foo.arena", b"arena", METHOD_STORED),
            ],
        );
        let pak = Pak::open(&zip).unwrap();

        assert_eq!(pak.len(), 2);
        assert!(!pak.is_empty());
        assert_eq!(pak.prefix_len(), 0);
        assert_eq!(pak.entries()[0].name(), "maps/q3dm1.bsp");
        assert_eq!(pak.entries()[1].name(), "scripts/foo.arena");
        assert_eq!(pak.entries()[0].method(), METHOD_STORED);
        assert_eq!(pak.entries()[0].uncompressed_size(), 3);
        // The central directory starts after every local record.
        assert_eq!(
            pak.entries()[0].central_offset(),
            (LOCAL_LEN + 14 + 3 + LOCAL_LEN + 17 + 5) as u32
        );
    }

    #[test]
    fn find_ignores_case() {
        let zip = build_zip(b"", &[("Maps/Q3DM1.BSP", b"map", METHOD_STORED)]);
        let pak = Pak::open(&zip).unwrap();

        assert!(pak.find("maps/q3dm1.bsp").is_some());
        assert!(pak.find("MAPS/Q3DM1.BSP").is_some());
        assert!(pak.find("maps/q3dm2.bsp").is_none());
    }

    #[test]
    fn read_returns_the_stored_bytes() {
        let zip = build_zip(
            b"",
            &[
                ("first.txt", b"hello", METHOD_STORED),
                ("second.txt", b"world", METHOD_STORED),
            ],
        );
        let pak = Pak::open(&zip).unwrap();

        assert_eq!(pak.read_file("FIRST.TXT").unwrap(), b"hello");
        assert_eq!(pak.read_file("second.txt").unwrap(), b"world");
        assert_eq!(pak.read(pak.find("first.txt").unwrap()).unwrap(), b"hello");
    }

    #[test]
    fn prepended_bytes_shift_local_headers() {
        let prefix = b"#!/bin/sh\n";
        let zip = build_zip(prefix, &[("file.txt", b"payload", METHOD_STORED)]);
        let pak = Pak::open(&zip).unwrap();

        assert_eq!(pak.prefix_len(), prefix.len());
        assert_eq!(pak.read_file("file.txt").unwrap(), b"payload");
    }

    #[test]
    fn deflated_entries_are_listed_but_not_read() {
        let zip = build_zip(b"", &[("compressed.txt", b"raw", METHOD_DEFLATE)]);
        let pak = Pak::open(&zip).unwrap();
        let entry = pak.find("compressed.txt").unwrap();

        assert_eq!(entry.method(), METHOD_DEFLATE);
        assert_eq!(
            pak.read(entry),
            Err(PakError::Unsupported("compressed entry"))
        );
    }

    #[test]
    fn empty_archive_opens() {
        let zip = build_zip(b"", &[]);
        let pak = Pak::open(&zip).unwrap();

        assert!(pak.is_empty());
        assert_eq!(pak.len(), 0);
        assert!(pak.find("anything").is_none());
    }

    #[test]
    fn data_without_eocd_is_not_a_zip() {
        assert_eq!(
            Pak::open(b"not a zip at all").unwrap_err(),
            PakError::NotAZip
        );
        assert_eq!(Pak::open(b"").unwrap_err(), PakError::NotAZip);
        assert_eq!(Pak::open(&[0u8; 21]).unwrap_err(), PakError::NotAZip);
    }

    #[test]
    fn multidisk_archive_is_rejected() {
        let mut zip = build_zip(b"", &[("file.txt", b"x", METHOD_STORED)]);
        let eocd = find_eocd(&zip).unwrap();
        zip[eocd + 4..eocd + 6].copy_from_slice(&1u16.to_le_bytes());

        assert_eq!(
            Pak::open(&zip).unwrap_err(),
            PakError::Unsupported("multi-disk archive")
        );
    }

    #[test]
    fn central_directory_past_the_eocd_is_rejected() {
        let mut zip = build_zip(b"", &[("file.txt", b"x", METHOD_STORED)]);
        let eocd = find_eocd(&zip).unwrap();
        let too_large = (eocd as u32) + 1;
        zip[eocd + 12..eocd + 16].copy_from_slice(&too_large.to_le_bytes());

        assert_eq!(Pak::open(&zip).unwrap_err(), PakError::Truncated);
    }

    #[test]
    fn bad_local_header_is_reported() {
        let mut zip = build_zip(b"", &[("file.txt", b"x", METHOD_STORED)]);
        zip[0..4].copy_from_slice(&0u32.to_le_bytes());
        let pak = Pak::open(&zip).unwrap();

        assert_eq!(
            pak.read(pak.find("file.txt").unwrap()),
            Err(PakError::BadEntry)
        );
    }

    #[test]
    fn truncated_entry_payload_is_reported() {
        let mut zip = build_zip(b"", &[("file.txt", b"payload", METHOD_STORED)]);
        // Claim the entry is larger than the archive holds.
        let eocd = find_eocd(&zip).unwrap();
        let central_offset = u32_from(&zip[eocd + 16..eocd + 20]) as usize;
        // Both sizes must be inflated; unequal sizes are rejected earlier.
        for field in [20, 24] {
            let at = central_offset + field;
            zip[at..at + 4].copy_from_slice(&9999u32.to_le_bytes());
        }
        let pak = Pak::open(&zip).unwrap();

        assert_eq!(
            pak.read(pak.find("file.txt").unwrap()),
            Err(PakError::Truncated)
        );
    }
}
