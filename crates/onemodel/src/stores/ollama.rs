use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Claim, Entry, StoreKind};
use crate::Env;
use crate::matching::parse_hex32;

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

    fn classify(&self, rel: &Path) -> Entry {
        let mut parts = rel.iter().map(|c| c.to_str().unwrap_or(""));
        let (Some("blobs"), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            return Entry::Ignore;
        };
        if name.contains("-partial") {
            Entry::InProgress
        } else if name.strip_prefix("sha256-").and_then(parse_hex32).is_some() {
            Entry::Candidate
        } else {
            Entry::Ignore
        }
    }

    fn claims(&self, root: &Path) -> io::Result<Vec<Claim>> {
        let manifests = root.join("manifests");
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(&manifests).follow_links(false).sort_by_file_name() {
            let entry = match entry {
                Ok(e) => e,
                Err(e)
                    if e.path() == Some(manifests.as_path())
                        && e.io_error().is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    break;
                }
                Err(e) => return Err(io::Error::other(e)),
            };
            if !entry.file_type().is_file() || entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let bytes = std::fs::read(entry.path())?;
            let manifest: Manifest = serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", entry.path().display())))?;
            for layer in manifest.config.into_iter().chain(manifest.layers) {
                let sha256 = layer.digest.strip_prefix("sha256:").and_then(parse_hex32).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: digest {:?} is not sha256:<64 hex>", entry.path().display(), layer.digest),
                    )
                })?;
                let path = root.join("blobs").join(format!("sha256-{}", &layer.digest["sha256:".len()..]));
                out.push(Claim::Regular { path, size: layer.size, sha256 });
            }
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
struct Manifest {
    config: Option<Layer>,
    #[serde(default)]
    layers: Vec<Layer>,
}

#[derive(Deserialize)]
struct Layer {
    digest: String,
    size: u64,
}
