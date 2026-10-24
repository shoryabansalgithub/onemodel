use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use crate::stores::{FoundFile, StoreIx};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

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

#[derive(Debug, Clone)]
pub struct Inode {
    pub stat: Fingerprint,
    pub paths: Vec<(PathBuf, StoreIx)>,
}

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
    pub groups: Vec<DupGroup>,
    pub unreadable: Vec<Unreadable>,
}

const SAMPLE: u64 = 1 << 20;
const BUF: usize = 8 << 20;

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
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4).min(items.len());
    let next = AtomicUsize::new(0);
    let mut results: Vec<Option<R>> = std::iter::repeat_with(|| None).take(items.len()).collect();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let ix = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(ix) else { break done };
                        done.push((ix, f(item)));
                    }
                })
            })
            .collect();
        for h in handles {
            for (ix, r) in h.join().expect("hash worker panicked") {
                results[ix] = Some(r);
            }
        }
    });
    items.into_iter().zip(results).map(|(t, r)| (t, r.expect("every index was taken by a worker"))).collect()
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

pub fn hash_file(path: &Path) -> io::Result<Hash> {
    let mut hasher = blake3::Hasher::new();
    read_chunks(path, |chunk| {
        hasher.update(chunk);
    })?;
    Ok(Hash(*hasher.finalize().as_bytes()))
}

/// Feed every byte of `path` to `f` in order, through one 8 MiB buffer (no mmap: a file truncated
/// under a mapping is SIGBUS).
pub fn read_chunks(path: &Path, mut f: impl FnMut(&[u8])) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; BUF];
    loop {
        let n = read_full(&mut file, &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        f(&buf[..n]);
    }
}

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
