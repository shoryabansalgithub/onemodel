//! llama.cpp `-hf` cache: `$LLAMA_CACHE`, else `~/Library/Caches/llama.cpp` (macOS) or
//! `~/.cache/llama.cpp` (Linux). Flat `*.gguf` plus `.json` etag sidecars; `*.downloadInProgress`.

use std::path::{Path, PathBuf};

use super::{Entry, StoreKind};
use crate::Env;

pub struct LlamaCpp;

impl StoreKind for LlamaCpp {
    fn name(&self) -> &'static str {
        "llama.cpp"
    }

    fn roots(&self, env: &Env) -> Vec<PathBuf> {
        if let Some(dir) = env.var("LLAMA_CACHE") {
            return vec![PathBuf::from(dir)];
        }
        if cfg!(target_os = "macos") {
            vec![env.home.join("Library/Caches/llama.cpp")]
        } else {
            vec![env.home.join(".cache/llama.cpp")]
        }
    }

    /// `*.downloadInProgress` -> InProgress, `*.gguf` -> Candidate, else Ignore.
    fn classify(&self, rel: &Path) -> Entry {
        todo!("{rel:?}")
    }
}
