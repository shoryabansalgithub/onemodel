//! LM Studio: the models folder from `~/.lmstudio/settings.json` (best effort), then
//! `~/.lmstudio/models`, then the legacy `~/.cache/lm-studio/models`. Layout `<publisher>/<repo>/<file>`.
//! The partial-download suffix is undocumented, so the generic suffix list applies.

use std::path::{Path, PathBuf};

use super::{Entry, StoreKind, generic_classify};
use crate::Env;

pub struct LmStudio;

impl StoreKind for LmStudio {
    fn name(&self) -> &'static str {
        "lmstudio"
    }

    /// settings.json is parsed with serde_json into `serde_json::Value` and the first string among
    /// the known keys (`downloadsFolder`, `modelsDirectory`) is used. Any parse failure is silent:
    /// the default roots still apply.
    fn roots(&self, env: &Env) -> Vec<PathBuf> {
        todo!("{:?}", env.home)
    }

    fn classify(&self, rel: &Path) -> Entry {
        generic_classify(rel)
    }
}
