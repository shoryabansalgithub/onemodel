//! Hugging Face hub cache: `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else `~/.cache/huggingface/hub`.
//!
//! `models--<org>--<repo>/snapshots/<rev>/<file>` are symlinks into `blobs/`. Newer hubs add a
//! shared `blobs/<2hex>/<sha256>` with per-repo blobs as symlinks into it. Only the regular file at
//! the end of a chain is a candidate; replacing it by rename keeps every symlink valid because
//! symlinks name paths, not inodes.

use std::path::{Path, PathBuf};

use super::{Entry, IntactProblem, StoreKind};
use crate::Env;

pub struct HuggingFace;

impl StoreKind for HuggingFace {
    fn name(&self) -> &'static str {
        "huggingface"
    }

    fn roots(&self, env: &Env) -> Vec<PathBuf> {
        if let Some(dir) = env.var("HF_HUB_CACHE") {
            return vec![PathBuf::from(dir)];
        }
        match env.var("HF_HOME") {
            Some(home) => vec![Path::new(home).join("hub")],
            None => vec![env.home.join(".cache/huggingface/hub")],
        }
    }

    /// `<kind>--*/blobs/<id>` and `blobs/<2hex>/<id>`: `.incomplete` -> InProgress, `.lock`/`.refs`
    /// and `.huggingface-shared-blobs` -> Ignore, else Candidate. Anything under `snapshots/` that is
    /// a regular file (old non-symlink caches on Windows-copied trees) -> Candidate too.
    fn classify(&self, rel: &Path) -> Entry {
        todo!("{rel:?}")
    }

    /// Every symlink under `*/snapshots/**` resolves (following the full chain) to an existing
    /// regular file. `deep`: blobs whose name is 64 hex equal the sha256 of their content.
    fn check_intact(&self, root: &Path, deep: bool) -> Vec<IntactProblem> {
        todo!("{root:?} {deep}")
    }
}
