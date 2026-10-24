//! The plan: for every duplicate group, which inode keeps its bytes on each volume, which paths
//! become clones of it, and which are refused and why.
//!
//! A `Plan` is the first record of a run's journal, verbatim. Resume never re-plans; it continues
//! this exact plan, which is what makes "resume after SIGKILL" reach the same result as an
//! uninterrupted run. Every op still revalidates its target at execution time, so a stale plan can
//! only produce refusals, never a wrong replacement.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::journal::Managed;
use crate::matching::{Fingerprint, Hash, Matches, Unreadable};
use crate::platform::Holder;
use crate::stores::{Inventory, Store, StoreIx};
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
pub struct Volume {
    pub dev: u64,
    pub mount: PathBuf,
    pub fs_type: String,
    /// statfs-level belief; the clone call itself is the final word (ENOTSUP -> NoCloneSupport).
    pub clones: bool,
    pub free_before: u64,
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
    pub role: Role,
}

/// Invariants (checked by `debug_assert_plan`):
/// - each (group, volume) has at most one `Keeper`;
/// - every `Target { keeper }` names a `Keeper` member of the same group on the same `dev`;
/// - `Target.op` values across the plan are exactly `0..n`, increasing in (group, member) order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Role {
    Keeper,
    Target {
        op: u32,
        keeper: u16,
    },
    /// Already a clone of this volume's keeper made by an earlier run (journal says so and the
    /// inode still matches). Not counted as reclaimable.
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
}

/// Build the plan: gather facts with I/O (volumes via statfs, open holders via one batched lsof
/// over every would-be target), then decide purely in `decide`.
///
/// `managed` (from the journal) makes repeated runs idempotent: existing clones become `Shared`
/// and their keeper stays the keeper.
pub fn make_plan(env: &Env, inv: Inventory, matches: Matches, mode: Mode, managed: &[Managed]) -> anyhow::Result<Plan> {
    let _ = (env, inv, matches, mode, managed);
    todo!()
}

/// Facts `make_plan` gathers with I/O before deciding.
#[allow(dead_code)]
struct Facts<'a> {
    inv: &'a Inventory,
    volumes: &'a [Volume],
    holders: &'a Result<std::collections::BTreeMap<PathBuf, Vec<Holder>>, String>,
    euid: u32,
    managed: &'a [Managed],
}

/// Pure decision, in this order per group:
/// 1. members in a downloading store -> Refused(StoreDownloading). They are neither keeper nor target.
/// 2. split remaining members by `stat.id.dev`.
/// 3. per volume: keeper = the inode that is already a managed keeper, else the inode with most
///    paths, else smallest path. Deterministic for a given machine state.
/// 4. volume with exactly one eligible member and another volume holding the content ->
///    Refused(DifferentVolume). Volume with `clones == false` in Clone mode -> Refused(NoCloneSupport).
/// 5. per non-keeper member: managed clone of this keeper -> Shared; nlink > 1 -> MultiplyLinked;
///    uid != our euid -> ForeignOwner; open per lsof -> OpenByProcess / OpenCheckFailed;
///    else Target with the next op index.
#[allow(dead_code)]
fn decide(facts: &Facts<'_>, matches: Matches, mode: Mode) -> Plan {
    let _ = (facts, matches, mode);
    todo!()
}
