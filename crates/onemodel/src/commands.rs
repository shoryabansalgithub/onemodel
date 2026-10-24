//! The four commands, each a short straight-line function over stores -> matching -> plan ->
//! journal -> replace. Reports are plain data; `main` renders them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::bail;
use serde::Serialize;

use crate::journal::{self, CrashPoint, Journal, Record, RunId, RunLog, RunWriter, crash_point};
use crate::matching::{self, Fingerprint, Unreadable};
use crate::plan::{self, GroupPlan, Mode, Plan, Refusal, Volume};
use crate::platform;
use crate::replace::{self, Ctx, Outcome};
use crate::stores::{self, Claim, Store};
use crate::{Env, Exit, Report};

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

/// discover -> find_duplicates -> make_plan(Clone). Takes the journal lock so it never sees a
/// half-appended run. Identical code path to `dedupe --dry-run` up to rendering.
pub fn scan(env: &Env) -> anyhow::Result<ScanReport> {
    let journal = Journal::open(&env.state_dir())?;
    let runs = journal.runs()?;
    let plan = fresh_plan(env, Mode::Clone, &runs)?;
    let managed = journal::managed(&runs);
    let summaries = runs
        .iter()
        .filter_map(|run| {
            let mine: Vec<_> = managed.iter().filter(|m| m.run == run.id).collect();
            (!mine.is_empty()).then(|| RunSummary {
                id: run.id.clone(),
                managed_files: mine.len() as u32,
                managed_bytes: mine.iter().map(|m| m.size).sum(),
            })
        })
        .collect();
    Ok(ScanReport {
        stores: plan.stores.clone(),
        reclaimable: plan.reclaimable(),
        groups: plan.groups,
        unreadable: plan.unreadable,
        runs: summaries,
        interrupted: runs.iter().find(|r| !r.ended).map(|r| r.id.clone()),
    })
}

fn fresh_plan(env: &Env, mode: Mode, runs: &[RunLog]) -> anyhow::Result<Plan> {
    let inv = stores::discover(env)?;
    let matches = matching::find_duplicates(inv.files.clone());
    plan::make_plan(env, inv, matches, mode, &journal::managed(runs))
}

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
    /// Measured: statfs free bytes per volume, plan time vs end of run.
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
    let journal = Journal::open(&env.state_dir())?;
    let runs = journal.runs()?;
    if let Some(run) = runs.iter().find(|r| !r.ended) {
        if dry_run {
            return Ok(DedupeReport {
                run: Some(run.id.clone()),
                dry_run,
                resumed: true,
                mode: run.plan.mode,
                plan: run.plan.clone(),
                results: Vec::new(),
                space: Vec::new(),
            });
        }
        let mut writer = journal.reopen(run)?;
        return execute(env, run, &mut writer, true);
    }
    let plan = fresh_plan(env, mode, &runs)?;
    if dry_run || plan.op_count() == 0 {
        return Ok(DedupeReport {
            run: None,
            dry_run,
            resumed: false,
            mode,
            plan,
            results: Vec::new(),
            space: Vec::new(),
        });
    }
    let (mut writer, run) = journal.begin(plan)?;
    crash_point(CrashPoint::AfterPlan);
    execute(env, &run, &mut writer, false)
}

fn execute(env: &Env, run: &RunLog, writer: &mut RunWriter, resumed: bool) -> anyhow::Result<DedupeReport> {
    let lsof = env.lsof();
    let mut cx = Ctx { run: &run.id, plan: &run.plan, mode: run.plan.mode, lsof: &lsof, writer };
    let mut results = Vec::new();
    for op in run.plan.ops() {
        let outcome = replace::drive(&mut cx, op, &run.ops[op.ix as usize])?;
        results.push(OpResult { op: op.ix, target: op.target.path.clone(), keeper: op.keeper.path.clone(), outcome });
    }
    crash_point(CrashPoint::BeforeEnd);
    let free_after = measure(&run.plan.volumes)?;
    cx.writer.append(&Record::End { free_after: free_after.clone() })?;
    let space = run
        .plan
        .volumes
        .iter()
        .zip(&free_after)
        .map(|(v, (_, after))| SpaceDelta { mount: v.mount.clone(), free_before: v.free_before, free_after: *after })
        .collect();
    Ok(DedupeReport {
        run: Some(run.id.clone()),
        dry_run: false,
        resumed,
        mode: run.plan.mode,
        plan: run.plan.clone(),
        results,
        space,
    })
}

/// (dev, free bytes) for every volume, in order.
fn measure(volumes: &[Volume]) -> anyhow::Result<Vec<(u64, u64)>> {
    volumes.iter().map(|v| Ok((v.dev, platform::free_bytes(&v.mount)?))).collect()
}

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

#[derive(Serialize)]
pub struct VerifyReport {
    /// Managed files (and their keepers) whose full content was hashed.
    pub checked_files: u32,
    pub checked_bytes: u64,
    /// Store roots whose claims were checked, and how many claims in total.
    pub stores: u32,
    pub claims: u32,
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
    Unreadable {
        path: PathBuf,
        error: String,
    },
    /// A dedupe was interrupted; run `onemodel dedupe` to finish it or `onemodel undo`.
    Interrupted {
        run: RunId,
    },
    /// A store's metadata names a file that is not there (or is not a regular file).
    ClaimMissing {
        store: String,
        path: PathBuf,
    },
    ClaimSizeMismatch {
        store: String,
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    ClaimDigestMismatch {
        store: String,
        path: PathBuf,
    },
    /// A snapshot symlink no longer leads to a regular file.
    LinkBroken {
        store: String,
        link: PathBuf,
    },
    /// The store's own metadata could not be read or parsed.
    ClaimsUnreadable {
        store: String,
        root: PathBuf,
        error: String,
    },
}

/// Managed files: for every `journal::managed` entry whose target still has its post-rename inode,
/// full BLAKE3 of target and (once per keeper) keeper equals the planned hash.
/// Stores: every claim of every store root holds. A claimed sha256 is checked for files onemodel
/// manages, and for every claimed file with `deep`. Exit 2 if `problems` is non-empty.
pub fn verify(env: &Env, deep: bool) -> anyhow::Result<VerifyReport> {
    let journal = Journal::open(&env.state_dir())?;
    let runs = journal.runs()?;
    let mut report = VerifyReport {
        checked_files: 0,
        checked_bytes: 0,
        stores: 0,
        claims: 0,
        problems: Vec::new(),
        released: Vec::new(),
    };
    for run in runs.iter().filter(|r| !r.ended) {
        report.problems.push(Problem::Interrupted { run: run.id.clone() });
    }
    let mut keepers = BTreeSet::new();
    let mut ours = BTreeSet::new();
    for m in journal::managed(&runs) {
        match Fingerprint::of(&m.target) {
            Err(_) => report.problems.push(Problem::Missing { path: m.target.clone() }),
            Ok(now) if now.id != m.ino => report.released.push(m.target.clone()),
            Ok(_) => {
                ours.insert(m.target.clone());
                check_hash(&mut report, m.target.clone(), m.hash, m.size);
            }
        }
        let keeper_is_ours = Fingerprint::of(&m.keeper).is_ok_and(|now| now.id == m.keeper_id);
        if keeper_is_ours && keepers.insert(m.keeper.clone()) {
            ours.insert(m.keeper.clone());
            check_hash(&mut report, m.keeper.clone(), m.hash, m.size);
        }
    }
    for (kind, root) in stores::locate(env) {
        report.stores += 1;
        let store = kind.name().to_string();
        let claims = match kind.claims(&root) {
            Ok(claims) => claims,
            Err(e) => {
                report.problems.push(Problem::ClaimsUnreadable { store, root, error: e.to_string() });
                continue;
            }
        };
        report.claims += claims.len() as u32;
        for claim in claims {
            if let Some(problem) = check_claim(&store, claim, |p| deep || ours.contains(p)) {
                report.problems.push(problem);
            }
        }
    }
    Ok(report)
}

fn check_claim(store: &str, claim: Claim, hash_it: impl Fn(&Path) -> bool) -> Option<Problem> {
    let store = store.to_string();
    match claim {
        Claim::LinkResolves { link } => {
            let resolves = fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink())
                && fs::metadata(&link).is_ok_and(|m| m.is_file());
            (!resolves).then_some(Problem::LinkBroken { store, link })
        }
        Claim::Regular { path, size, sha256 } => {
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) if m.is_file() => m,
                _ => return Some(Problem::ClaimMissing { store, path }),
            };
            if meta.len() != size {
                return Some(Problem::ClaimSizeMismatch { store, path, expected: size, actual: meta.len() });
            }
            if !hash_it(&path) {
                return None;
            }
            match sha256_file(&path) {
                Ok(actual) if actual == sha256 => None,
                Ok(_) => Some(Problem::ClaimDigestMismatch { store, path }),
                Err(e) => Some(Problem::Unreadable { path, error: e.to_string() }),
            }
        }
    }
}

fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().into())
}

fn check_hash(report: &mut VerifyReport, path: PathBuf, expected: matching::Hash, size: u64) {
    report.checked_files += 1;
    report.checked_bytes += size;
    match matching::hash_file(&path) {
        Ok(h) if h == expected => {}
        Ok(_) => report.problems.push(Problem::ContentChanged { path }),
        Err(e) => report.problems.push(Problem::Unreadable { path, error: e.to_string() }),
    }
}

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
