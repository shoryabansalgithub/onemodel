//! `--path <dir>`: user-named directories. One store per directory, kind "path".
//!
//! Unlike app stores, a partial file here does not freeze the directory (a Downloads folder with a
//! `.crdownload` should not block deduping the finished GGUFs next to it): partials are Ignore.

use std::path::{Path, PathBuf};

use super::{Entry, StoreKind, is_partial};
use crate::Env;

pub struct UserPaths;

impl StoreKind for UserPaths {
    fn name(&self) -> &'static str {
        "path"
    }

    fn roots(&self, env: &Env) -> Vec<PathBuf> {
        env.user_paths.clone()
    }

    fn classify(&self, rel: &Path) -> Entry {
        if is_partial(rel) { Entry::Ignore } else { Entry::Candidate }
    }
}
