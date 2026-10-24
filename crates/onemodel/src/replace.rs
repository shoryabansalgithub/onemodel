use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::journal::{CrashPoint, OpState, Record, RunId, RunLog, RunWriter, crash_point};
use crate::matching::{Fingerprint, first_difference};
use crate::plan::{Mode, Op, Plan, Refusal};
use crate::platform::{self, CloneError};
use crate::stores;

pub struct Ctx<'a> {
    pub run: &'a RunId,
    pub plan: &'a Plan,
    pub lsof: &'a Path,
    pub writer: &'a mut RunWriter,
}

impl<'a> Ctx<'a> {
    pub fn new(run: &'a RunLog, lsof: &'a Path, writer: &'a mut RunWriter) -> Ctx<'a> {
        Ctx { run: &run.id, plan: &run.plan, lsof, writer }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Replaced,
    Refused { refusal: Refusal },
    AlreadyIndependent { note: String },
    Restored,
}

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
    let planned_mtime = if cx.plan.mode == Mode::Clone { op.target.stat.mtime_ns } else { op.keeper.stat.mtime_ns };
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
    platform::fsync(parent(&op.target.path))?;
    cx.writer.append(&Record::Done { op: op.ix })?;
    crash_point(CrashPoint::AfterDone);
    Ok(Outcome::Replaced)
}

fn finish_undo_rename(cx: &mut Ctx<'_>, op: &Op<'_>) -> anyhow::Result<Outcome> {
    platform::fsync(parent(&op.target.path))?;
    cx.writer.append(&Record::Undone { op: op.ix, note: None })?;
    Ok(Outcome::Restored)
}

fn target_unchanged(op: &Op<'_>) -> bool {
    Fingerprint::of(&op.target.path).is_ok_and(|now| op.target.stat.same_file_unchanged(&now))
}

fn refuse(cx: &mut Ctx<'_>, op: &Op<'_>, refusal: Refusal) -> anyhow::Result<Outcome> {
    cx.writer.append(&Record::Refused { op: op.ix, refusal: refusal.clone() })?;
    Ok(Outcome::Refused { refusal })
}

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

    if !target_unchanged(&op) {
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

fn revalidate(cx: &Ctx<'_>, op: &Op<'_>) -> Result<(), Refusal> {
    let keeper_same = Fingerprint::of(&op.keeper.path).is_ok_and(|now| now.id == op.keeper.stat.id);
    if !target_unchanged(op) || !keeper_same {
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

fn prepare(cx: &Ctx<'_>, op: &Op<'_>, temp: &Path) -> Result<(), Refusal> {
    match cx.plan.mode {
        Mode::Clone => platform::clone_file(&op.keeper.path, temp).map_err(|e| match e {
            CloneError::Unsupported => Refusal::NoCloneSupport {
                fs_type: cx.plan.volume(op.target.stat.id.dev).map_or_else(String::new, |v| v.fs_type.clone()),
            },
            CloneError::Io(e) => io_refusal("clone", e),
        })?,
        Mode::Hardlink => fs::hard_link(&op.keeper.path, temp).map_err(|e| io_refusal("hard link", e))?,
    }
    crash_point(CrashPoint::AfterClone);
    if cx.plan.mode == Mode::Clone {
        platform::copy_metadata(&op.target.path, temp).map_err(|e| io_refusal("metadata copy", e))?;
        crash_point(CrashPoint::AfterMetadata);
    }
    seal(temp, &op.target.path)?;
    crash_point(CrashPoint::AfterCompare);
    Ok(())
}

pub fn drive_undo(cx: &mut Ctx<'_>, op: Op<'_>, state: &OpState) -> anyhow::Result<Option<Outcome>> {
    let (done_ino, staged_ino) = match state {
        OpState::Done { ino } => (*ino, None),
        OpState::UndoStaged { done_ino, ino } => (*done_ino, Some(*ino)),
        OpState::Pending | OpState::Staged { .. } | OpState::Refused(_) | OpState::Undone => return Ok(None),
    };
    if staged_ino.is_some() && target_ino(&op)? == staged_ino {
        return finish_undo_rename(cx, &op).map(Some);
    }
    remove_if_present(&undo_temp_path(cx.run, &op))?;
    undo_done(cx, op, done_ino).map(Some)
}

fn target_ino(op: &Op<'_>) -> anyhow::Result<Option<u64>> {
    match Fingerprint::of(&op.target.path) {
        Ok(fp) if fp.id.dev == op.target.stat.id.dev => Ok(Some(fp.id.ino)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn independent(cx: &mut Ctx<'_>, op: &Op<'_>, now: Option<u64>) -> anyhow::Result<Outcome> {
    let note = if now.is_some() { "replaced by another file since dedupe" } else { "missing" }.to_string();
    cx.writer.append(&Record::Undone { op: op.ix, note: Some(note.clone()) })?;
    Ok(Outcome::AlreadyIndependent { note })
}

fn undo_refuse(cx: &mut Ctx<'_>, op: &Op<'_>, refusal: Refusal) -> anyhow::Result<Outcome> {
    cx.writer.append(&Record::UndoRefused { op: op.ix, refusal: refusal.clone() })?;
    Ok(Outcome::Refused { refusal })
}

fn undo_done(cx: &mut Ctx<'_>, op: Op<'_>, done_ino: u64) -> anyhow::Result<Outcome> {
    let now = target_ino(&op)?;
    if now != Some(done_ino) {
        return independent(cx, &op, now);
    }
    if let Err(refusal) = open_check(cx.lsof, &op.target.path) {
        return undo_refuse(cx, &op, refusal);
    }
    let available = platform::free_bytes(parent(&op.target.path))?;
    if available < op.size {
        return undo_refuse(cx, &op, Refusal::NotEnoughSpace { needed: op.size, available });
    }
    let temp = undo_temp_path(cx.run, &op);
    if let Err(refusal) = rematerialize(cx, &op, &temp) {
        remove_if_present(&temp)?;
        return undo_refuse(cx, &op, refusal);
    }
    let ino = Fingerprint::of(&temp)?.id.ino;
    cx.writer.append(&Record::UndoStaged { op: op.ix, ino })?;
    crash_point(CrashPoint::UndoAfterStaged);
    let now = target_ino(&op)?;
    if now != Some(done_ino) {
        remove_if_present(&temp)?;
        return independent(cx, &op, now);
    }
    if let Err(e) = fs::rename(&temp, &op.target.path) {
        remove_if_present(&temp)?;
        return undo_refuse(cx, &op, io_refusal("rename", e));
    }
    crash_point(CrashPoint::UndoAfterRename);
    finish_undo_rename(cx, &op)
}

fn rematerialize(cx: &Ctx<'_>, op: &Op<'_>, temp: &Path) -> Result<(), Refusal> {
    platform::real_copy(&op.target.path, temp).map_err(|e| io_refusal("copy", e))?;
    platform::copy_metadata(&op.target.path, temp).map_err(|e| io_refusal("metadata copy", e))?;
    if cx.plan.mode == Mode::Hardlink {
        platform::set_mode_and_mtime(temp, op.target.stat.mode, op.target.stat.mtime_ns)
            .map_err(|e| io_refusal("metadata copy", e))?;
    }
    seal(temp, &op.target.path)?;
    crash_point(CrashPoint::UndoAfterCopy);
    Ok(())
}

fn seal(temp: &Path, target: &Path) -> Result<(), Refusal> {
    platform::fsync(temp).map_err(|e| io_refusal("fsync", e))?;
    match first_difference(temp, target) {
        Ok(None) => Ok(()),
        Ok(Some(offset)) => Err(Refusal::ContentMismatch { offset }),
        Err(e) => Err(io_refusal("compare", e)),
    }
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

pub fn temp_path(run: &RunId, op: &Op<'_>) -> PathBuf {
    parent(&op.target.path).join(format!("{}{}-{}.tmp", stores::TEMP_PREFIX, run.0, op.ix))
}

pub fn undo_temp_path(run: &RunId, op: &Op<'_>) -> PathBuf {
    temp_path(run, op).with_extension("undo.tmp")
}
