//! Identity, twice: which inode a path is (`FileId`, `Fingerprint`), and which bytes it holds (`Hash`).
//!
//! The public surface is one pipeline function plus two primitives. The stages (size bucket ->
//! sampled hash -> full BLAKE3) are private: no caller needs to know they exist, and changing the
//! sampling window cannot leak anywhere.
//!
//! Hash equality only *proposes* a duplicate. Nothing in this module decides a replacement; the
//! byte-for-byte compare that authorizes one happens in `replace`, against the clone itself.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, MetadataExt};
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
        Ok(Fingerprint::from_metadata(&std::fs::symlink_metadata(path)?))
    }

    pub fn from_metadata(m: &Metadata) -> Fingerprint {
        Fingerprint {
            id: FileId { dev: m.dev(), ino: m.ino() },
            size: m.size(),
            mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            ctime_ns: m.ctime() * 1_000_000_000 + m.ctime_nsec(),
            nlink: m.nlink(),
            uid: m.uid(),
            mode: m.mode(),
        }
    }

    pub fn same_file_unchanged(&self, now: &Fingerprint) -> bool {
        self.id == now.id && self.size == now.size && self.mtime_ns == now.mtime_ns && self.ctime_ns == now.ctime_ns
    }
}

/// BLAKE3 of the full content. Serialized as 64 lowercase hex chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn hex(&self) -> String {
        to_hex(&self.0)
    }
}

impl Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_hex32(&s).map(Hash).ok_or_else(|| serde::de::Error::custom(format!("not a 64-char hex hash: {s}")))
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
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

/// Bytes read from each of the head, middle and tail of a file for the sampled hash.
const SAMPLE: u64 = 1 << 20;
const BUF: usize = 8 << 20;

/// The matching pipeline.
///
/// 1. collapse paths by `FileId`; bucket inodes by size; drop buckets with one inode.
/// 2. sampled hash = BLAKE3(size_le || head || middle || tail), 1 MiB each (the whole file when it
///    is at most 3 MiB). GGUF headers are shared across quantizations, so head alone is weak.
///    Drop singleton sub-buckets.
/// 3. full BLAKE3 with a read loop (no mmap: a file truncated under a mapping is SIGBUS), files
///    hashed in parallel on `std::thread::scope` workers (min(available_parallelism, 4)).
/// 4. group by full hash; drop singletons.
///
/// Cost model: every file in a surviving sampled bucket is read fully once. No hash cache in v1:
/// a cache keyed on fingerprint is a second source of truth for content and the first thing to lie.
pub fn find_duplicates(files: Vec<FoundFile>) -> Matches {
    let mut by_id: BTreeMap<FileId, Inode> = BTreeMap::new();
    for f in files {
        by_id
            .entry(f.stat.id)
            .or_insert_with(|| Inode { stat: f.stat, paths: Vec::new() })
            .paths
            .push((f.path, f.store));
    }
    let mut by_size: BTreeMap<u64, Vec<Inode>> = BTreeMap::new();
    for mut inode in by_id.into_values() {
        inode.paths.sort();
        by_size.entry(inode.stat.size).or_default().push(inode);
    }
    let sized: Vec<Inode> = by_size.into_values().filter(|b| b.len() >= 2).flatten().collect();

    let mut unreadable = Vec::new();
    let sampled = refine(sized, |i| sampled_hash(&i.paths[0].0, i.stat.size), &mut unreadable);
    let full =
        refine(sampled.into_iter().flatten().map(|(i, _)| i).collect(), |i| hash_file(&i.paths[0].0), &mut unreadable);

    let mut groups: Vec<DupGroup> = full
        .into_iter()
        .map(|mut inodes| {
            inodes.sort_by(|a, b| a.0.paths[0].0.cmp(&b.0.paths[0].0));
            let (hash, size) = (inodes[0].1, inodes[0].0.stat.size);
            DupGroup { hash, size, inodes: inodes.into_iter().map(|(i, _)| i).collect() }
        })
        .collect();
    groups.sort_by_key(|g| (Reverse(g.size), g.hash));
    Matches { groups, unreadable }
}

/// Hash every inode with `key`, then split into buckets of equal (size, key) with at least two
/// members. Read failures go to `unreadable` and drop the inode.
fn refine(
    inodes: Vec<Inode>,
    key: impl Fn(&Inode) -> io::Result<Hash> + Sync,
    unreadable: &mut Vec<Unreadable>,
) -> Vec<Vec<(Inode, Hash)>> {
    let mut buckets: BTreeMap<(u64, Hash), Vec<(Inode, Hash)>> = BTreeMap::new();
    for (inode, result) in parallel_map(inodes, key) {
        match result {
            Ok(h) => buckets.entry((inode.stat.size, h)).or_default().push((inode, h)),
            Err(e) => unreadable.push(Unreadable { path: inode.paths[0].0.clone(), error: e.to_string() }),
        }
    }
    buckets.into_values().filter(|b| b.len() >= 2).collect()
}

fn parallel_map<T: Sync + Send, R: Send>(items: Vec<T>, f: impl Fn(&T) -> R + Sync) -> Vec<(T, R)> {
    if items.is_empty() {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
    let chunk = items.len().div_ceil(workers);
    let results: Vec<R> = std::thread::scope(|s| {
        let handles: Vec<_> = items.chunks(chunk).map(|c| s.spawn(|| c.iter().map(&f).collect::<Vec<R>>())).collect();
        handles.into_iter().flat_map(|h| h.join().expect("hash worker panicked")).collect()
    });
    items.into_iter().zip(results).collect()
}

fn sampled_hash(path: &Path, size: u64) -> io::Result<Hash> {
    let file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    let windows = if size <= 3 * SAMPLE {
        vec![(0, size)]
    } else {
        vec![(0, SAMPLE), (size / 2 - SAMPLE / 2, SAMPLE), (size - SAMPLE, SAMPLE)]
    };
    let mut buf = vec![0u8; SAMPLE.min(size) as usize];
    for (offset, len) in windows {
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(buf.len() as u64) as usize;
            file.read_exact_at(&mut buf[..n], offset + done)?;
            hasher.update(&buf[..n]);
            done += n as u64;
        }
    }
    Ok(Hash(*hasher.finalize().as_bytes()))
}

/// Full BLAKE3 of one file, read loop with an 8 MiB buffer.
pub fn hash_file(path: &Path) -> io::Result<Hash> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; BUF];
    loop {
        let n = read_full(&mut file, &mut buf)?;
        if n == 0 {
            return Ok(Hash(*hasher.finalize().as_bytes()));
        }
        hasher.update(&buf[..n]);
    }
}

/// Byte-for-byte compare. `Ok(None)` = same length and same bytes; `Ok(Some(off))` = first
/// differing byte offset (or the shorter length).
pub fn first_difference(a: &Path, b: &Path) -> io::Result<Option<u64>> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; BUF], vec![0u8; BUF]);
    let mut offset = 0u64;
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        let n = na.min(nb);
        if ba[..n] != bb[..n] {
            let at = ba[..n].iter().zip(&bb[..n]).position(|(x, y)| x != y).unwrap_or(n);
            return Ok(Some(offset + at as u64));
        }
        if na != nb {
            return Ok(Some(offset + n as u64));
        }
        if n == 0 {
            return Ok(None);
        }
        offset += n as u64;
    }
}

/// Fill `buf` unless EOF comes first. Returns the bytes read.
fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}
