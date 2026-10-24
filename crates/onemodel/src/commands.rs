//! The four commands, each a short straight-line function over stores -> matching -> plan ->
//! journal -> replace. Reports are plain data; `main` renders them.

use std::path::PathBuf;

use serde::Serialize;

use crate::journal::{CrashPoint, Journal, RunId, crash_point};
use crate::matching::Unreadable;
use crate::plan::{GroupPlan, Mode, Plan, Refusal};
use crate::replace::Outcome;
use crate::stores::{IntactProblem, Store};
use crate::{Env, Exit, Report};

// ---------------------------------------------------------------------------------------------
// scan

#[derive(Serialize)]
pub struct ScanReport {
    pub stores: Vec<Store>,
    pub groups: Vec<GroupPlan>,
    /// Sum of target sizes: what `dedupe` would free if every op succeeds.
    pub reclaimable: u64,
    pub unreadable: Vec<Unreadable>,
    /// Runs with ops still managed, for `undo --run`.
    pub runs: Vec<RunSummary>,
    /// Set when a dedupe was interrupted: the next `dedupe` resumes it.
    pub interrupted: Option<RunId>,
}

#[derive(Serialize)]
pub struct RunSummary {
    pub id: RunId,
    pub managed_files: u32,
    pub managed_bytes: u64,
}

/// discover -> find_duplicates -> make_plan(Clone). Takes the journal lock (read-only use) so it
/// never sees a half-appended run. Identical code path to `dedupe --dry-run` up to rendering.
pub fn scan(env: &Env) -> anyhow::Result<ScanReport> {
    let _ = env;
    todo!()
}

// ---------------------------------------------------------------------------------------------
// dedupe

#[derive(Serialize)]
pub struct DedupeReport {
    pub run: Option<RunId>,
    pub dry_run: bool,
    /// True when this invocation continued an interrupted run instead of planning a new one.
    pub resumed: bool,
    pub mode: Mode,
    /// The plan executed (or that would be, for --dry-run), including plan-time refusals.
    pub plan: Plan,
    /// One per op, in op order.
    pub results: Vec<OpResult>,
    /// Measured: statvfs free bytes per volume, plan time vs end of run.
    pub space: Vec<SpaceDelta>,
}

#[derive(Serialize)]
pub struct OpResult {
    pub op: u32,
    pub target: PathBuf,
    pub keeper: PathBuf,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Serialize)]
pub struct SpaceDelta {
    pub mount: PathBuf,
    pub free_before: u64,
    pub free_after: u64,
}

/// ```text
/// journal = Journal::open(state_dir)                      // lock
/// if an un-ended run exists:                              // resume, whatever flags were passed
///     dry_run  -> report "would resume run X", change nothing
///     else     -> writer = reopen(run); execute(run.plan, run.ops)
/// else:
///     plan = make_plan(discover(), find_duplicates(), mode, managed(runs))
///     dry_run or no ops -> report, write nothing
///     (writer, run) = journal.begin(plan)                  // Plan record durable
///     crash_point(AfterPlan)
///     execute(plan, all Pending)
/// execute = for op in plan.ops(): replace::drive(op, state[op])   // recovery is inside drive
///           crash_point(BeforeEnd); append End{free_after}
/// ```
/// A resumed run reports `free_before` from its plan, so the measured savings cover the whole run.
pub fn dedupe(env: &Env, mode: Mode, dry_run: bool) -> anyhow::Result<DedupeReport> {
    let _ = (env, mode, dry_run, Journal::open, crash_point as fn(CrashPoint));
    todo!()
}

// ---------------------------------------------------------------------------------------------
// undo

#[derive(Serialize)]
pub struct UndoReport {
    pub results: Vec<(RunId, OpResult)>,
    pub space: Vec<SpaceDelta>,
}

/// ```text
/// journal = open; runs = journal.runs()
/// for each un-ended run (interrupted dedupe): settle in-flight ops with replace::drive's recovery
///     rows only (Staged -> Done or Refused; Pending left Pending), append End. No new replaces.
/// for run in runs newest-first (or only --run), ops in reverse order:
///     replace::drive_undo(op, state)    // includes free-space check -> UndoRefused(NotEnoughSpace)
/// ```
/// Idempotent: a second `undo` finds every op Undone and does nothing.
pub fn undo(env: &Env, run: Option<RunId>) -> anyhow::Result<UndoReport> {
    let _ = (env, run, Refusal::ChangedSinceScan);
    todo!()
}

// ---------------------------------------------------------------------------------------------
// verify

#[derive(Serialize)]
pub struct VerifyReport {
    pub checked_files: u32,
    pub checked_bytes: u64,
    pub problems: Vec<Problem>,
    /// Managed files an app has since replaced (new inode). Informational: no longer ours.
    pub released: Vec<PathBuf>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Problem {
    /// A managed file or its keeper no longer hashes to the planned content (in-place write).
    ContentChanged {
        path: PathBuf,
    },
    Missing {
        path: PathBuf,
    },
    Store {
        store: String,
        problem: IntactProblem,
    },
    /// A dedupe was interrupted; run `onemodel dedupe` to finish it or `onemodel undo`.
    Interrupted {
        run: RunId,
    },
}

/// For every `journal::managed` entry whose target still has its post-rename inode: full BLAKE3 of
/// target and (once per keeper) keeper equals the planned hash. Then every discovered store's
/// `check_intact(deep)`. Exit 2 if `problems` is non-empty.
pub fn verify(env: &Env, deep: bool) -> anyhow::Result<VerifyReport> {
    let _ = (env, deep);
    todo!()
}

// ---------------------------------------------------------------------------------------------

impl Report for ScanReport {
    fn human(&self) -> String {
        todo!("stores table, groups with keeper/clone/refuse lines, reclaimable total")
    }
    fn exit(&self) -> Exit {
        Exit::Ok
    }
}

impl Report for DedupeReport {
    fn human(&self) -> String {
        todo!("per-op lines, refusals with reasons, measured freed bytes per volume")
    }
    fn exit(&self) -> Exit {
        Exit::Ok
    }
}

impl Report for UndoReport {
    fn human(&self) -> String {
        todo!()
    }
    fn exit(&self) -> Exit {
        let unsettled = self.results.iter().any(|(_, r)| matches!(r.outcome, Outcome::Refused { .. }));
        if unsettled { Exit::Problems } else { Exit::Ok }
    }
}

impl Report for VerifyReport {
    fn human(&self) -> String {
        todo!()
    }
    fn exit(&self) -> Exit {
        if self.problems.is_empty() { Exit::Ok } else { Exit::Problems }
    }
}
