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

const GLOBAL: &str = "Options for every command";

#[derive(Parser, Debug)]
#[command(
    name = "onemodel",
    version,
    about = "Keep one copy of each AI model file on disk, using copy-on-write clones",
    long_about = "Finds byte-identical model files across Ollama, Hugging Face, LM Studio, llama.cpp \
and any directory you name, and replaces duplicates with copy-on-write clones of one copy. Every \
change is journaled first, so an interrupted run can be resumed or undone."
)]
struct Cli {
    /// Also treat DIR as a store. Repeatable. Every regular file of at least --min-size inside is a candidate.
    #[arg(long = "path", value_name = "DIR", global = true, help_heading = GLOBAL)]
    paths: Vec<PathBuf>,

    /// Ignore files smaller than this. Accepts K, M and G suffixes (powers of 1024).
    #[arg(long, value_name = "SIZE", default_value = "32M", value_parser = parse_size, global = true, help_heading = GLOBAL)]
    min_size: u64,

    /// Keep the journal in DIR instead of the default location.
    #[arg(long, value_name = "DIR", env = "ONEMODEL_STATE", global = true, help_heading = GLOBAL)]
    state: Option<PathBuf>,

    /// Print one JSON document on stdout instead of text. Warnings still go to stderr.
    #[arg(long, global = true, help_heading = GLOBAL)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Report stores, duplicate files and reclaimable space. Changes nothing.
    Scan,
    /// Replace duplicates with clones of one copy. Resumes an interrupted run first.
    Dedupe {
        /// Show what would be replaced or refused, then stop. Writes nothing.
        #[arg(long)]
        dry_run: bool,
        /// Use hard links instead of clones. A write through any one path then changes every path,
        /// and each path loses its own xattrs. Works without clone support.
        #[arg(long)]
        hardlink: bool,
    },
    /// Make every replaced file an independent copy again. Needs the space back.
    Undo {
        /// Only undo this run (ids are listed by `scan`). Default: every run, newest first.
        #[arg(long, value_name = "RUN_ID")]
        run: Option<RunId>,
        /// Only undo files under PATH. Repeatable.
        #[arg(long, value_name = "PATH")]
        only: Vec<PathBuf>,
    },
    /// Check that managed files and stores are intact. Exits 2 on any problem.
    Verify {
        /// Also check every Ollama blob against the sha256 in its manifest.
        #[arg(long)]
        deep: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Env {
    pub home: PathBuf,
    vars: BTreeMap<String, String>,
    pub user_paths: Vec<PathBuf>,
    pub min_size: u64,
    state: Option<PathBuf>,
}

impl Env {
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

    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn state_dir(&self) -> PathBuf {
        match &self.state {
            Some(dir) => dir.clone(),
            None => platform::default_state_dir(&self.home, self.var("XDG_STATE_HOME")),
        }
    }

    pub fn lsof(&self) -> PathBuf {
        self.var("ONEMODEL_LSOF").map_or_else(platform::default_lsof, PathBuf::from)
    }

    pub fn show(&self, path: &Path) -> String {
        let full = match path.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        };
        let mut out = String::new();
        let mut run = String::new();
        for c in full.chars().chain(std::iter::once('/')) {
            if c.is_ascii_hexdigit() {
                run.push(c);
                continue;
            }
            if run.len() >= 40 {
                out.push_str(&run[..12]);
                out.push('\u{2026}');
            } else {
                out.push_str(&run);
            }
            run.clear();
            out.push(c);
        }
        out.pop();
        out
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Ok = 0,
    Error = 1,
    Problems = 2,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(exit) => ExitCode::from(exit as u8),
        Err(err) => {
            eprintln!("onemodel: {err:#}");
            ExitCode::from(Exit::Error as u8)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<Exit> {
    let env = Env::from_process(cli.paths, cli.min_size, cli.state)?;
    match cli.cmd {
        Cmd::Scan => render(&env, cli.json, &commands::scan(&env)?),
        Cmd::Dedupe { dry_run, hardlink } => {
            if hardlink {
                eprintln!("{HARDLINK_WARNING}");
            }
            let mode = if hardlink { Mode::Hardlink } else { Mode::Clone };
            render(&env, cli.json, &commands::dedupe(&env, mode, dry_run)?)
        }
        Cmd::Undo { run, only } => {
            let only = only
                .iter()
                .map(|p| std::fs::canonicalize(p).with_context(|| format!("--only {}", p.display())))
                .collect::<anyhow::Result<Vec<_>>>()?;
            render(&env, cli.json, &commands::undo(&env, run, &only)?)
        }
        Cmd::Verify { deep } => render(&env, cli.json, &commands::verify(&env, deep)?),
    }
}

const HARDLINK_WARNING: &str = "warning: --hardlink makes every duplicate the same file. If any app \
writes to one path, every other path changes too, and each path's own xattrs are replaced by the \
keeper's. Clones (the default) do not have this problem on APFS, btrfs and XFS.";

pub trait Report: serde::Serialize {
    fn human(&self, env: &Env) -> String;
    fn exit(&self) -> Exit {
        Exit::Ok
    }
}

fn render<R: Report>(env: &Env, json: bool, report: &R) -> anyhow::Result<Exit> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        print!("{}", report.human(env));
    }
    Ok(report.exit())
}

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

pub fn plural(n: impl Into<u64>, noun: &str) -> String {
    let n = n.into();
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

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
