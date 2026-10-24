//! The four commands, each a short straight-line function over stores -> matching -> plan ->
//! journal -> replace. Reports are plain data; `main` renders them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::bail;
use serde::Serialize;

use crate::journal::{self, CrashPoint, Journal, Record, RunId, RunLog, RunWriter, crash_point};
use crate::matching::{self, Fingerprint, Unreadable};
use crate::plan::{self, GroupPlan, Mode, Plan, Role, Volume};
use crate::platform;
use crate::replace::{self, Ctx, Outcome};
use crate::stores::{self, Claim, Store};
use crate::{Env, Exit, Report, fmt_bytes, plural};

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
    pub results: Vec<UndoResult>,
    pub space: Vec<SpaceDelta>,
}

#[derive(Serialize)]
pub struct UndoResult {
    pub run: RunId,
    #[serde(flatten)]
    pub result: OpResult,
}

/// ```text
/// journal = open; runs = journal.runs()
/// for each un-ended run (interrupted dedupe): settle in-flight ops with replace::settle
///     (Staged -> Done or Refused; Pending left Pending, its temp removed), append End.
/// for run in runs newest-first (or only --run), ops in reverse order, targets under --only:
///     replace::drive_undo(op, state)    // lsof and free-space checks -> UndoRefused
/// ```
/// Idempotent: a second `undo` finds every op Undone and does nothing.
pub fn undo(env: &Env, run: Option<RunId>, only: &[PathBuf]) -> anyhow::Result<UndoReport> {
    let journal = Journal::open(&env.state_dir())?;
    let runs = journal.runs()?;
    if let Some(id) = &run
        && !runs.iter().any(|r| &r.id == id)
    {
        bail!("no run {id} in the journal");
    }
    let lsof = env.lsof();
    for interrupted in runs.iter().filter(|r| !r.ended) {
        let mut writer = journal.reopen(interrupted)?;
        let mut cx = Ctx {
            run: &interrupted.id,
            plan: &interrupted.plan,
            mode: interrupted.plan.mode,
            lsof: &lsof,
            writer: &mut writer,
        };
        for op in interrupted.plan.ops() {
            replace::settle(&mut cx, op, &interrupted.ops[op.ix as usize])?;
        }
        writer.append(&Record::End { free_after: measure(&interrupted.plan.volumes)? })?;
    }
    let runs = journal.runs()?;
    let selected: Vec<&RunLog> = runs.iter().rev().filter(|r| run.as_ref().is_none_or(|id| &r.id == id)).collect();
    let mut volumes: Vec<Volume> = Vec::new();
    for v in selected.iter().flat_map(|r| &r.plan.volumes) {
        if !volumes.iter().any(|seen| seen.dev == v.dev) {
            volumes.push(Volume { free_before: platform::free_bytes(&v.mount)?, ..v.clone() });
        }
    }
    let mut results = Vec::new();
    for log in selected {
        let mut writer = journal.reopen(log)?;
        let mut cx = Ctx { run: &log.id, plan: &log.plan, mode: log.plan.mode, lsof: &lsof, writer: &mut writer };
        let ops: Vec<_> = log.plan.ops().collect();
        for op in ops.into_iter().rev() {
            if !only.is_empty() && !only.iter().any(|p| op.target.path.starts_with(p)) {
                continue;
            }
            if let Some(outcome) = replace::drive_undo(&mut cx, op, &log.ops[op.ix as usize])? {
                let result =
                    OpResult { op: op.ix, target: op.target.path.clone(), keeper: op.keeper.path.clone(), outcome };
                results.push(UndoResult { run: log.id.clone(), result });
            }
        }
    }
    let space = measure(&volumes)?
        .into_iter()
        .zip(&volumes)
        .map(|((_, after), v)| SpaceDelta { mount: v.mount.clone(), free_before: v.free_before, free_after: after })
        .collect();
    Ok(UndoReport { results, space })
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
    fn human(&self, env: &Env) -> String {
        let mut out = String::new();
        if self.stores.is_empty() {
            out.push_str("no model stores found\n");
        } else {
            out.push_str("stores\n");
            out.push_str(&stores_table(env, &self.stores));
        }
        out.push_str(&groups_text(env, &self.groups));
        for u in &self.unreadable {
            out.push_str(&format!("unreadable {}: {}\n", env.show(&u.path), u.error));
        }
        for r in &self.runs {
            out.push_str(&format!(
                "run {} manages {} ({})\n",
                r.id,
                plural(r.managed_files, "file"),
                fmt_bytes(r.managed_bytes)
            ));
        }
        if let Some(run) = &self.interrupted {
            out.push_str(&format!(
                "run {run} was interrupted: `onemodel dedupe` finishes it, `onemodel undo` reverts it\n"
            ));
        }
        out.push_str(&format!("reclaimable {}{}\n", fmt_bytes(self.reclaimable), tally(&self.groups)));
        out
    }
    fn exit(&self) -> Exit {
        Exit::Ok
    }
}

impl Report for DedupeReport {
    fn human(&self, env: &Env) -> String {
        let mut out = String::new();
        if self.resumed {
            let run = self.run.as_ref().map_or_else(String::new, |r| format!(" {r}"));
            out.push_str(&format!("resuming interrupted run{run}\n"));
        }
        if self.dry_run {
            out.push_str(&groups_text(env, &self.plan.groups));
            out.push_str(&format!(
                "dry run: would clone {} and free up to {}{}\n",
                plural(self.plan.op_count(), "file"),
                fmt_bytes(self.plan.reclaimable()),
                tally(&self.plan.groups)
            ));
            return out;
        }
        let mut lines = Vec::new();
        for r in &self.results {
            match &r.outcome {
                Outcome::Replaced => {
                    let how = if self.mode == Mode::Hardlink { "hard link to" } else { "clone of" };
                    lines.push(row("replaced", &format!("{}  ({how} {})", env.show(&r.target), env.show(&r.keeper))));
                }
                Outcome::Refused { refusal } => {
                    lines.push(row("refused", &format!("{}  {refusal}", env.show(&r.target))));
                }
                Outcome::AlreadyIndependent { .. } | Outcome::Restored => {}
            }
        }
        for m in self.plan.groups.iter().flat_map(|g| &g.members) {
            if let Role::Refused { refusal } = &m.role {
                lines.push(row("refused", &format!("{}  {refusal}", env.show(&m.path))));
            }
        }
        out.push_str(&lines.concat());
        if self.results.is_empty() {
            let shared = count_roles(&self.plan.groups, |r| matches!(r, Role::Shared));
            out.push_str(&match shared {
                0 if self.plan.groups.is_empty() => "no duplicates found\n".to_string(),
                0 => "nothing to replace\n".to_string(),
                1 => "nothing to replace; 1 file is already shared\n".to_string(),
                n => format!("nothing to replace; {n} files are already shared\n"),
            });
        }
        out.push_str(&space_text(env, &self.space));
        out
    }
    fn exit(&self) -> Exit {
        Exit::Ok
    }
}

impl Report for UndoReport {
    fn human(&self, _env: &Env) -> String {
        format!("{} ops\n", self.results.len())
    }
    fn exit(&self) -> Exit {
        let unsettled = self.results.iter().any(|r| matches!(r.result.outcome, Outcome::Refused { .. }));
        if unsettled { Exit::Problems } else { Exit::Ok }
    }
}

impl Report for VerifyReport {
    fn human(&self, _env: &Env) -> String {
        format!("{} problems\n", self.problems.len())
    }
    fn exit(&self) -> Exit {
        if self.problems.is_empty() { Exit::Ok } else { Exit::Problems }
    }
}

fn row(label: &str, rest: &str) -> String {
    format!("{label:<9} {rest}\n")
}

fn stores_table(env: &Env, stores: &[Store]) -> String {
    let roots: Vec<String> = stores.iter().map(|s| env.show(&s.root)).collect();
    let kind_w = stores.iter().map(|s| s.kind.len()).max().unwrap_or(0);
    let root_w = roots.iter().map(String::len).max().unwrap_or(0);
    let mut out = String::new();
    for (s, root) in stores.iter().zip(&roots) {
        let mut line = format!(
            "  {:<kind_w$}  {:<root_w$}  {:>9}  {:>9}",
            s.kind,
            root,
            plural(s.files, "file"),
            fmt_bytes(s.bytes)
        );
        if let Some(marker) = &s.downloading {
            line.push_str(&format!("  downloading ({})", env.show(marker)));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn groups_text(env: &Env, groups: &[GroupPlan]) -> String {
    let mut out = String::new();
    if !groups.is_empty() {
        out.push_str("duplicates\n");
    }
    for g in groups {
        let head = format!("  {:>9}  {}", fmt_bytes(g.size), &g.hash.hex()[..8]);
        for (i, m) in g.members.iter().enumerate() {
            let lead = if i == 0 { head.clone() } else { " ".repeat(head.len()) };
            let (label, why) = match &m.role {
                Role::Keeper => ("keep", String::new()),
                Role::Target { .. } => ("clone", String::new()),
                Role::Shared => ("shared", String::new()),
                Role::Refused { refusal } => ("refuse", format!("  {refusal}")),
            };
            out.push_str(&format!("{lead}  {label:<6}  {}{why}\n", env.show(&m.path)));
        }
    }
    out
}

fn count_roles(groups: &[GroupPlan], f: impl Fn(&Role) -> bool) -> usize {
    groups.iter().flat_map(|g| &g.members).filter(|m| f(&m.role)).count()
}

/// ` (3 to clone, 1 refused, 2 already shared)`, leaving out zero counts.
fn tally(groups: &[GroupPlan]) -> String {
    let parts: Vec<String> = [
        (count_roles(groups, |r| matches!(r, Role::Target { .. })), "to clone"),
        (count_roles(groups, |r| matches!(r, Role::Refused { .. })), "refused"),
        (count_roles(groups, |r| matches!(r, Role::Shared)), "already shared"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) }
}

/// One line per volume from statfs: `freed 4.37 GB on / (free 112.40 GB -> 116.77 GB)`, or
/// `used` when free space fell.
fn space_text(env: &Env, space: &[SpaceDelta]) -> String {
    space
        .iter()
        .map(|d| {
            let verb = if d.free_after >= d.free_before { "freed" } else { "used" };
            format!(
                "{verb} {} on {} (free {} -> {})\n",
                fmt_bytes(d.free_after.abs_diff(d.free_before)),
                env.show(&d.mount),
                fmt_bytes(d.free_before),
                fmt_bytes(d.free_after)
            )
        })
        .collect()
}
