//! Hugging Face hub cache: `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else `~/.cache/huggingface/hub`.
//!
//! `models--<org>--<repo>/snapshots/<rev>/<file>` are symlinks into `blobs/`. Newer hubs add a
//! shared `blobs/<2hex>/<id>` with per-repo blobs as symlinks into it. Only the regular file at
//! the end of a chain is a candidate; replacing it by rename keeps every symlink valid because
//! symlinks name paths, not inodes.

use std::io;
use std::path::{Path, PathBuf};

use super::{Claim, Entry, StoreKind};
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

    fn classify(&self, rel: &Path) -> Entry {
        hub_classify(rel)
    }

    fn claims(&self, root: &Path) -> io::Result<Vec<Claim>> {
        snapshot_links(root)
    }
}

/// The hub cache layout, shared with llama.cpp, which writes the same tree.
/// `models--*/blobs/<id>` and `blobs/<2hex>/<id>` are candidates; `.incomplete` anywhere is a
/// download in progress; `.lock`, `.refs`, dotfiles and everything else are not ours.
pub fn hub_classify(rel: &Path) -> Entry {
    let parts: Vec<&str> = rel.iter().map(|c| c.to_str().unwrap_or("")).collect();
    let name = parts.last().copied().unwrap_or("");
    if name.ends_with(".incomplete") {
        return Entry::InProgress;
    }
    if name.starts_with('.') || name.ends_with(".lock") || name.ends_with(".refs") {
        return Entry::Ignore;
    }
    match parts.as_slice() {
        [repo, "blobs", _] if repo.starts_with("models--") => Entry::Candidate,
        ["blobs", shard, _] if shard.len() == 2 && shard.bytes().all(|b| b.is_ascii_hexdigit()) => Entry::Candidate,
        _ => Entry::Ignore,
    }
}

/// Every symlink under `models--*/snapshots/` must resolve to a regular file.
pub fn snapshot_links(root: &Path) -> io::Result<Vec<Claim>> {
    let mut out = Vec::new();
    for repo in std::fs::read_dir(root)? {
        let repo = repo?;
        if !repo.file_name().to_string_lossy().starts_with("models--") || !repo.file_type()?.is_dir() {
            continue;
        }
        let snapshots = repo.path().join("snapshots");
        if !snapshots.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&snapshots).follow_links(false).sort_by_file_name() {
            let entry = entry.map_err(io::Error::other)?;
            if entry.path_is_symlink() {
                out.push(Claim::LinkResolves { link: entry.into_path() });
            }
        }
    }
    Ok(out)
}
