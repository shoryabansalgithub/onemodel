//! onemodel: one physical copy of each model file across local-AI app stores.
//!
//! Module map (call depth from `main` is at most three files: main -> commands -> replace -> platform):
//!
//! | module      | owns the knowledge of                                                        |
//! |-------------|------------------------------------------------------------------------------|
//! | `main`      | CLI surface, the process environment (`Env`), rendering reports, exit codes   |
//! | `commands`  | what scan / dedupe / undo / verify do, in terms of the modules below          |
//! | `stores`    | where each app keeps models, what is a candidate, what means "downloading"    |
//! | `matching`  | file identity (dev/ino/fingerprint) and content identity (hash pipeline)      |
//! | `plan`      | keeper choice, per-volume clone sets, preflight refusals (`Refusal`)          |
//! | `journal`   | the on-disk record format, run files, the lock, replay into op states         |
//! | `replace`   | the one atomic file swap, its crash recovery, and its inverse (rematerialize) |
//! | `platform`  | every OS call that differs between macOS and Linux; the only `unsafe`         |

// Sketch only: bodies are todo!(), so most items are not yet reachable.
#![allow(dead_code)]

mod commands;
mod journal;
mod matching;
mod plan;
mod platform;
mod replace;
mod stores;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};

use crate::journal::RunId;
use crate::plan::Mode;

#[derive(Parser, Debug)]
#[command(
    name = "onemodel",
    version,
    about = "Keep one copy of each AI model file on disk, using copy-on-write clones"
)]
struct Cli {
    /// Also treat DIR as a store. Repeatable. Every regular file >= --min-size inside is a candidate.
    #[arg(long = "path", value_name = "DIR", global = true)]
    paths: Vec<PathBuf>,

    /// Ignore files smaller than this. Accepts K, M, G suffixes (powers of 1024).
    #[arg(long, value_name = "SIZE", default_value = "32M", value_parser = parse_size, global = true)]
    min_size: u64,

    /// Keep the journal in DIR instead of the default location.
    #[arg(long, value_name = "DIR", env = "ONEMODEL_STATE", global = true)]
    state: Option<PathBuf>,

    /// Print one JSON document on stdout instead of text. Warnings still go to stderr.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Find stores and duplicate groups and report reclaimable space. Changes nothing.
    Scan,
    /// Replace duplicates with clones of one keeper per volume. Resumes an interrupted run first.
    Dedupe {
        /// Show exactly what would be replaced or refused, then stop. Writes no journal.
        #[arg(long)]
        dry_run: bool,
        /// Use hard links instead of clones. DANGEROUS: an app writing one path changes every path,
        /// and per-path xattrs are lost. Prints a warning; works on filesystems without clone support.
        #[arg(long)]
        hardlink: bool,
    },
    /// Turn every file onemodel replaced back into an independent copy (needs the space back).
    Undo {
        /// Only undo this run (ids are listed by `scan`). Default: every run, newest first.
        #[arg(long, value_name = "RUN_ID")]
        run: Option<RunId>,
    },
    /// Check every managed file is still byte-identical to its planned content and every store is intact.
    Verify {
        /// Also re-hash every Ollama and HF blob against the sha256 in its name.
        #[arg(long)]
        deep: bool,
    },
}

/// Everything onemodel reads from the process environment, captured once in `main`.
/// Stores locate themselves only through this, so tests isolate the binary from the real machine by
/// setting `HOME` (and clearing `OLLAMA_MODELS` etc.), which is exactly what a user's shell does.
#[derive(Debug, Clone)]
pub struct Env {
    pub home: PathBuf,
    vars: BTreeMap<String, String>,
    /// From `--path`, canonicalized. Non-directories are rejected here.
    pub user_paths: Vec<PathBuf>,
    pub min_size: u64,
    /// From `--state` or `ONEMODEL_STATE`.
    state: Option<PathBuf>,
}

impl Env {
    /// Invariant: `home` is absolute; `user_paths` are canonical existing directories.
    pub fn from_process(user_paths: Vec<PathBuf>, min_size: u64, state: Option<PathBuf>) -> anyhow::Result<Env> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).context("HOME is not set")?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            bail!("HOME must be an absolute path, not {}", home.display());
        }
        let home = std::fs::canonicalize(&home).unwrap_or(home);
        let vars =
            std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
        let user_paths = user_paths
            .iter()
            .map(|p| {
                let canonical = std::fs::canonicalize(p).with_context(|| format!("--path {}", p.display()))?;
                if !canonical.is_dir() {
                    bail!("--path {} is not a directory", p.display());
                }
                Ok(canonical)
            })
            .collect::<anyhow::Result<_>>()?;
        let state = state.map(|s| std::path::absolute(&s)).transpose().context("--state")?;
        Ok(Env { home, vars, user_paths, min_size, state })
    }

    /// Non-empty value of an environment variable.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    /// Where the journal and lock live. `--state` / `ONEMODEL_STATE` wins; else macOS
    /// `~/Library/Application Support/onemodel`, Linux `$XDG_STATE_HOME/onemodel` or
    /// `~/.local/state/onemodel`.
    pub fn state_dir(&self) -> PathBuf {
        if let Some(dir) = &self.state {
            return dir.clone();
        }
        if cfg!(target_os = "macos") {
            return self.home.join("Library/Application Support/onemodel");
        }
        match self.var("XDG_STATE_HOME") {
            Some(dir) if Path::new(dir).is_absolute() => Path::new(dir).join("onemodel"),
            _ => self.home.join(".local/state/onemodel"),
        }
    }

    /// Path of the lsof binary: `ONEMODEL_LSOF` if set, else `/usr/sbin/lsof` on macOS, else `lsof` on PATH.
    pub fn lsof(&self) -> PathBuf {
        match self.var("ONEMODEL_LSOF") {
            Some(p) => PathBuf::from(p),
            None if cfg!(target_os = "macos") => PathBuf::from("/usr/sbin/lsof"),
            None => PathBuf::from("lsof"),
        }
    }

    /// `path` with the home directory written as `~`, for human output.
    pub fn show(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

/// Process exit codes. Refusals are not failures: a dedupe that refused some files exits 0 and says why.
#[repr(u8)]
#[derive(Clone, Copy)]
pub enum Exit {
    Ok = 0,
    /// Operational error: journal unwritable, another onemodel holds the lock, bad arguments.
    Error = 1,
    /// `verify` found a problem, or `dedupe`/`undo` left an op it could not settle.
    Problems = 2,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = run(cli);
    match result {
        Ok(exit) => ExitCode::from(exit as u8),
        Err(err) => {
            eprintln!("onemodel: {err:#}");
            ExitCode::from(Exit::Error as u8)
        }
    }
}

/// Flat dispatch: build `Env`, call one command, render its report. No logic lives here.
fn run(cli: Cli) -> anyhow::Result<Exit> {
    let env = Env::from_process(cli.paths, cli.min_size, cli.state)?;
    match cli.cmd {
        Cmd::Scan => {
            let report = commands::scan(&env)?;
            render(cli.json, &report)
        }
        Cmd::Dedupe { dry_run, hardlink } => {
            let mode = if hardlink { Mode::Hardlink } else { Mode::Clone };
            if hardlink {
                eprintln!("{}", HARDLINK_WARNING);
            }
            let report = commands::dedupe(&env, mode, dry_run)?;
            render(cli.json, &report)
        }
        Cmd::Undo { run } => {
            let report = commands::undo(&env, run)?;
            render(cli.json, &report)
        }
        Cmd::Verify { deep } => {
            let report = commands::verify(&env, deep)?;
            render(cli.json, &report)
        }
    }
}

const HARDLINK_WARNING: &str = "warning: --hardlink makes every duplicate the SAME file. If any app \
writes to one path, every other path changes too, and per-path xattrs are replaced by the keeper's. \
Prefer clones (the default) on APFS, btrfs and XFS.";

/// Every report knows its exit code and both of its renderings, so `run` stays a dispatcher.
pub trait Report: serde::Serialize {
    fn human(&self) -> String;
    fn exit(&self) -> Exit;
}

fn render<R: Report>(json: bool, report: &R) -> anyhow::Result<Exit> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        print!("{}", report.human());
    }
    Ok(report.exit())
}

/// "32M" -> 33554432. Plain digits are bytes; K, M, G (optionally followed by "iB" or "B") are
/// powers of 1024.
fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(digits);
    let n: u64 = num.parse().map_err(|_| format!("{s:?} is not a size like 500M or 2G"))?;
    let shift = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        _ => return Err(format!("{s:?} is not a size like 500M or 2G")),
    };
    n.checked_mul(1 << shift).ok_or_else(|| format!("{s:?} is too large"))
}

/// Human byte count in decimal GB/MB (matches Finder and `df -H`, which is how users check savings).
pub fn fmt_bytes(n: u64) -> String {
    let f = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.2} GB", f / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1} MB", f / 1e6)
    } else if n >= 1_000 {
        format!("{:.1} kB", f / 1e3)
    } else {
        format!("{n} B")
    }
}
