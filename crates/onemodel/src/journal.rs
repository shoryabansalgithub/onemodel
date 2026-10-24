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
//! (the write never completed, so the action it guarded never started) and is dropped. Any other
//! unparseable line, or an illegal state transition, is a hard error: we never guess about disk state.
//!
//! Undo appends to the run file of the op it reverses, so an op's whole life is in one place.

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::matching::{FileId, Hash};
use crate::plan::{Plan, Refusal};

/// `<unix-seconds>-<pid>`: sortable by start time, unique per machine (one onemodel at a time).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunId(pub String);

impl std::str::FromStr for RunId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        todo!("{s}: validate <digits>-<digits>")
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
        todo!("{state_dir:?}")
    }

    /// Every run, oldest first, replayed into op states.
    pub fn runs(&self) -> anyhow::Result<Vec<RunLog>> {
        let _ = &self.dir;
        todo!()
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
pub fn replay(lines: &[u8]) -> anyhow::Result<(RunId, Plan, Vec<OpState>, bool)> {
    let _ = lines;
    todo!()
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
