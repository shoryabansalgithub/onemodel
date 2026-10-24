//! The one dangerous operation, its crash recovery, and its inverse. Nothing else in onemodel
//! renames over a user's file.
//!
//! Public surface: [`drive`] (dedupe one op to a terminal state, whatever state the journal left it
//! in) and [`drive_undo`] (the same for undo). Callers never see recovery as a separate step, so
//! they cannot forget it: resuming a SIGKILLed run is just calling `drive` again on every op.
//!
//! ## Replace, step by step (crash point after each)
//!
//! ```text
//! 0  revalidate   lstat(target) same_file_unchanged plan fingerprint     else Refused(ChangedSinceScan)
//!                 lstat(keeper).id == plan keeper id, same dev            else Refused(ChangedSinceScan)
//!                 store not downloading now                               else Refused(StoreDownloading)
//!                 lsof(target) empty                                      else Refused(OpenByProcess|OpenCheckFailed)
//! 1  clone        remove stale temp; clone keeper -> temp (O_EXCL)        ENOTSUP/EOPNOTSUPP/EXDEV -> Refused(NoCloneSupport)
//!                 (hardlink mode: link(keeper, temp))                      [AfterClone]
//! 2  metadata     strip temp xattrs, copy target xattrs, mode, then atime/mtime last   [AfterMetadata]
//!                 (hardlink mode: skipped, the inode is the keeper's)
//! 3  verify       fsync temp; first_difference(temp, target) == None     else Refused(ContentMismatch)   [AfterCompare]
//! 4  journal      append Staged { op, ino(temp) }  (fsynced)             [AfterStaged]
//! 5  swap         re-lstat target unchanged; rename(temp, target); fsync(dir)   [AfterRename]
//! 6  journal      append Done { op }                                      [AfterDone]
//! ```
//! Any failure in 0-3 removes the temp and appends `Refused`. Comparing the *clone* against the
//! target (not the keeper) means the bytes we verify are the bytes we install.
//!
//! ## Recovery table (`drive` on a non-terminal op)
//!
//! | journal state  | disk                                   | action                                         |
//! |----------------|----------------------------------------|------------------------------------------------|
//! | Pending        | temp may exist (killed in 1-3)         | remove temp; run steps 0-6                     |
//! | Staged{ino}    | target ino == ino                      | rename happened (killed in 5-6): fsync dir; append Done |
//! | Staged{ino}    | target unchanged since plan            | rename did not happen: remove temp; run 0-6     |
//! | Staged{ino}    | target missing or another inode        | remove temp; append Refused(ChangedSinceScan)  |
//! | Done / Refused | -                                      | nothing (idempotent)                           |
//!
//! Every row converges to the state an uninterrupted run reaches, because step 0 revalidates
//! against the same plan and every removed temp is recreated from the same keeper.
//!
//! ## Undo (`drive_undo`)
//!
//! Undo never needs the keeper: it rematerializes the target from its own current bytes, so it
//! cannot lose an edit an app made after dedupe. Real copy (read/write loop: `std::fs::copy` would
//! clone on macOS), metadata copied from the target, fsync, compare, `UndoStaged`, rename, fsync dir,
//! `Undone`. Same shape, same recovery table with `Done.ino` in the role of the plan fingerprint:
//!
//! | journal state           | disk                         | action                                 |
//! |-------------------------|------------------------------|----------------------------------------|
//! | Done{ino}               | target ino == ino            | check free space; rematerialize        |
//! | Done{ino}               | missing / other inode        | append Undone{note}: already independent |
//! | UndoStaged{done, ino}   | target ino == ino            | fsync dir; append Undone               |
//! | UndoStaged{done, ino}   | target ino == done           | remove temp; rematerialize again       |
//! | UndoStaged{done, ino}   | anything else                | remove temp; append Undone{note}       |

use std::path::{Path, PathBuf};

use crate::journal::{OpState, RunId, RunWriter};
use crate::plan::{Mode, Op, Plan, Refusal};

/// Everything a run's ops share. Built once per command.
pub struct Ctx<'a> {
    pub run: &'a RunId,
    pub plan: &'a Plan,
    pub mode: Mode,
    pub lsof: &'a Path,
    pub writer: &'a mut RunWriter,
}

/// Terminal result of one op for the report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Replaced,
    Refused {
        refusal: Refusal,
    },
    /// Undo only: the file was already independent (app replaced it, or it is gone).
    AlreadyIndependent {
        note: String,
    },
    Restored,
}

/// Take `op` from whatever state the journal recorded to a terminal state, appending records as
/// it goes. `Err` only for journal write failures and other errors that make continuing unsafe;
/// per-file trouble is a `Refused` outcome.
pub fn drive(cx: &mut Ctx<'_>, op: Op<'_>, state: &OpState) -> anyhow::Result<Outcome> {
    let _ = (cx, op, state);
    todo!("recovery table, then steps 0-6")
}

/// Undo counterpart of [`drive`]. Ops that are Pending/Refused/Undone return without touching disk.
pub fn drive_undo(cx: &mut Ctx<'_>, op: Op<'_>, state: &OpState) -> anyhow::Result<Option<Outcome>> {
    let _ = (cx, op, state);
    todo!("undo recovery table, then rematerialize")
}

/// `<target dir>/.onemodel-<run>-<op>.tmp`. Same directory as the target, so rename is atomic and
/// cannot cross volumes. Deterministic, so recovery finds it without a record of its name.
pub fn temp_path(run: &RunId, op: &Op<'_>) -> PathBuf {
    let dir = op.target.path.parent().expect("target paths are absolute files");
    dir.join(format!("{}{}-{}.tmp", crate::stores::TEMP_PREFIX, run.0, op.ix))
}

/// Undo temps get their own suffix so a dedupe temp and an undo temp for the same op never collide.
pub fn undo_temp_path(run: &RunId, op: &Op<'_>) -> PathBuf {
    temp_path(run, op).with_extension("undo.tmp")
}
