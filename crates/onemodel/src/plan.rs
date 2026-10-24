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
    pub stores: Vec<Store>,
    pub volumes: Vec<Volume>,
    pub groups: Vec<GroupPlan>,
    pub unreadable: Vec<Unreadable>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupPlan {
    pub hash: Hash,
    pub size: u64,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub path: PathBuf,
    pub other_links: Vec<PathBuf>,
    pub store: StoreIx,
    pub stat: Fingerprint,
    #[serde(flatten)]
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Role {
    Keeper,
    Target { op: u32, keeper: u16 },
    Shared,
    Refused { refusal: Refusal },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    DifferentVolume { keeper_mount: PathBuf },
    NoCloneSupport { fs_type: String },
    OpenByProcess { holders: Vec<Holder> },
    OpenCheckFailed { detail: String },
    StoreDownloading { store: String, marker: PathBuf },
    MultiplyLinked { nlink: u64 },
    ForeignOwner { uid: u32 },
    ChangedSinceScan,
    ContentMismatch { offset: u64 },
    NotEnoughSpace { needed: u64, available: u64 },
    Io { step: String, error: String },
    HasAcl,
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

#[derive(Debug, Clone, Copy)]
pub struct Op<'p> {
    pub ix: u32,
    pub hash: Hash,
    pub size: u64,
    pub keeper: &'p Member,
    pub target: &'p Member,
}

impl Plan {
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

    pub fn reclaimable(&self) -> u64 {
        self.ops().map(|op| op.size).sum()
    }

    pub fn volume(&self, dev: u64) -> Option<&Volume> {
        self.volumes.iter().find(|v| v.dev == dev)
    }
}

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

struct Facts<'a> {
    stores: &'a [Store],
    volumes: &'a [Volume],
    holders: &'a Result<BTreeMap<PathBuf, Vec<Holder>>, String>,
    acl: &'a BTreeSet<PathBuf>,
    euid: u32,
    managed: &'a [Managed],
}

fn decide(facts: &Facts<'_>, groups: Vec<crate::matching::DupGroup>, mode: Mode) -> Vec<GroupPlan> {
    let managed_keepers: BTreeSet<(FileId, Hash)> = facts.managed.iter().map(|m| (m.keeper_id, m.hash)).collect();
    let managed_targets: BTreeSet<(FileId, Hash)> = facts.managed.iter().map(|m| (m.id_after_swap, m.hash)).collect();
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
                Member { path, other_links: paths.map(|(p, _)| p).collect(), store, stat: inode.stat, role }
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
