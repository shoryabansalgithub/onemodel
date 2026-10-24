//! The one dangerous operation, its crash recovery, and its inverse. Nothing else in onemodel
//! renames over a user's file.
//!
//! Public surface: [`drive`] (dedupe one op to a terminal state, whatever state the journal left it
//! in), [`settle`] (finish only what an interrupted run already started, for `undo`) and
//! [`drive_undo`] (the same as `drive` for undo). Callers never see recovery as a separate step, so
//! they cannot forget it: resuming a SIGKILLed run is just calling `drive` again on every op.
//!
//! ## Replace, step by step (crash point after each)
//!
//! ```text
//! 0  revalidate   lstat(target) same_file_unchanged plan fingerprint     else Refused(ChangedSinceScan)
//!                 lstat(keeper).id == plan keeper id                      else Refused(ChangedSinceScan)
//!                 neither carries an extended ACL                         else Refused(HasAcl)
//!                 store not downloading now                               else Refused(StoreDownloading)
//!                 lsof(target) empty                                      else Refused(OpenByProcess|OpenCheckFailed)
//! 1  clone        remove stale temp; clone keeper -> temp (O_EXCL)        ENOTSUP/EOPNOTSUPP/EXDEV -> Refused(NoCloneSupport)
//!                 (hardlink mode: link(keeper, temp))                      [AfterClone]
//! 2  metadata     temp gets exactly the target's xattrs, mode, then atime/mtime   [AfterMetadata]
//!                 (hardlink mode: skipped, the inode is the keeper's)
//! 3  verify       fsync temp; first_difference(temp, target) == None     else Refused(ContentMismatch)   [AfterCompare]
//! 4  journal      append Staged { op, ino(temp) }  (fsynced)             [AfterStaged]
//! 5  swap         re-lstat target unchanged; rename(temp, target)         [AfterRename]; fsync(dir)
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
//! | Staged{ino}    | target ino == ino, planned size+mtime  | rename happened (killed in 5-6): fsync dir; append Done |
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
//! cannot lose an edit an app made after dedupe. lsof check, free-space check, real copy
//! (read/write loop: `std::fs::copy` would clone on macOS), metadata copied from the target, fsync,
//! compare, `UndoStaged`, rename, fsync dir, `Undone`. Same shape, same recovery logic with
//! `Done.ino` in the role of the plan fingerprint:
//!
//! | journal state           | disk                         | action                                 |
//! |-------------------------|------------------------------|----------------------------------------|
//! | Done{ino}               | target ino == ino            | remove temp; check open + free space; rematerialize |
//! | Done{ino}               | missing / other inode        | remove temp; append Undone{note}: already independent |
//! | UndoStaged{done, ino}   | target ino == ino            | fsync dir; append Undone               |
//! | UndoStaged{done, ino}   | otherwise                    | remove temp; continue as Done{done}    |

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::journal::{CrashPoint, OpState, Record, RunId, RunWriter, crash_point};
use crate::matching::{Fingerprint, first_difference};
use crate::plan::{Mode, Op, Plan, Refusal};
use crate::platform::{self, CloneError};
use crate::stores;

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
    match state {
        OpState::Done { .. } | OpState::UndoStaged { .. } | OpState::Undone => Ok(Outcome::Replaced),
        OpState::Refused(refusal) => Ok(Outcome::Refused { refusal: refusal.clone() }),
        OpState::Pending => {
            remove_if_present(&temp_path(cx.run, &op))?;
            replace(cx, op)
        }
        OpState::Staged { ino } => match staged_disk_state(cx, &op, *ino)? {
            StagedDisk::Renamed => finish_rename(cx, &op),
            StagedDisk::NotRenamed => {
                remove_if_present(&temp_path(cx.run, &op))?;
                replace(cx, op)
            }
            StagedDisk::Gone => {
                remove_if_present(&temp_path(cx.run, &op))?;
                refuse(cx, &op, Refusal::ChangedSinceScan)
            }
        },
    }
}

/// For `undo` of an interrupted run: finish a rename that already happened, and otherwise remove
/// leftovers without replacing anything new. Pending ops stay Pending.
pub fn settle(cx: &mut Ctx<'_>, op: Op<'_>, state: &OpState) -> anyhow::Result<()> {
    match state {
        OpState::Pending => remove_if_present(&temp_path(cx.run, &op)),
        OpState::Staged { ino } => {
            if staged_disk_state(cx, &op, *ino)? == StagedDisk::Renamed {
                finish_rename(cx, &op)?;
            } else {
                remove_if_present(&temp_path(cx.run, &op))?;
                refuse(cx, &op, Refusal::Interrupted)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StagedDisk {
    Renamed,
    NotRenamed,
    Gone,
}

fn staged_disk_state(cx: &Ctx<'_>, op: &Op<'_>, ino: u64) -> anyhow::Result<StagedDisk> {
    let now = match Fingerprint::of(&op.target.path) {
        Ok(fp) => fp,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(StagedDisk::Gone),
        Err(e) => return Err(e.into()),
    };
    let planned_mtime = if cx.mode == Mode::Clone { op.target.stat.mtime_ns } else { op.keeper.stat.mtime_ns };
    if now.id.ino == ino && now.id.dev == op.target.stat.id.dev && now.size == op.size && now.mtime_ns == planned_mtime
    {
        Ok(StagedDisk::Renamed)
    } else if op.target.stat.same_file_unchanged(&now) {
        Ok(StagedDisk::NotRenamed)
    } else {
        Ok(StagedDisk::Gone)
    }
}

fn finish_rename(cx: &mut Ctx<'_>, op: &Op<'_>) -> anyhow::Result<Outcome> {
    platform::fsync_dir(parent(&op.target.path))?;
    cx.writer.append(&Record::Done { op: op.ix })?;
    crash_point(CrashPoint::AfterDone);
    Ok(Outcome::Replaced)
}

fn refuse(cx: &mut Ctx<'_>, op: &Op<'_>, refusal: Refusal) -> anyhow::Result<Outcome> {
    cx.writer.append(&Record::Refused { op: op.ix, refusal: refusal.clone() })?;
    Ok(Outcome::Refused { refusal })
}

/// Steps 0-6 for an op with no temp on disk.
fn replace(cx: &mut Ctx<'_>, op: Op<'_>) -> anyhow::Result<Outcome> {
    if let Err(refusal) = revalidate(cx, &op) {
        return refuse(cx, &op, refusal);
    }
    let temp = temp_path(cx.run, &op);
    if let Err(refusal) = prepare(cx, &op, &temp) {
        remove_if_present(&temp)?;
        return refuse(cx, &op, refusal);
    }
    let ino = Fingerprint::of(&temp)?.id.ino;
    cx.writer.append(&Record::Staged { op: op.ix, ino })?;
    crash_point(CrashPoint::AfterStaged);

    let unchanged = Fingerprint::of(&op.target.path).is_ok_and(|now| op.target.stat.same_file_unchanged(&now));
    if !unchanged {
        remove_if_present(&temp)?;
        return refuse(cx, &op, Refusal::ChangedSinceScan);
    }
    if let Err(e) = fs::rename(&temp, &op.target.path) {
        remove_if_present(&temp)?;
        return refuse(cx, &op, io_refusal("rename", e));
    }
    crash_point(CrashPoint::AfterRename);
    finish_rename(cx, &op)
}

/// Step 0: is the plan still true for this op, right now?
fn revalidate(cx: &Ctx<'_>, op: &Op<'_>) -> Result<(), Refusal> {
    let unchanged = Fingerprint::of(&op.target.path).is_ok_and(|now| op.target.stat.same_file_unchanged(&now));
    let keeper_same = Fingerprint::of(&op.keeper.path).is_ok_and(|now| now.id == op.keeper.stat.id);
    if !unchanged || !keeper_same {
        return Err(Refusal::ChangedSinceScan);
    }
    for path in [&op.keeper.path, &op.target.path] {
        if platform::has_acl(path).map_err(|e| io_refusal("ACL check", e))? {
            return Err(Refusal::HasAcl);
        }
    }
    let store = &cx.plan.stores[op.target.store.0 as usize];
    match stores::downloading_now(store) {
        Ok(Some(marker)) => return Err(Refusal::StoreDownloading { store: store.kind.clone(), marker }),
        Ok(None) => {}
        Err(e) => return Err(Refusal::Io { step: "download check".into(), error: format!("{e:#}") }),
    }
    open_check(cx.lsof, &op.target.path)
}

fn open_check(lsof: &Path, path: &Path) -> Result<(), Refusal> {
    match platform::open_holders(lsof, &[path]) {
        Err(detail) => Err(Refusal::OpenCheckFailed { detail }),
        Ok(open) => match open.into_values().next() {
            Some(holders) => Err(Refusal::OpenByProcess { holders }),
            None => Ok(()),
        },
    }
}

/// Steps 1-3: a temp next to the target that is byte-identical to it and carries its metadata.
fn prepare(cx: &Ctx<'_>, op: &Op<'_>, temp: &Path) -> Result<(), Refusal> {
    match cx.mode {
        Mode::Clone => platform::clone_file(&op.keeper.path, temp).map_err(|e| match e {
            CloneError::Unsupported => Refusal::NoCloneSupport {
                fs_type: cx.plan.volume(op.target.stat.id.dev).map_or_else(String::new, |v| v.fs_type.clone()),
            },
            CloneError::Io(e) => io_refusal("clone", e),
        })?,
        Mode::Hardlink => fs::hard_link(&op.keeper.path, temp).map_err(|e| io_refusal("hard link", e))?,
    }
    crash_point(CrashPoint::AfterClone);
    if cx.mode == Mode::Clone {
        platform::copy_metadata(&op.target.path, temp).map_err(|e| io_refusal("metadata copy", e))?;
        crash_point(CrashPoint::AfterMetadata);
    }
    platform::fsync_file(temp).map_err(|e| io_refusal("fsync", e))?;
    match first_difference(temp, &op.target.path) {
        Ok(None) => {}
        Ok(Some(offset)) => return Err(Refusal::ContentMismatch { offset }),
        Err(e) => return Err(io_refusal("compare", e)),
    }
    crash_point(CrashPoint::AfterCompare);
    Ok(())
}

/// Undo counterpart of [`drive`]. Ops that are Pending/Refused/Undone return without touching disk.
pub fn drive_undo(cx: &mut Ctx<'_>, op: Op<'_>, state: &OpState) -> anyhow::Result<Option<Outcome>> {
    let _ = (cx, op, state);
    todo!("undo recovery table, then rematerialize")
}

fn io_refusal(step: &str, e: io::Error) -> Refusal {
    Refusal::Io { step: step.to_string(), error: e.to_string() }
}

fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

fn parent(path: &Path) -> &Path {
    path.parent().expect("target paths are absolute files")
}

/// `<target dir>/.onemodel-<run>-<op>.tmp`. Same directory as the target, so rename is atomic and
/// cannot cross volumes. Deterministic, so recovery finds it without a record of its name.
pub fn temp_path(run: &RunId, op: &Op<'_>) -> PathBuf {
    parent(&op.target.path).join(format!("{}{}-{}.tmp", stores::TEMP_PREFIX, run.0, op.ix))
}

/// Undo temps get their own suffix so a dedupe temp and an undo temp for the same op never collide.
pub fn undo_temp_path(run: &RunId, op: &Op<'_>) -> PathBuf {
    temp_path(run, op).with_extension("undo.tmp")
}
