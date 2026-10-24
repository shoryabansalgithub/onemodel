mod huggingface;
mod llamacpp;
mod lmstudio;
mod ollama;
mod user;

use std::collections::BTreeSet;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::matching::{self, Fingerprint};

pub trait StoreKind: Sync {
    fn name(&self) -> &'static str;

    fn roots(&self, env: &Env) -> Vec<PathBuf>;

    fn classify(&self, rel: &Path) -> Entry;

    fn claims(&self, root: &Path) -> io::Result<Vec<Claim>> {
        let _ = root;
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    Regular { path: PathBuf, size: u64, sha256: [u8; 32] },
    LinkResolves { link: PathBuf },
}

#[derive(Debug)]
pub enum ClaimBroken {
    Missing,
    SizeMismatch { expected: u64, actual: u64 },
    DigestMismatch,
    Unreadable(io::Error),
    LinkBroken,
}

impl Claim {
    pub fn path(&self) -> &Path {
        match self {
            Claim::Regular { path, .. } => path,
            Claim::LinkResolves { link } => link,
        }
    }

    pub fn check(&self, hash_it: impl FnOnce(&Path) -> bool) -> Result<(), ClaimBroken> {
        match self {
            Claim::LinkResolves { link } => {
                let resolves = std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink())
                    && std::fs::metadata(link).is_ok_and(|m| m.is_file());
                if resolves { Ok(()) } else { Err(ClaimBroken::LinkBroken) }
            }
            Claim::Regular { path, size, sha256 } => {
                let meta = match std::fs::symlink_metadata(path) {
                    Ok(m) if m.is_file() => m,
                    _ => return Err(ClaimBroken::Missing),
                };
                if meta.len() != *size {
                    return Err(ClaimBroken::SizeMismatch { expected: *size, actual: meta.len() });
                }
                if !hash_it(path) {
                    return Ok(());
                }
                match sha256_file(path) {
                    Ok(actual) if actual == *sha256 => Ok(()),
                    Ok(_) => Err(ClaimBroken::DigestMismatch),
                    Err(e) => Err(ClaimBroken::Unreadable(e)),
                }
            }
        }
    }
}

fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    matching::read_chunks(path, |chunk| hasher.update(chunk))?;
    Ok(hasher.finalize().into())
}

pub fn generic_classify(rel: &Path) -> Entry {
    if is_partial(rel) { Entry::InProgress } else { Entry::Candidate }
}

pub fn is_partial(rel: &Path) -> bool {
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    PARTIAL_SUFFIXES.iter().any(|s| name.ends_with(s))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Candidate,
    InProgress,
    Ignore,
}

pub static REGISTRY: &[&dyn StoreKind] =
    &[&ollama::Ollama, &huggingface::HuggingFace, &lmstudio::LmStudio, &llamacpp::LlamaCpp, &user::UserPaths];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StoreIx(pub u16);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    pub kind: String,
    pub root: PathBuf,
    pub downloading: Option<PathBuf>,
    pub files: u32,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct FoundFile {
    pub path: PathBuf,
    pub store: StoreIx,
    pub stat: Fingerprint,
}

#[derive(Debug, Clone)]
pub struct Inventory {
    pub stores: Vec<Store>,
    pub files: Vec<FoundFile>,
}

pub fn discover(env: &Env) -> anyhow::Result<Inventory> {
    let mut inv = Inventory { stores: Vec::new(), files: Vec::new() };
    let mut files_seen = BTreeSet::new();
    for (kind, root) in locate(env) {
        let ix = StoreIx(u16::try_from(inv.stores.len()).context("too many stores")?);
        let mut store =
            Store { kind: kind.name().to_string(), root: root.clone(), downloading: None, files: 0, bytes: 0 };
        for (path, rel, meta) in walk(&root) {
            if !files_seen.insert(path.clone()) {
                continue;
            }
            match kind.classify(&rel) {
                Entry::InProgress => {
                    store.downloading.get_or_insert(path);
                }
                Entry::Candidate if meta.len() >= env.min_size => {
                    store.files += 1;
                    store.bytes += meta.len();
                    inv.files.push(FoundFile { path, store: ix, stat: Fingerprint::from_metadata(&meta) });
                }
                Entry::Candidate | Entry::Ignore => {}
            }
        }
        inv.stores.push(store);
    }
    Ok(inv)
}

pub fn locate(env: &Env) -> Vec<(&'static dyn StoreKind, PathBuf)> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for &kind in REGISTRY {
        for root in kind.roots(env) {
            let Ok(root) = std::fs::canonicalize(&root) else { continue };
            if root.is_dir() && seen.insert(root.clone()) {
                out.push((kind, root));
            }
        }
    }
    out
}

pub fn downloading_now(store: &Store) -> anyhow::Result<Option<PathBuf>> {
    let kind = kind_by_name(&store.kind).with_context(|| format!("unknown store kind {:?}", store.kind))?;
    Ok(walk(&store.root).find(|(_, rel, _)| kind.classify(rel) == Entry::InProgress).map(|(path, _, _)| path))
}

pub fn kind_by_name(name: &str) -> Option<&'static dyn StoreKind> {
    REGISTRY.iter().copied().find(|k| k.name() == name)
}

fn walk(root: &Path) -> impl Iterator<Item = (PathBuf, PathBuf, Metadata)> + '_ {
    walkdir::WalkDir::new(root).follow_links(false).sort_by_file_name().into_iter().filter_map(move |entry| {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("onemodel: skipping part of {}: {e}", root.display());
                return None;
            }
        };
        if !entry.file_type().is_file() || entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            return None;
        }
        let meta = entry.metadata().ok()?;
        let rel = entry.path().strip_prefix(root).ok()?.to_path_buf();
        Some((entry.into_path(), rel, meta))
    })
}

const PARTIAL_SUFFIXES: &[&str] =
    &[".part", ".partial", ".download", ".incomplete", ".tmp", ".downloadInProgress", ".crdownload"];

pub const TEMP_PREFIX: &str = ".onemodel-";
