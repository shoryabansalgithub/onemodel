//! The journal: the single source of truth for what onemodel has done to the disk.
//!
//! Layout under `Env::state_dir()`:
//! ```text
//! lock                      flock(LOCK_EX|LOCK_NB) for the whole process; SIGKILL releases it
//! runs/<run-id>.jsonl       one file per dedupe run, append-only, one `Record` per line
//! ```
//!
//! Write discipline: `RunWriter::append` writes one complete line with a single `write_all` and
//! then `sync_all` (F_FULLFSYNC on macOS) before returning. Creating a run file also fsyncs `runs/`.
//! So when any record is observable after a crash, every earlier record is too.
//!
//! Read discipline: `replay` parses every line. A final line without its newline is a torn append
//! (the write never completed, so the action it guarded never started) and is dropped; `reopen`
//! truncates it away before appending. Any other unparseable line, or an illegal state transition,
//! is a hard error: we never guess about disk state.
//!
//! Undo appends to the run file of the op it reverses, so an op's whole life is in one place.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::matching::{FileId, Hash};
use crate::plan::{Plan, Refusal};
use crate::platform;

/// `<unix-seconds>-<pid>`: sortable by start time, unique per machine (one onemodel at a time).
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

/// One journal line. `op` is the plan's op index.
///
/// Per-op lifecycle (dedupe):  Pending -> Staged -> Done      (or Pending|Staged -> Refused)
/// Per-op lifecycle (undo):    Done -> UndoStaged -> Undone   (or Done|UndoStaged -> UndoRefused -> Done)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    /// Always the first line. Written before anything on disk changes.
    Plan {
        v: u32,
        run: RunId,
        plan: Plan,
    },
    /// The temp file for `op` is cloned, has the target's metadata, is fsynced and is byte-identical
    /// to the target. `ino` is the temp's inode. The next disk action is rename(temp, target).
    /// This is the "journal written (fsynced) before each replace" record.
    Staged {
        op: u32,
        ino: u64,
    },
    /// The rename happened and the directory is fsynced. Target now has inode `Staged.ino`.
    Done {
        op: u32,
    },
    Refused {
        op: u32,
        refusal: Refusal,
    },
    /// Every op is terminal (Done or Refused) or was left Pending by an aborting `undo`.
    End {
        free_after: Vec<(u64, u64)>,
    },
    /// Undo's independent copy of the target is written, fsynced and compared; `ino` is its inode.
    UndoStaged {
        op: u32,
        ino: u64,
    },
    /// Target is an independent file again (or was already: `note` says why no copy was needed).
    Undone {
        op: u32,
        note: Option<String>,
    },
    /// Undo could not proceed for this op this time. The op stays Done; the next `undo` retries.
    UndoRefused {
        op: u32,
        refusal: Refusal,
    },
}

/// The state of one op after replaying its run file. Derived, never stored.
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
    /// Indexed by op.
    pub ops: Vec<OpState>,
    pub ended: bool,
    pub file: PathBuf,
    /// Bytes of complete lines; anything after is a torn append.
    valid_len: u64,
}

/// A file that is currently a clone (or hardlink) made by onemodel: op Done, not Undone.
#[derive(Debug, Clone)]
pub struct Managed {
    pub run: RunId,
    pub op: u32,
    pub target: PathBuf,
    /// Inode the target had right after our rename. If the path now has another inode, an app
    /// replaced the file and it is no longer ours to verify or undo.
    pub ino: FileId,
    pub keeper: PathBuf,
    pub keeper_id: FileId,
    pub hash: Hash,
}

/// Holds the process-wide lock for its lifetime. Every command that reads or writes runs opens one,
/// so scan/dedupe/undo/verify never observe each other mid-append.
pub struct Journal {
    dir: PathBuf,
    _lock: File,
}

impl Journal {
    /// mkdir -p, take the lock. A held lock is an error: "another onemodel (pid N) is running".
    /// The pid is read from the lock file, written after locking, for the message only.
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
        Ok(Journal { dir: state_dir.to_path_buf(), _lock: lock })
    }

    fn runs_dir(&self) -> PathBuf {
        self.dir.join("runs")
    }

    /// Every run, oldest first, replayed into op states. Leftover `.tmp` files are runs whose Plan
    /// never became durable, so nothing on disk changed for them: they are removed.
    pub fn runs(&self) -> anyhow::Result<Vec<RunLog>> {
        let mut runs = Vec::new();
        for entry in fs::read_dir(self.runs_dir())? {
            let path = entry?.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("jsonl") => {
                    let bytes = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
                    runs.push(replay(path.clone(), &bytes).with_context(|| format!("journal {}", path.display()))?);
                }
                Some("tmp") => fs::remove_file(&path)?,
                _ => {}
            }
        }
        runs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(runs)
    }

    /// Create `runs/<id>.jsonl` with O_EXCL, append the Plan record, fsync file and `runs/`.
    pub fn begin(&self, plan: Plan) -> anyhow::Result<(RunWriter, RunLog)> {
        let _ = plan;
        todo!()
    }

    /// Append to an existing run (resume, undo).
    pub fn reopen(&self, run: &RunLog) -> anyhow::Result<RunWriter> {
        let _ = run;
        todo!()
    }
}

pub struct RunWriter {
    file: File,
}

impl RunWriter {
    /// Serialize, append one line, `sync_all`. Returns only once the record is durable.
    /// An error here aborts the command: we must never act on a record we could not persist.
    pub fn append(&mut self, rec: &Record) -> anyhow::Result<()> {
        let _ = (&self.file, rec);
        todo!()
    }
}

/// Fold one run file's records into op states. Illegal transitions are errors.
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

/// Every file onemodel currently manages, across all runs.
pub fn managed(runs: &[RunLog]) -> Vec<Managed> {
    let _ = runs;
    todo!()
}

/// Named points where the test suite kills the process with a real SIGKILL.
///
/// Seam: if `ONEMODEL_CRASH_AT=<point>:<n>` (e.g. `after-staged:2`) and this is the n-th time this
/// process reaches `<point>`, the process sends itself SIGKILL. No destructors, no flushes, no
/// unwinding: the same as `kill -9` from outside. Inert when the variable is unset (one env read,
/// cached in a OnceLock). Kept in release builds on purpose so the tested binary is the shipped one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CrashPoint {
    /// Plan record durable; nothing on disk changed yet.
    AfterPlan,
    /// Temp clone exists, metadata not yet applied.
    AfterClone,
    /// Temp has target's metadata, not yet fsynced or compared.
    AfterMetadata,
    /// Temp fsynced and byte-compared equal; Staged not yet written.
    AfterCompare,
    /// Staged durable; rename not yet done.
    AfterStaged,
    /// Renamed over the target; directory not yet fsynced; Done not yet written.
    AfterRename,
    /// Done durable.
    AfterDone,
    /// All ops terminal; End not yet written.
    BeforeEnd,
    /// Undo: independent copy written to temp; UndoStaged not yet written.
    UndoAfterCopy,
    /// Undo: UndoStaged durable; rename not yet done.
    UndoAfterStaged,
    /// Undo: renamed; Undone not yet written.
    UndoAfterRename,
}

pub fn crash_point(p: CrashPoint) {
    // TODO: OnceLock<Option<(CrashPoint, u32)>> parsed from env; static AtomicU32 hit counters per point;
    //       on match: platform::sigkill_self().
    let _ = p;
}
