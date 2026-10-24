//! The store registry: everything onemodel knows about where apps keep models.
//!
//! A store kind answers its own questions and nothing else: where are your roots, what is this
//! file (candidate / in-progress download / not yours), and what must stay true for the app to
//! keep working (its [`Claim`]s). The walk, the size filter, symlink handling, overlap between
//! roots, the "downloading" verdict and checking claims are shared, so a new kind cannot get them
//! wrong. Store kinds never hash, stat for policy, or write.
//!
//! Adding Jan, GPT4All or ComfyUI is one file implementing [`StoreKind`] plus one line in
//! [`REGISTRY`]. Example (the whole module):
//!
//! ```ignore
//! // stores/jan.rs
//! pub struct Jan;
//! impl StoreKind for Jan {
//!     fn name(&self) -> &'static str { "jan" }
//!     fn roots(&self, env: &Env) -> Vec<PathBuf> {
//!         vec![env.home.join("Library/Application Support/Jan/data/models"), env.home.join("jan/models")]
//!     }
//!     fn classify(&self, rel: &Path) -> Entry { generic_classify(rel) }
//! }
//! ```

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
use crate::matching::Fingerprint;

/// What one app's model store knows about itself.
///
/// Implementations are stateless unit structs; all machine state comes in through `env` / `root`.
pub trait StoreKind: Sync {
    /// Stable identifier, written into the journal and shown to users. Never rename one.
    fn name(&self) -> &'static str;

    /// Directories where this app keeps models on this machine, most authoritative first.
    /// May return paths that do not exist; `discover` drops them. Must not do I/O beyond
    /// best-effort reads of the app's own settings file.
    fn roots(&self, env: &Env) -> Vec<PathBuf>;

    /// Classify one regular file found under a root. `rel` is relative to that root.
    /// Never called for symlinks, directories, or onemodel's own temp files.
    fn classify(&self, rel: &Path) -> Entry;

    /// What must hold for the app to find its models, read from the app's own metadata (manifests,
    /// snapshot symlinks). `verify` checks these centrally. Most stores claim nothing.
    fn claims(&self, root: &Path) -> io::Result<Vec<Claim>> {
        let _ = root;
        Ok(Vec::new())
    }
}

/// An integrity assertion a store makes about its own files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// `path` must be a regular file of this size. Its sha256 is checked for files onemodel manages,
    /// and for every claimed file with `verify --deep`.
    Regular { path: PathBuf, size: u64, sha256: [u8; 32] },
    /// `link` must be a symlink whose chain ends at an existing regular file.
    LinkResolves { link: PathBuf },
}

/// The default for stores without their own naming rules: a known partial-download suffix means
/// the app is downloading; any other file is a candidate.
pub fn generic_classify(rel: &Path) -> Entry {
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if PARTIAL_SUFFIXES.iter().any(|s| name.ends_with(s)) { Entry::InProgress } else { Entry::Candidate }
}

/// The verdict on one regular file in a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// A finished model file onemodel may replace or keep.
    Candidate,
    /// A partial download. Its presence marks the whole store as downloading: no member of that
    /// store is planned as keeper or target until the download is gone.
    InProgress,
    /// Not a model file (manifests, locks, sidecars). Untouched and unreported.
    Ignore,
}

/// Built-in store kinds, in priority order: when roots overlap (`--path ~/.ollama`), the file is
/// attributed to the first kind that claims it.
pub static REGISTRY: &[&dyn StoreKind] =
    &[&ollama::Ollama, &huggingface::HuggingFace, &lmstudio::LmStudio, &llamacpp::LlamaCpp, &user::UserPaths];

/// Index into `Inventory::stores` (and into `Plan::stores`, which is a copy of the same list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StoreIx(pub u16);

/// One root of one kind that exists on this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    /// `StoreKind::name`; resolved back to the kind with [`kind_by_name`].
    pub kind: String,
    pub root: PathBuf,
    /// First in-progress file found under `root`, if any. `Some` means the app is mid-download.
    pub downloading: Option<PathBuf>,
    /// Candidate files at or above the minimum size, and their total size.
    pub files: u32,
    pub bytes: u64,
}

/// A candidate file, already lstat'ed. Paths are absolute and unique across the inventory.
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

/// Find every store root on this machine and every candidate file in it.
///
/// Invariants of the result:
/// - only regular files (lstat, never following symlinks) with size >= `env.min_size`;
/// - each canonical path appears once, attributed to the first registry kind whose root holds it;
/// - files named `.onemodel-*` are never returned and never mark a store as downloading;
/// - `stores` order is registry order, then root order: deterministic for a given machine state.
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

/// Every store root that exists on this machine, canonical and unique, in registry then root order.
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

/// Re-check a store for an in-progress download right before touching any of its files.
/// A pull can start between plan and execute; this is the cheap metadata-only walk.
pub fn downloading_now(store: &Store) -> anyhow::Result<Option<PathBuf>> {
    let kind = kind_by_name(&store.kind).with_context(|| format!("unknown store kind {:?}", store.kind))?;
    Ok(walk(&store.root).find(|(_, rel, _)| kind.classify(rel) == Entry::InProgress).map(|(path, _, _)| path))
}

/// Look up a registered kind by its stable name (from a journal written by an older run).
pub fn kind_by_name(name: &str) -> Option<&'static dyn StoreKind> {
    REGISTRY.iter().copied().find(|k| k.name() == name)
}

/// Shared no-follow traversal: yields (absolute path, path relative to root, lstat) for regular
/// files only, skipping onemodel temp files. Unreadable subdirectories are skipped with a warning
/// on stderr, never fatal: a store we cannot fully read is still safe to partially dedupe because
/// every replace revalidates its own target.
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

/// Suffixes that mean "a download is still being written" across apps whose exact convention is
/// unknown (LM Studio) or shared (HF `.incomplete`, llama.cpp `.downloadInProgress`).
pub const PARTIAL_SUFFIXES: &[&str] =
    &[".part", ".partial", ".download", ".incomplete", ".tmp", ".downloadInProgress", ".crdownload"];

/// Prefix of onemodel's own temp files (see `replace::temp_path`). Ignored by every walk.
pub const TEMP_PREFIX: &str = ".onemodel-";
