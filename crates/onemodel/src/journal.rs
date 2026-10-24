use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::matching::{FileId, Hash};
use crate::plan::{Plan, Refusal};
use crate::platform;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunId(pub String);

impl RunId {
    fn new() -> RunId {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        RunId(format!("{secs}-{}", std::process::id()))
    }

    fn key(&self) -> (u64, u64) {
        let (a, b) = self.0.split_once('-').unwrap_or(("0", "0"));
        (a.parse().unwrap_or(0), b.parse().unwrap_or(0))
    }
}

impl Ord for RunId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key()).then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for RunId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for RunId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        match s.split_once('-') {
            Some((a, b)) if digits(a) && digits(b) => Ok(RunId(s.to_string())),
            _ => Err(format!("{s:?} is not a run id (expected <digits>-<digits>, as listed by `onemodel scan`)")),
        }
    }
}

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Plan { v: u32, run: RunId, plan: Plan },
    Staged { op: u32, ino: u64 },
    Done { op: u32 },
    Refused { op: u32, refusal: Refusal },
    End { free_after: Vec<(u64, u64)> },
    UndoStaged { op: u32, ino: u64 },
    Undone { op: u32, note: Option<String> },
    UndoRefused { op: u32, refusal: Refusal },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpState {
    Pending,
    Staged { ino: u64 },
    Done { ino: u64 },
    Refused(Refusal),
    UndoStaged { done_ino: u64, ino: u64 },
    Undone,
}

#[derive(Debug, Clone)]
pub struct RunLog {
    pub id: RunId,
    pub plan: Plan,
    pub ops: Vec<OpState>,
    pub ended: bool,
    pub file: PathBuf,
    valid_len: u64,
}

#[derive(Debug, Clone)]
pub struct Managed {
    pub run: RunId,
    pub target: PathBuf,
    pub id_after_swap: FileId,
    pub keeper: PathBuf,
    pub keeper_id: FileId,
    pub hash: Hash,
    pub size: u64,
}

pub struct Journal {
    dir: PathBuf,
    _lock: File,
}

impl Journal {
    pub fn open(state_dir: &Path) -> anyhow::Result<Journal> {
        let runs = state_dir.join("runs");
        fs::create_dir_all(&runs).with_context(|| format!("cannot create the journal at {}", runs.display()))?;
        let path = state_dir.join("lock");
        let mut lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        if !platform::try_lock_exclusive(&lock)? {
            let mut pid = String::new();
            let _ = lock.read_to_string(&mut pid);
            bail!("another onemodel (pid {}) is running; wait for it to finish", pid.trim());
        }
        lock.set_len(0)?;
        lock.rewind()?;
        write!(lock, "{}", std::process::id())?;
        for entry in fs::read_dir(&runs)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "tmp") {
                fs::remove_file(&path)?;
            }
        }
        Ok(Journal { dir: state_dir.to_path_buf(), _lock: lock })
    }

    fn runs_dir(&self) -> PathBuf {
        self.dir.join("runs")
    }

    pub fn runs(&self) -> anyhow::Result<Vec<RunLog>> {
        let mut runs = Vec::new();
        for entry in fs::read_dir(self.runs_dir())? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                let bytes = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
                runs.push(replay(path.clone(), &bytes).with_context(|| format!("journal {}", path.display()))?);
            }
        }
        runs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(runs)
    }

    pub fn begin(&self, plan: Plan) -> anyhow::Result<(RunWriter, RunLog)> {
        let id = RunId::new();
        let file = self.runs_dir().join(format!("{id}.jsonl"));
        let tmp = file.with_extension("jsonl.tmp");
        let ops = vec![OpState::Pending; plan.op_count() as usize];
        let rec = Record::Plan { v: FORMAT_VERSION, run: id.clone(), plan };
        let line = record_line(&rec)?;
        {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(&line)?;
            f.sync_all()?;
        }
        if file.exists() {
            bail!("run file {} already exists", file.display());
        }
        fs::rename(&tmp, &file)?;
        platform::fsync(&self.runs_dir())?;
        let Record::Plan { plan, .. } = rec else { unreachable!() };
        let log = RunLog { id, plan, ops, ended: false, file, valid_len: line.len() as u64 };
        let writer = self.reopen(&log)?;
        Ok((writer, log))
    }

    pub fn reopen(&self, run: &RunLog) -> anyhow::Result<RunWriter> {
        let file = OpenOptions::new().append(true).open(&run.file)?;
        if file.metadata()?.len() != run.valid_len {
            file.set_len(run.valid_len)?;
            file.sync_all()?;
        }
        Ok(RunWriter { file })
    }
}

pub struct RunWriter {
    file: File,
}

impl RunWriter {
    pub fn append(&mut self, rec: &Record) -> anyhow::Result<()> {
        let line = record_line(rec)?;
        self.file.write_all(&line).context("cannot write the journal")?;
        self.file.sync_all().context("cannot fsync the journal")?;
        Ok(())
    }
}

fn record_line(rec: &Record) -> anyhow::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(rec)?;
    line.push(b'\n');
    Ok(line)
}

pub fn replay(file: PathBuf, bytes: &[u8]) -> anyhow::Result<RunLog> {
    let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut lines = bytes[..complete].split(|&b| b == b'\n').filter(|l| !l.is_empty()).enumerate();
    let Some((_, first)) = lines.next() else { bail!("empty run file") };
    let Record::Plan { v, run, plan } = serde_json::from_slice(first).context("line 1")? else {
        bail!("line 1 is not a plan record");
    };
    if v != FORMAT_VERSION {
        bail!("journal format {v} is not supported by this onemodel (format {FORMAT_VERSION})");
    }
    let mut ops = vec![OpState::Pending; plan.op_count() as usize];
    let mut ended = false;
    for (i, line) in lines {
        let rec: Record = serde_json::from_slice(line).with_context(|| format!("line {}", i + 1))?;
        let op_ix = match &rec {
            Record::Plan { .. } => bail!("line {}: second plan record", i + 1),
            Record::End { .. } => {
                if ended {
                    bail!("line {}: second end record", i + 1);
                }
                ended = true;
                continue;
            }
            Record::Staged { op, .. }
            | Record::Done { op }
            | Record::Refused { op, .. }
            | Record::UndoStaged { op, .. }
            | Record::Undone { op, .. }
            | Record::UndoRefused { op, .. } => *op as usize,
        };
        let Some(state) = ops.get_mut(op_ix) else { bail!("line {}: op {op_ix} is not in the plan", i + 1) };
        let undo_record = matches!(rec, Record::UndoStaged { .. } | Record::Undone { .. } | Record::UndoRefused { .. });
        if undo_record != ended {
            bail!(
                "line {}: {} record {} the end record",
                i + 1,
                if undo_record { "undo" } else { "dedupe" },
                if ended { "after" } else { "before" }
            );
        }
        *state = match (&*state, rec) {
            (OpState::Pending | OpState::Staged { .. }, Record::Staged { ino, .. }) => OpState::Staged { ino },
            (OpState::Staged { ino }, Record::Done { .. }) => OpState::Done { ino: *ino },
            (OpState::Pending | OpState::Staged { .. }, Record::Refused { refusal, .. }) => OpState::Refused(refusal),
            (
                OpState::Done { ino: done_ino } | OpState::UndoStaged { done_ino, .. },
                Record::UndoStaged { ino, .. },
            ) => OpState::UndoStaged { done_ino: *done_ino, ino },
            (OpState::Done { .. } | OpState::UndoStaged { .. }, Record::Undone { .. }) => OpState::Undone,
            (OpState::Done { ino } | OpState::UndoStaged { done_ino: ino, .. }, Record::UndoRefused { .. }) => {
                OpState::Done { ino: *ino }
            }
            (from, rec) => bail!("line {}: {rec:?} is not a valid step from {from:?}", i + 1),
        };
    }
    Ok(RunLog { id: run, plan, ops, ended, file, valid_len: complete as u64 })
}

pub fn managed(runs: &[RunLog]) -> Vec<Managed> {
    let mut out = Vec::new();
    for run in runs {
        for op in run.plan.ops() {
            let ino = match run.ops[op.ix as usize] {
                OpState::Done { ino } | OpState::UndoStaged { done_ino: ino, .. } => ino,
                _ => continue,
            };
            out.push(Managed {
                run: run.id.clone(),
                target: op.target.path.clone(),
                id_after_swap: FileId { dev: op.target.stat.id.dev, ino },
                keeper: op.keeper.path.clone(),
                keeper_id: op.keeper.stat.id,
                hash: op.hash,
                size: op.size,
            });
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CrashPoint {
    AfterPlan,
    AfterClone,
    AfterMetadata,
    AfterCompare,
    AfterStaged,
    AfterRename,
    AfterDone,
    BeforeEnd,
    UndoAfterCopy,
    UndoAfterStaged,
    UndoAfterRename,
}

pub fn crash_point(p: CrashPoint) {
    static TARGET: OnceLock<Option<(CrashPoint, u32)>> = OnceLock::new();
    static HITS: AtomicU32 = AtomicU32::new(0);
    let target = TARGET.get_or_init(|| {
        let raw = std::env::var("ONEMODEL_CRASH_AT").ok().filter(|v| !v.is_empty())?;
        let parsed = raw.split_once(':').and_then(|(name, n)| {
            Some((<CrashPoint as clap::ValueEnum>::from_str(name, false).ok()?, n.parse::<u32>().ok()?))
        });
        if parsed.is_none() {
            eprintln!("onemodel: ONEMODEL_CRASH_AT={raw:?} is not <point>:<n>");
            std::process::exit(1);
        }
        parsed
    });
    if let Some((point, n)) = *target
        && point == p
        && HITS.fetch_add(1, Ordering::SeqCst) + 1 == n
    {
        platform::sigkill_self();
    }
}
