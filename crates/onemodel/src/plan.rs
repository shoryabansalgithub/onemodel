//! The plan: for every duplicate group, which inode keeps its bytes on each volume, which paths
//! become clones of it, and which are refused and why.
//!
//! A `Plan` is the first record of a run's journal, verbatim. Resume never re-plans; it continues
//! this exact plan, which is what makes "resume after SIGKILL" reach the same result as an
//! uninterrupted run. Every op still revalidates its target at execution time, so a stale plan can
//! only produce refusals, never a wrong replacement.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::journal::Managed;
use crate::matching::{FileId, Fingerprint, Hash, Matches, Unreadable};
use crate::platform::{self, Holder, Volume};
use crate::stores::{Store, StoreIx};
use crate::{Env, fmt_bytes};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Clone,
    Hardlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub mode: Mode,
    /// Snapshot of `Inventory::stores`; `Member::store` indexes this. Lets a resumed run re-check
    /// "downloading" without rediscovering.
    pub stores: Vec<Store>,
    /// Every volume any member lives on, with free space measured at plan time.
    pub volumes: Vec<Volume>,
    /// Sorted (size desc, hash). Op indices are assigned in (group, member) order.
    pub groups: Vec<GroupPlan>,
    pub unreadable: Vec<Unreadable>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupPlan {
    pub hash: Hash,
    pub size: u64,
    /// One entry per inode in the `DupGroup`, in the same order. A hardlinked inode contributes its
    /// first path as `path` and the rest as `also`.
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub path: PathBuf,
    pub also: Vec<PathBuf>,
    pub store: StoreIx,
    pub stat: Fingerprint,
    #[serde(flatten)]
    pub role: Role,
}

/// Invariants (checked by `debug_assert_plan`):
/// - each (group, volume) has at most one `Keeper`;
/// - every `Target { keeper }` names a `Keeper` member of the same group on the same `dev`;
/// - `Target.op` values across the plan are exactly `0..n`, increasing in (group, member) order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Role {
    Keeper,
    Target {
        op: u32,
        keeper: u16,
    },
    /// Already a clone made by an earlier run (journal says so and the inode still matches).
    /// Not counted as reclaimable.
    Shared,
    Refused {
        refusal: Refusal,
    },
}

/// Why onemodel will not touch a file. Every variant renders as one plain sentence.
/// Persisted in the journal, so variants are only ever added, never renamed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// No other copy of this content on this file's volume; clones cannot cross volumes.
    DifferentVolume { keeper_mount: PathBuf },
    /// The filesystem cannot clone: decided from statfs at plan time, or from ENOTSUP / EOPNOTSUPP /
    /// EXDEV returned by the clone call itself.
    NoCloneSupport { fs_type: String },
    /// lsof lists this file as open or mapped by a running process.
    OpenByProcess { holders: Vec<Holder> },
    /// lsof could not run or its output could not be trusted. Fail closed.
    OpenCheckFailed { detail: String },
    /// The owning app is mid-download (partial file present in its store).
    StoreDownloading { store: String, marker: PathBuf },
    /// The target inode has other names (nlink > 1), possibly outside any store. Replacing one
    /// name would silently split a link the app may rely on. It can still be a keeper.
    MultiplyLinked { nlink: u64 },
    /// Not owned by us; a clone would be owned by us and chown needs root.
    ForeignOwner { uid: u32 },
    /// The target is not the inode we planned against (size, mtime, ctime or inode changed).
    ChangedSinceScan,
    /// The prepared clone differs from the target at this byte. The hash said equal; the bytes
    /// said no (keeper changed in place, or a collision). Never link on hash alone.
    ContentMismatch { offset: u64 },
    /// `undo`: not enough free space on this volume to rematerialize the file.
    NotEnoughSpace { needed: u64, available: u64 },
    /// An unexpected I/O error at a named step; the temp file was removed.
    Io { step: String, error: String },
    /// The file (keeper or target) carries a macOS extended ACL. clonefile would give the target the
    /// keeper's ACL and onemodel does not copy ACLs.
    HasAcl,
    /// The run was interrupted and then undone before this file was replaced.
    Interrupted,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::DifferentVolume { keeper_mount } => write!(
                f,
                "no other copy on this volume, and clones cannot cross volumes (the keeper is on {})",
                keeper_mount.display()
            ),
            Refusal::NoCloneSupport { fs_type } => write!(f, "the {fs_type} filesystem does not support clones"),
            Refusal::OpenByProcess { holders } => {
                let who: Vec<String> = holders.iter().map(|h| format!("{} (pid {})", h.command, h.pid)).collect();
                write!(f, "open by {}", who.join(", "))
            }
            Refusal::OpenCheckFailed { detail } => write!(f, "could not check whether it is open: {detail}"),
            Refusal::StoreDownloading { store, marker } => {
                let name =
                    marker.file_name().map_or_else(|| marker.display().to_string(), |n| n.to_string_lossy().into());
                write!(f, "{store} is downloading ({name} exists)")
            }
            Refusal::MultiplyLinked { nlink } => {
                write!(f, "it has {nlink} hard links, and replacing one name would split them")
            }
            Refusal::ForeignOwner { uid } => write!(f, "owned by another user (uid {uid})"),
            Refusal::ChangedSinceScan => write!(f, "it changed after it was scanned"),
            Refusal::ContentMismatch { offset } => write!(f, "its bytes differ from the keeper's at offset {offset}"),
            Refusal::NotEnoughSpace { needed, available } => write!(
                f,
                "not enough free space to restore it ({} needed, {} free)",
                fmt_bytes(*needed),
                fmt_bytes(*available)
            ),
            Refusal::Io { step, error } => write!(f, "{step} failed: {error}"),
            Refusal::HasAcl => write!(f, "it has an access control list, which onemodel does not copy"),
            Refusal::Interrupted => write!(f, "the run was interrupted and undone before this file was replaced"),
        }
    }
}

/// A view of one target: what `replace` needs, borrowed from the plan (no copies to drift).
#[derive(Debug, Clone, Copy)]
pub struct Op<'p> {
    pub ix: u32,
    pub hash: Hash,
    pub size: u64,
    pub keeper: &'p Member,
    pub target: &'p Member,
}

impl Plan {
    /// Targets in execution order (op index order).
    pub fn ops(&self) -> impl Iterator<Item = Op<'_>> {
        self.groups.iter().flat_map(|g| {
            g.members.iter().filter_map(move |m| match m.role {
                Role::Target { op, keeper } => {
                    Some(Op { ix: op, hash: g.hash, size: g.size, keeper: &g.members[keeper as usize], target: m })
                }
                _ => None,
            })
        })
    }

    pub fn op_count(&self) -> u32 {
        self.ops().count() as u32
    }

    /// Bytes that become shared if every target succeeds: sum of target sizes.
    pub fn reclaimable(&self) -> u64 {
        self.ops().map(|op| op.size).sum()
    }

    pub fn volume(&self, dev: u64) -> Option<&Volume> {
        self.volumes.iter().find(|v| v.dev == dev)
    }
}

/// Build the plan: gather facts with I/O (volumes via statfs, ACLs, open holders via one batched
/// lsof over every member), then decide purely in `decide`.
///
/// `managed` (from the journal) makes repeated runs idempotent: existing clones become `Shared`
/// and their keeper stays the keeper.
pub fn make_plan(
    env: &Env,
    stores: Vec<Store>,
    matches: Matches,
    mode: Mode,
    managed: &[Managed],
) -> anyhow::Result<Plan> {
    let mut volumes: Vec<Volume> = Vec::new();
    let mut acl = BTreeSet::new();
    let mut paths: Vec<&Path> = Vec::new();
    for g in &matches.groups {
        for inode in &g.inodes {
            let path = &inode.paths[0].0;
            if !volumes.iter().any(|v| v.dev == inode.stat.id.dev) {
                volumes.push(platform::volume(path)?);
            }
            if platform::has_acl(path).unwrap_or(false) {
                acl.insert(path.clone());
            }
            paths.push(path);
        }
    }
    let holders = platform::open_holders(&env.lsof(), &paths);
    let facts =
        Facts { stores: &stores, volumes: &volumes, holders: &holders, acl: &acl, euid: platform::euid(), managed };
    let groups = decide(&facts, matches.groups, mode);
    let plan = Plan { mode, stores, volumes, groups, unreadable: matches.unreadable };
    debug_assert_plan(&plan);
    Ok(plan)
}

/// Facts `make_plan` gathers with I/O before deciding.
struct Facts<'a> {
    stores: &'a [Store],
    volumes: &'a [Volume],
    holders: &'a Result<BTreeMap<PathBuf, Vec<Holder>>, String>,
    acl: &'a BTreeSet<PathBuf>,
    euid: u32,
    managed: &'a [Managed],
}

/// Pure decision, in this order per group:
/// 1. members in a downloading store -> Refused(StoreDownloading); members with an extended ACL ->
///    Refused(HasAcl). They are neither keeper nor target.
/// 2. split remaining members by `stat.id.dev`.
/// 3. per volume: keeper = the inode that is already a managed keeper, else a managed clone (the
///    original was replaced, so a re-downloaded original costs one op, not N), else the inode with
///    most links, else smallest path. Deterministic for a given machine state.
/// 4. the volume holding the most members is the group's home; a member alone on any other volume
///    -> Refused(DifferentVolume).
/// 5. per non-keeper member: managed clone -> Shared; volume without clones in Clone mode ->
///    NoCloneSupport; nlink > 1 -> MultiplyLinked; uid != our euid -> ForeignOwner; open per lsof
///    -> OpenByProcess / OpenCheckFailed; else Target with the next op index.
fn decide(facts: &Facts<'_>, groups: Vec<crate::matching::DupGroup>, mode: Mode) -> Vec<GroupPlan> {
    let managed_keepers: BTreeSet<(FileId, Hash)> = facts.managed.iter().map(|m| (m.keeper_id, m.hash)).collect();
    let managed_targets: BTreeSet<(FileId, Hash)> = facts.managed.iter().map(|m| (m.ino, m.hash)).collect();
    let mut next_op = 0u32;
    let mut out = Vec::new();
    for g in groups {
        let mut roles: Vec<Option<Role>> = vec![None; g.inodes.len()];
        for (ix, inode) in g.inodes.iter().enumerate() {
            let downloading = inode.paths.iter().find_map(|(_, s)| {
                let store = &facts.stores[s.0 as usize];
                store.downloading.as_ref().map(|m| (store.kind.clone(), m.clone()))
            });
            if let Some((store, marker)) = downloading {
                roles[ix] = Some(Role::Refused { refusal: Refusal::StoreDownloading { store, marker } });
            } else if facts.acl.contains(&inode.paths[0].0) {
                roles[ix] = Some(Role::Refused { refusal: Refusal::HasAcl });
            }
        }
        let mut by_dev: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for (ix, inode) in g.inodes.iter().enumerate().filter(|(ix, _)| roles[*ix].is_none()) {
            by_dev.entry(inode.stat.id.dev).or_default().push(ix);
        }
        let is_managed_keeper = |ix: usize| managed_keepers.contains(&(g.inodes[ix].stat.id, g.hash));
        let is_managed_target = |ix: usize| managed_targets.contains(&(g.inodes[ix].stat.id, g.hash));
        let keeper_of = |ixs: &[usize]| {
            *ixs.iter()
                .min_by_key(|&&ix| {
                    let i = &g.inodes[ix];
                    (
                        Reverse(is_managed_keeper(ix)),
                        Reverse(is_managed_target(ix)),
                        Reverse(i.stat.nlink),
                        &i.paths[0].0,
                    )
                })
                .expect("partitions are non-empty")
        };
        let volume_of =
            |dev: u64| facts.volumes.iter().find(|v| v.dev == dev).expect("every member's volume was measured");
        let home = by_dev.iter().max_by_key(|(_, ixs)| (ixs.len(), Reverse(ixs[0]))).map(|(dev, _)| volume_of(*dev));
        for (dev, ixs) in &by_dev {
            let keeper = keeper_of(ixs);
            let vol = volume_of(*dev);
            match home {
                Some(home) if ixs.len() == 1 && *dev != home.dev => {
                    let keeper_mount = home.mount.clone();
                    roles[keeper] = Some(Role::Refused { refusal: Refusal::DifferentVolume { keeper_mount } });
                    continue;
                }
                _ => roles[keeper] = Some(Role::Keeper),
            }
            for &ix in ixs.iter().filter(|&&ix| ix != keeper) {
                let inode = &g.inodes[ix];
                let path = &inode.paths[0].0;
                let refusal = if is_managed_target(ix) {
                    roles[ix] = Some(Role::Shared);
                    continue;
                } else if mode == Mode::Clone && !vol.clones {
                    Refusal::NoCloneSupport { fs_type: vol.fs_type.clone() }
                } else if inode.stat.nlink > 1 {
                    Refusal::MultiplyLinked { nlink: inode.stat.nlink }
                } else if inode.stat.uid != facts.euid {
                    Refusal::ForeignOwner { uid: inode.stat.uid }
                } else {
                    match facts.holders {
                        Err(detail) => Refusal::OpenCheckFailed { detail: detail.clone() },
                        Ok(open) => match open.get(path) {
                            Some(holders) => Refusal::OpenByProcess { holders: holders.clone() },
                            None => {
                                roles[ix] = Some(Role::Target { op: 0, keeper: keeper as u16 });
                                continue;
                            }
                        },
                    }
                };
                roles[ix] = Some(Role::Refused { refusal });
            }
        }
        let members = g
            .inodes
            .into_iter()
            .zip(roles)
            .map(|(inode, role)| {
                let mut role = role.expect("every member gets a role");
                if let Role::Target { op, .. } = &mut role {
                    *op = next_op;
                    next_op += 1;
                }
                let mut paths = inode.paths.into_iter();
                let (path, store) = paths.next().expect("an inode has at least one path");
                Member { path, also: paths.map(|(p, _)| p).collect(), store, stat: inode.stat, role }
            })
            .collect();
        out.push(GroupPlan { hash: g.hash, size: g.size, members });
    }
    out
}

fn debug_assert_plan(plan: &Plan) {
    if cfg!(debug_assertions) {
        for (expected, op) in plan.ops().enumerate() {
            assert_eq!(op.ix as usize, expected, "op indices are 0..n in (group, member) order");
            assert_eq!(op.keeper.role, Role::Keeper, "every target names a keeper");
            assert_eq!(op.keeper.stat.id.dev, op.target.stat.id.dev, "keeper and target share a volume");
        }
    }
}
