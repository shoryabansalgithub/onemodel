//! llama.cpp `-hf` cache: `$LLAMA_CACHE`, else `~/Library/Caches/llama.cpp` (macOS) or
//! `~/.cache/llama.cpp` (Linux). Current builds write the Hugging Face hub layout here; older ones
//! wrote flat `*.gguf` files with `.json` etag sidecars. Downloads in flight end in
//! `.downloadInProgress`.

use std::path::{Path, PathBuf};

use super::huggingface::hub_classify;
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

    fn classify(&self, rel: &Path) -> Entry {
        let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".downloadInProgress") {
            return Entry::InProgress;
        }
        match hub_classify(rel) {
            Entry::Ignore if rel.components().count() == 1 && name.ends_with(".gguf") => Entry::Candidate,
            entry => entry,
        }
    }
}
