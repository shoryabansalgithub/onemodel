//! The store registry: everything onemodel knows about where apps keep models.
//!
//! A store kind answers three questions and nothing else: where are your roots, what is this file
//! (candidate / in-progress download / not yours), and are you intact. The walk, the size filter,
//! symlink handling, overlap between roots and the "downloading" verdict are shared and live here,
//! so a new kind cannot get them wrong.
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

use std::path::{Path, PathBuf};

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

    /// Store-specific structural integrity, run by `verify`. `deep` permits re-hashing content.
    /// Default: no structure beyond the files themselves.
    fn check_intact(&self, root: &Path, deep: bool) -> Vec<IntactProblem> {
        let _ = (root, deep);
        Vec::new()
    }
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

#[derive(Debug, Clone, Serialize)]
pub struct IntactProblem {
    pub path: PathBuf,
    pub detail: String,
}

/// Find every store root on this machine and every candidate file in it.
///
/// Invariants of the result:
/// - only regular files (lstat, never following symlinks) with size >= `env.min_size`;
/// - each canonical path appears once, attributed to the first registry kind that claims it;
/// - files named `.onemodel-*.tmp` are never returned and never mark a store as downloading;
/// - `stores` order is registry order, then root order: deterministic for a given machine state.
pub fn discover(env: &Env) -> anyhow::Result<Inventory> {
    let _ = env;
    // TODO: for kind in REGISTRY, for root in kind.roots(env) that is an existing dir (dedupe by
    //       canonical path): walk(root) -> classify each regular file -> collect Candidates, record
    //       first InProgress as `downloading`.
    todo!()
}

/// Re-check a store for an in-progress download right before touching any of its files.
/// A pull can start between plan and execute; this is the cheap metadata-only walk.
pub fn downloading_now(store: &Store) -> anyhow::Result<Option<PathBuf>> {
    todo!("walk({:?}), first file classified InProgress", store.root)
}

/// Look up a registered kind by its stable name (from a journal written by an older run).
pub fn kind_by_name(name: &str) -> Option<&'static dyn StoreKind> {
    REGISTRY.iter().copied().find(|k| k.name() == name)
}

/// Shared no-follow traversal: yields (absolute path, path relative to root, lstat) for regular
/// files only, skipping onemodel temp files. Unreadable subdirectories are skipped with a warning
/// on stderr, never fatal: a store we cannot fully read is still safe to partially dedupe because
/// every replace revalidates its own target.
fn walk(root: &Path) -> impl Iterator<Item = (PathBuf, PathBuf, std::fs::Metadata)> {
    let _ = root;
    std::iter::from_fn(|| todo!())
}

/// Suffixes that mean "a download is still being written" across apps whose exact convention is
/// unknown (LM Studio) or shared (HF `.incomplete`, llama.cpp `.downloadInProgress`).
pub const PARTIAL_SUFFIXES: &[&str] =
    &[".part", ".partial", ".download", ".incomplete", ".tmp", ".downloadInProgress", ".crdownload"];

/// Classification for stores with no layout of their own: partial suffix -> InProgress, else Candidate.
pub fn generic_classify(rel: &Path) -> Entry {
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if PARTIAL_SUFFIXES.iter().any(|s| name.ends_with(s)) { Entry::InProgress } else { Entry::Candidate }
}

/// Prefix of onemodel's own temp files (see `replace::temp_path`). Ignored by every walk.
pub const TEMP_PREFIX: &str = ".onemodel-";
