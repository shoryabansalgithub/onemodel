//! Identity, twice: which inode a path is (`FileId`, `Fingerprint`), and which bytes it holds (`Hash`).
//!
//! The public surface is one pipeline function plus two primitives. The stages (size bucket ->
//! partial hash -> full BLAKE3) are private: no caller needs to know they exist, and changing the
//! partial-hash window cannot leak anywhere.
//!
//! Hash equality only *proposes* a duplicate. Nothing in this module decides a replacement; the
//! byte-for-byte compare that authorizes one happens in `replace::replace`, against the clone itself.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::stores::{FoundFile, StoreIx};

/// An inode. Two paths with the same `FileId` are one file (hardlinks), never duplicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// What lstat said about a path at one instant.
///
/// `same_file_unchanged` is the single definition of "this is still the file we planned against".
/// ctime is included on purpose: chmod, xattr writes, link-count changes and in-place writes all
/// bump it, and a false "changed" only costs a refusal and a rerun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub id: FileId,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub nlink: u64,
    pub uid: u32,
    pub mode: u32,
}

impl Fingerprint {
    /// lstat, never following symlinks.
    pub fn of(path: &Path) -> io::Result<Fingerprint> {
        todo!("{path:?}: std::fs::symlink_metadata + MetadataExt")
    }

    pub fn same_file_unchanged(&self, now: &Fingerprint) -> bool {
        self.id == now.id && self.size == now.size && self.mtime_ns == now.mtime_ns && self.ctime_ns == now.ctime_ns
    }
}

/// BLAKE3 of the full content. Serialized as 64 lowercase hex chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(pub [u8; 32]);

impl Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let _ = s;
        todo!("hex")
    }
}
impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let _ = d;
        todo!("hex")
    }
}

/// One inode and every candidate path that names it (hardlinks collapsed). `paths` sorted.
#[derive(Debug, Clone)]
pub struct Inode {
    pub stat: Fingerprint,
    pub paths: Vec<(PathBuf, StoreIx)>,
}

/// Inodes whose full BLAKE3 hashes are equal. Invariant: `inodes.len() >= 2`, all `stat.size == size`,
/// sorted by first path. May span volumes; `plan` splits it per volume.
#[derive(Debug, Clone)]
pub struct DupGroup {
    pub hash: Hash,
    pub size: u64,
    pub inodes: Vec<Inode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unreadable {
    pub path: PathBuf,
    pub error: String,
}

pub struct Matches {
    /// Sorted by (size descending, hash): biggest savings first, deterministic.
    pub groups: Vec<DupGroup>,
    /// Files that vanished or failed to read mid-hash. Reported, never fatal.
    pub unreadable: Vec<Unreadable>,
}

/// The matching pipeline.
///
/// 1. collapse paths by `FileId`; bucket inodes by size; drop buckets with one inode.
/// 2. partial hash = BLAKE3(size_le || first 1 MiB || last 1 MiB); drop singleton sub-buckets.
/// 3. full BLAKE3 with a read loop (no mmap: a file truncated under a mapping is SIGBUS), files
///    hashed in parallel on `std::thread::scope` workers (min(available_parallelism, 4)).
/// 4. group by full hash; drop singletons.
///
/// Cost model: every file in a surviving size bucket is read fully once. No hash cache in v1:
/// a cache keyed on fingerprint is a second source of truth for content and the first thing to lie.
pub fn find_duplicates(files: Vec<FoundFile>) -> Matches {
    let _ = files;
    todo!()
}

/// Full BLAKE3 of one file, read loop with an 8 MiB buffer.
pub fn hash_file(path: &Path) -> io::Result<Hash> {
    todo!("{path:?}")
}

/// Byte-for-byte compare. `Ok(None)` = same length and same bytes; `Ok(Some(off))` = first
/// differing byte offset (or the shorter length).
pub fn first_difference(a: &Path, b: &Path) -> io::Result<Option<u64>> {
    todo!("{a:?} {b:?}: two 8 MiB buffers, read_exact chunks, memcmp")
}
