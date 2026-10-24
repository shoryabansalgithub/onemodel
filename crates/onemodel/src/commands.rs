use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::bail;
use serde::Serialize;

use crate::journal::{self, CrashPoint, Journal, Record, RunId, RunLog, RunWriter, crash_point};
use crate::matching::{self, Fingerprint, Unreadable};
use crate::plan::{self, GroupPlan, Mode, Op, Plan, Role};
use crate::platform::{self, Volume};
use crate::replace::{self, Ctx, Outcome};
use crate::stores::{self, ClaimBroken, Inventory, Store};
use crate::{Env, Exit, Report, fmt_bytes, plural};

#[derive(Serialize)]
pub struct ScanReport {
    pub stores: Vec<Store>,
    pub groups: Vec<GroupPlan>,
    pub reclaimable: u64,
    pub unreadable: Vec<Unreadable>,
    pub runs: Vec<RunSummary>,
    pub interrupted: Option<RunId>,
}

#[derive(Serialize)]
pub struct RunSummary {
    pub id: RunId,
    pub managed_files: u32,
    pub managed_bytes: u64,
}

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
    let Inventory { stores, files } = stores::discover(env)?;
    plan::make_plan(env, stores, matching::find_duplicates(files), mode, &journal::managed(runs))
}

#[derive(Serialize)]
pub struct DedupeReport {
    pub run: Option<RunId>,
    pub dry_run: bool,
    pub resumed: bool,
    pub mode: Mode,
    pub plan: Plan,
    pub results: Vec<OpResult>,
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

impl OpResult {
    fn new(op: Op<'_>, outcome: Outcome) -> OpResult {
        OpResult { op: op.ix, target: op.target.path.clone(), keeper: op.keeper.path.clone(), outcome }
    }
}

#[derive(Serialize)]
pub struct SpaceDelta {
    pub mount: PathBuf,
    pub free_before: u64,
    pub free_after: u64,
}

fn space_deltas(volumes: &[Volume], free_after: &[(u64, u64)]) -> Vec<SpaceDelta> {
    volumes
        .iter()
        .zip(free_after)
        .map(|(v, (_, after))| SpaceDelta { mount: v.mount.clone(), free_before: v.free, free_after: *after })
        .collect()
}

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
    let mut cx = Ctx::new(run, &lsof, writer);
    let mut results = Vec::new();
    for op in run.plan.ops() {
        let outcome = replace::drive(&mut cx, op, &run.ops[op.ix as usize])?;
        results.push(OpResult::new(op, outcome));
    }
    crash_point(CrashPoint::BeforeEnd);
    let free_after = measure(&run.plan.volumes)?;
    let space = space_deltas(&run.plan.volumes, &free_after);
    cx.writer.append(&Record::End { free_after })?;
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
        let mut cx = Ctx::new(interrupted, &lsof, &mut writer);
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
            volumes.push(Volume { free: platform::free_bytes(&v.mount)?, ..v.clone() });
        }
    }
    let mut results = Vec::new();
    for log in selected {
        let mut writer = journal.reopen(log)?;
        let mut cx = Ctx::new(log, &lsof, &mut writer);
        let ops: Vec<_> = log.plan.ops().collect();
        for op in ops.into_iter().rev() {
            if !only.is_empty() && !only.iter().any(|p| op.target.path.starts_with(p)) {
                continue;
            }
            if let Some(outcome) = replace::drive_undo(&mut cx, op, &log.ops[op.ix as usize])? {
                results.push(UndoResult { run: log.id.clone(), result: OpResult::new(op, outcome) });
            }
        }
    }
    let space = space_deltas(&volumes, &measure(&volumes)?);
    Ok(UndoReport { results, space })
}

#[derive(Serialize)]
pub struct VerifyReport {
    pub checked_files: u32,
    pub checked_bytes: u64,
    pub stores: u32,
    pub claims: u32,
    pub problems: Vec<Problem>,
    pub released: Vec<PathBuf>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Problem {
    ContentChanged { path: PathBuf },
    Missing { path: PathBuf },
    Unreadable { path: PathBuf, error: String },
    Interrupted { run: RunId },
    ClaimMissing { store: String, path: PathBuf },
    ClaimSizeMismatch { store: String, path: PathBuf, expected: u64, actual: u64 },
    ClaimDigestMismatch { store: String, path: PathBuf },
    LinkBroken { store: String, link: PathBuf },
    ClaimsUnreadable { store: String, root: PathBuf, error: String },
}

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
            let Err(broken) = claim.check(|p| deep || ours.contains(p)) else { continue };
            let (store, path) = (store.clone(), claim.path().to_path_buf());
            report.problems.push(match broken {
                ClaimBroken::Missing => Problem::ClaimMissing { store, path },
                ClaimBroken::SizeMismatch { expected, actual } => {
                    Problem::ClaimSizeMismatch { store, path, expected, actual }
                }
                ClaimBroken::DigestMismatch => Problem::ClaimDigestMismatch { store, path },
                ClaimBroken::Unreadable(e) => Problem::Unreadable { path, error: e.to_string() },
                ClaimBroken::LinkBroken => Problem::LinkBroken { store, link: path },
            });
        }
    }
    Ok(report)
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
                "dry run: would free up to {}{}\n",
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
}

impl Report for UndoReport {
    fn human(&self, env: &Env) -> String {
        let mut out = String::new();
        for r in &self.results {
            let path = env.show(&r.result.target);
            out.push_str(&match &r.result.outcome {
                Outcome::Restored => row("restored", &path),
                Outcome::AlreadyIndependent { note } => {
                    row("skipped", &format!("{path}  already independent ({note})"))
                }
                Outcome::Refused { refusal } => row("refused", &format!("{path}  {refusal}")),
                Outcome::Replaced => String::new(),
            });
        }
        if self.results.is_empty() {
            out.push_str("nothing to undo\n");
        } else {
            out.push_str(&space_text(env, &self.space));
        }
        if self.exit() != Exit::Ok {
            out.push_str("some files are still shared; run `onemodel undo` again once the reason is gone\n");
        }
        out
    }
    fn exit(&self) -> Exit {
        let unsettled = self.results.iter().any(|r| matches!(r.result.outcome, Outcome::Refused { .. }));
        if unsettled { Exit::Problems } else { Exit::Ok }
    }
}

impl Report for VerifyReport {
    fn human(&self, env: &Env) -> String {
        let mut out = String::new();
        for p in &self.problems {
            let (what, path) = match p {
                Problem::ContentChanged { path } => ("content changed".to_string(), path),
                Problem::Missing { path } => ("missing".to_string(), path),
                Problem::Unreadable { path, error } => (format!("unreadable ({error})"), path),
                Problem::Interrupted { run } => {
                    out.push_str(&row(
                        "problem",
                        &format!("run {run} was interrupted; run `onemodel dedupe` or `onemodel undo`"),
                    ));
                    continue;
                }
                Problem::ClaimMissing { store, path } => (format!("{store} file missing"), path),
                Problem::ClaimSizeMismatch { store, path, expected, actual } => {
                    (format!("{store} file is {actual} bytes, its manifest says {expected}"), path)
                }
                Problem::ClaimDigestMismatch { store, path } => {
                    (format!("{store} file does not match its digest"), path)
                }
                Problem::LinkBroken { store, link } => (format!("{store} link is broken"), link),
                Problem::ClaimsUnreadable { store, root, error } => {
                    (format!("{store} metadata unreadable ({error})"), root)
                }
            };
            out.push_str(&row("problem", &format!("{what}  {}", env.show(path))));
        }
        for path in &self.released {
            out.push_str(&row("released", &format!("{}  replaced by an app since dedupe", env.show(path))));
        }
        let summary = format!(
            "{} ({}) hashed, {} checked in {}",
            plural(self.checked_files, "managed file"),
            fmt_bytes(self.checked_bytes),
            plural(self.claims, "store claim"),
            plural(self.stores, "store")
        );
        out.push_str(&match self.problems.len() {
            0 => format!("ok: {summary}\n"),
            n => format!("{}: {summary}\n", plural(n as u64, "problem")),
        });
        out
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
