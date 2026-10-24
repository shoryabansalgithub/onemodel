//! LM Studio: `~/.lmstudio/models`, the older `~/.cache/lm-studio/models`, and the folder named by
//! `downloadsFolder` in `~/.lmstudio/settings.json` (best effort). Layout `<publisher>/<repo>/<file>`.
//! The partial-download suffix is undocumented, so the generic suffix list applies.

use std::path::{Path, PathBuf};

use super::{Entry, StoreKind, generic_classify};
use crate::Env;

pub struct LmStudio;

impl StoreKind for LmStudio {
    fn name(&self) -> &'static str {
        "lmstudio"
    }

    fn roots(&self, env: &Env) -> Vec<PathBuf> {
        let mut roots = vec![env.home.join(".lmstudio/models"), env.home.join(".cache/lm-studio/models")];
        roots.extend(downloads_folder(env));
        roots
    }

    fn classify(&self, rel: &Path) -> Entry {
        generic_classify(rel)
    }
}

/// Any failure (no file, bad JSON, missing key, relative path) means "no extra root".
fn downloads_folder(env: &Env) -> Option<PathBuf> {
    let bytes = std::fs::read(env.home.join(".lmstudio/settings.json")).ok()?;
    let settings: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let raw = settings.get("downloadsFolder")?.as_str()?;
    let path = match raw.strip_prefix("~/") {
        Some(rest) => env.home.join(rest),
        None => PathBuf::from(raw),
    };
    path.is_absolute().then_some(path)
}
