//! Ollama: `$OLLAMA_MODELS` or `~/.ollama/models`.
//!
//! `blobs/sha256-<64hex>` is content-addressed; `manifests/<registry>/<ns>/<model>/<tag>` is JSON
//! listing `layers[].digest` and `size`. Pulls write `blobs/sha256-<hex>-partial[-N]`.

use std::path::{Path, PathBuf};

use super::{Entry, IntactProblem, StoreKind};
use crate::Env;

pub struct Ollama;

impl StoreKind for Ollama {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn roots(&self, env: &Env) -> Vec<PathBuf> {
        match env.var("OLLAMA_MODELS") {
            Some(dir) => vec![PathBuf::from(dir)],
            None => vec![env.home.join(".ollama/models")],
        }
    }

    /// `blobs/sha256-<64hex>` -> Candidate; `blobs/*-partial*` -> InProgress; everything else Ignore
    /// (manifests are tiny JSON and must never be touched).
    fn classify(&self, rel: &Path) -> Entry {
        todo!("{rel:?}")
    }

    /// Every layer of every manifest has a blob `blobs/sha256-<hex>` that is a regular file of the
    /// manifest's `size`. `deep`: its sha256 equals the digest. Unparseable manifest = problem.
    fn check_intact(&self, root: &Path, deep: bool) -> Vec<IntactProblem> {
        todo!("{root:?} {deep}")
    }
}
