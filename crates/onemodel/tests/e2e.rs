//! End-to-end suite: every test runs the built binary against a fixture HOME in a tempdir, with
//! `env_clear()` plus PATH, HOME and every store variable pointing inside the fixture, and the
//! journal in a fixture `--state`. No test can see or touch a real model store. Assertions are on
//! what a user observes: files on disk (bytes, mode, mtime, xattrs, symlinks, inode sharing),
//! statfs free space, exit codes, and the `--json` reports.
//!
//! Platform seams, and nothing else:
//! - clones: the host filesystem. macOS tempdirs are APFS. On Linux, clone tests need
//!   `ONEMODEL_TEST_BTRFS=<dir on btrfs or xfs>` and say why they skipped otherwise.
//! - extra volumes: macOS `hdiutil` images attached at a fixture mountpoint, detached on drop.
//! - lsof: the real one, against a real child process; `ONEMODEL_LSOF` for "lsof cannot run".
//! - crash: `ONEMODEL_CRASH_AT=<point>:<n>`, a real SIGKILL the binary sends itself.

use std::collections::BTreeMap;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

const MIB: usize = 1 << 20;
/// Bigger than the 3 MiB sampled-hash window, so a byte at 1.25 MiB is outside every sample.
const SIZE: usize = 4 * MIB;
const XATTR: &str = "user.onemodel.test";

fn content(seed: &str) -> Vec<u8> {
    let mut out = vec![0u8; SIZE];
    blake3::Hasher::new().update(seed.as_bytes()).finalize_xof().fill(&mut out);
    out
}

/// sha256 hex of contents A, B and C: the names Ollama and the hub give those blobs.
fn digests() -> &'static [String; 3] {
    static D: OnceLock<[String; 3]> = OnceLock::new();
    D.get_or_init(|| ["A", "B", "C"].map(|s| sha256_hex(&content(s))))
}

fn with_byte(mut bytes: Vec<u8>, at: usize) -> Vec<u8> {
    bytes[at] ^= 0xff;
    bytes
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn tmp_root() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return Some(std::env::temp_dir());
    }
    let dir = std::env::var_os("ONEMODEL_TEST_BTRFS").map(PathBuf::from);
    if dir.is_none() {
        eprintln!("skipped: set ONEMODEL_TEST_BTRFS to a directory on btrfs or xfs to run clone tests");
    }
    dir
}

/// Write a file with exact metadata. xattr first, then mode (0444 forbids xattr writes), then mtime.
fn put(path: &Path, bytes: &[u8], mode: u32, mtime_s: u64, tag: Option<&str>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    if let Some(tag) = tag {
        xattr::set(path, XATTR, tag.as_bytes()).unwrap();
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    let mtime = UNIX_EPOCH + Duration::new(mtime_s, 123_456_789);
    File::open(path).unwrap().set_times(FileTimes::new().set_modified(mtime)).unwrap();
}

fn link(path: &Path, target: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    symlink(target, path).unwrap();
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    extra: PathBuf,
    extra_args: Vec<String>,
}

/// Relative paths of the standard fixture, under HOME.
const OLLAMA_A: &str = ".ollama/models/blobs/sha256-A";
const OLLAMA_B: &str = ".ollama/models/blobs/sha256-B";
const LM_A: &str = ".lmstudio/models/pub/repo/a.gguf";
const LM_C: &str = ".lmstudio/models/pub/repo/c.gguf";
const LLAMA_A: &str = "Library/Caches/llama.cpp/pub_repo_a.gguf";
const LLAMA_B: &str = "Library/Caches/llama.cpp/models--org--b/blobs/B";
const HF_A: &str = "hf/hub/models--org--a/blobs/A";
const HF_C: &str = "hf/hub/blobs/CC/C";
const EXTRA_B: &str = "extra/b-copy.gguf";

impl Fixture {
    fn empty() -> Option<Fixture> {
        let tmp = tempfile::Builder::new().prefix("onemodel-e2e-").tempdir_in(tmp_root()?).unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let home = root.join("home");
        let extra = home.join("extra");
        fs::create_dir_all(&extra).unwrap();
        Some(Fixture { _tmp: tmp, state: root.join("state"), root, home, extra, extra_args: Vec::new() })
    }

    /// A machine with five stores. Content A is in four stores, B in three, C in two; D and E are
    /// near-copies of A (D differs in the first byte, E only at 1.25 MiB, outside every sample);
    /// F is one inode with two hardlinked names. A hub `.lock` file holds C's bytes but is never a
    /// candidate. Every file has its own mode, mtime and xattr.
    /// An uninterrupted `dedupe` makes exactly six replacements.
    fn new() -> Option<Fixture> {
        let fx = Fixture::empty()?;
        let (a, b, c) = (content("A"), content("B"), content("C"));
        let [sa, sb, sc] = digests();
        let config = br#"{"model_format":"gguf"}"#;
        put(&fx.path(OLLAMA_A), &a, 0o644, 1_700_000_001, Some("ollama-a"));
        put(&fx.path(OLLAMA_B), &b, 0o644, 1_700_000_002, Some("ollama-b"));
        put(
            &fx.home.join(format!(".ollama/models/blobs/sha256-{}", sha256_hex(config))),
            config,
            0o644,
            1_700_000_003,
            None,
        );
        let manifest = json!({
            "schemaVersion": 2,
            "config": {"digest": format!("sha256:{}", sha256_hex(config)), "size": config.len()},
            "layers": [
                {"digest": format!("sha256:{sa}"), "size": SIZE},
                {"digest": format!("sha256:{sb}"), "size": SIZE},
            ],
        });
        let manifest_path = fx.home.join(".ollama/models/manifests/registry.ollama.ai/library/m/latest");
        put(&manifest_path, manifest.to_string().as_bytes(), 0o644, 1_700_000_004, None);

        put(&fx.path(HF_A), &a, 0o644, 1_700_000_011, Some("hf-a"));
        link(&fx.home.join("hf/hub/models--org--a/snapshots/rev1/a.gguf"), &format!("../../blobs/{sa}"));
        put(&fx.home.join("hf/hub/models--org--a/refs/main"), b"rev1", 0o644, 1_700_000_012, None);
        put(&fx.path(HF_C), &c, 0o444, 1_700_000_013, Some("hf-c"));
        put(&fx.home.join(format!("hf/hub/blobs/{}/{sc}.lock", &sc[..2])), &c, 0o644, 1_700_000_014, None);
        put(&fx.home.join("hf/hub/blobs/.huggingface-shared-blobs"), b"", 0o644, 1_700_000_015, None);
        link(&fx.home.join(format!("hf/hub/models--org--c/blobs/{sc}")), &format!("../../blobs/{}/{sc}", &sc[..2]));
        link(&fx.home.join("hf/hub/models--org--c/snapshots/rev2/c.gguf"), &format!("../../blobs/{sc}"));

        put(&fx.home.join(LM_A), &a, 0o640, 1_700_000_021, Some("lm-a"));
        put(&fx.home.join(LM_C), &c, 0o644, 1_700_000_022, Some("lm-c"));

        put(&fx.home.join(LLAMA_A), &a, 0o600, 1_700_000_031, Some("llama-a"));
        put(&fx.home.join("Library/Caches/llama.cpp/pub_repo_a.gguf.json"), b"{}", 0o644, 1_700_000_032, None);
        put(&fx.path(LLAMA_B), &b, 0o644, 1_700_000_033, Some("llama-b"));
        link(
            &fx.home.join("Library/Caches/llama.cpp/models--org--b/snapshots/rev3/b.gguf"),
            &format!("../../blobs/{sb}"),
        );

        put(&fx.home.join(EXTRA_B), &b, 0o644, 1_700_000_041, Some("extra-b"));
        put(&fx.home.join("extra/d.gguf"), &with_byte(a.clone(), 0), 0o644, 1_700_000_042, None);
        put(&fx.home.join("extra/e.gguf"), &with_byte(a.clone(), MIB + MIB / 4), 0o644, 1_700_000_043, None);
        put(&fx.home.join("extra/hl1"), &content("F"), 0o644, 1_700_000_044, None);
        fs::hard_link(fx.home.join("extra/hl1"), fx.home.join("extra/hl2")).unwrap();
        Some(fx)
    }

    /// The on-disk path of a fixture name (content-addressed names get their real digests).
    fn path(&self, rel: &str) -> PathBuf {
        let [sa, sb, sc] = digests();
        self.home.join(
            rel.replace("sha256-A", &format!("sha256-{sa}"))
                .replace("sha256-B", &format!("sha256-{sb}"))
                .replace("blobs/A", &format!("blobs/{sa}"))
                .replace("blobs/B", &format!("blobs/{sb}"))
                .replace("CC/C", &format!("{}/{sc}", &sc[..2])),
        )
    }

    /// Inverse of `path`: a reported absolute path back to its fixture name.
    fn name(&self, abs: &str) -> String {
        let [sa, sb, sc] = digests();
        let rel = abs.strip_prefix(&format!("{}/", self.home.display())).unwrap_or(abs);
        rel.replace(&format!("{}/{sc}", &sc[..2]), "CC/C")
            .replace(sa.as_str(), "A")
            .replace(sb.as_str(), "B")
            .replace(sc.as_str(), "C")
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_onemodel"));
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("OLLAMA_MODELS", self.home.join(".ollama/models"))
            .env("HF_HOME", self.home.join("hf"))
            .env("HF_HUB_CACHE", self.home.join("hf/hub"))
            .env("LLAMA_CACHE", self.home.join("Library/Caches/llama.cpp"))
            .envs(env.iter().copied())
            .arg("--state")
            .arg(&self.state)
            .args(["--min-size", "1M", "--path"])
            .arg(&self.extra)
            .args(&self.extra_args)
            .args(args)
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().unwrap()
    }

    /// Run with `--json`, require `code`, parse stdout.
    fn json_code(&self, args: &[&str], code: i32) -> Value {
        let out = self.run(&[&["--json"], args].concat());
        assert_eq!(out.status.code(), Some(code), "{args:?}\n{}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{args:?}: {e}\n{}", text(&out)))
    }

    fn json(&self, args: &[&str]) -> Value {
        self.json_code(args, 0)
    }

    fn run_files(&self) -> Vec<String> {
        match fs::read_dir(self.state.join("runs")) {
            Ok(dir) => dir.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// `(target, outcome or refusal reason)` for every op in a dedupe or undo report, sorted.
    fn outcomes(&self, report: &Value) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = report["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let tag = match r["outcome"].as_str().unwrap() {
                    "refused" => format!("refused:{}", r["refusal"]["reason"].as_str().unwrap()),
                    other => other.to_string(),
                };
                (self.name(r["target"].as_str().unwrap()), tag)
            })
            .collect();
        out.sort();
        out
    }

    /// `(path, role or refusal reason)` for every member of every group in a plan, sorted.
    fn roles(&self, groups: &Value) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = groups
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["members"].as_array().unwrap())
            .map(|m| {
                let tag = match m["role"].as_str().unwrap() {
                    "refused" => format!("refused:{}", m["refusal"]["reason"].as_str().unwrap()),
                    other => other.to_string(),
                };
                (self.name(m["path"].as_str().unwrap()), tag)
            })
            .collect();
        out.sort();
        out
    }
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    out.sort();
    out
}

/// Everything a user can observe about HOME, independent of inode numbers: per path its kind,
/// content hash, mode, mtime, xattrs or symlink target, then which paths share an inode.
/// onemodel temp files show up as paths, so a leftover temp makes two snapshots differ.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Disk(Vec<String>);

fn disk(fx: &Fixture) -> Disk {
    let mut lines = Vec::new();
    let mut inodes: BTreeMap<(u64, u64), Vec<String>> = BTreeMap::new();
    let mut stack = vec![fx.home.clone()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        for path in entries {
            let rel = fx.name(&path.to_string_lossy());
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                let resolves = fs::metadata(&path).is_ok_and(|m| m.is_file());
                lines.push(format!("link {rel} -> {} resolves={resolves}", fx.name(&target.to_string_lossy())));
            } else if meta.is_dir() {
                lines.push(format!("dir  {rel}"));
                stack.push(path);
            } else {
                let mut xattrs: Vec<String> = xattr::list(&path)
                    .unwrap()
                    .filter(|n| n != "com.apple.provenance")
                    .map(|n| {
                        let v = xattr::get(&path, &n).unwrap().unwrap_or_default();
                        format!("{}={}", n.to_string_lossy(), String::from_utf8_lossy(&v))
                    })
                    .collect();
                xattrs.sort();
                let hash = blake3::hash(&fs::read(&path).unwrap()).to_hex();
                lines.push(format!(
                    "file {rel} {} mode={:o} mtime={}.{} xattrs={xattrs:?}",
                    &hash[..16],
                    meta.mode() & 0o7777,
                    meta.mtime(),
                    meta.mtime_nsec()
                ));
                inodes.entry((meta.dev(), meta.ino())).or_default().push(rel);
            }
        }
    }
    lines.sort();
    let mut shared: Vec<String> = inodes
        .into_values()
        .filter(|p| p.len() > 1)
        .map(|mut p| {
            format!("same-inode {}", {
                p.sort();
                p.join(" ")
            })
        })
        .collect();
    shared.sort();
    lines.extend(shared);
    Disk(lines)
}

fn same_inode(fx: &Fixture, a: &str, b: &str) -> bool {
    let (ma, mb) = (fs::metadata(fx.path(a)).unwrap(), fs::metadata(fx.path(b)).unwrap());
    (ma.dev(), ma.ino()) == (mb.dev(), mb.ino())
}

/// The six replacements an uninterrupted dedupe of the standard fixture makes.
fn six_replaced() -> Vec<(String, String)> {
    pairs(&[
        (OLLAMA_A, "replaced"),
        (LLAMA_A, "replaced"),
        (HF_A, "replaced"),
        (LLAMA_B, "replaced"),
        (EXTRA_B, "replaced"),
        (HF_C, "replaced"),
    ])
}

/// Roles in the standard fixture before any dedupe: keepers are the smallest path per group.
fn fresh_roles() -> Vec<(String, String)> {
    pairs(&[
        (LM_A, "keeper"),
        (OLLAMA_A, "target"),
        (LLAMA_A, "target"),
        (HF_A, "target"),
        (OLLAMA_B, "keeper"),
        (LLAMA_B, "target"),
        (EXTRA_B, "target"),
        (LM_C, "keeper"),
        (HF_C, "target"),
    ])
}

fn shared_roles() -> Vec<(String, String)> {
    fresh_roles().into_iter().map(|(p, r)| (p, if r == "target" { "shared".to_string() } else { r })).collect()
}

#[test]
fn scan_reports_groups_and_changes_nothing() {
    let Some(fx) = Fixture::new() else { return };
    let before = disk(&fx);
    let scan = fx.json(&["scan"]);
    assert_eq!(fx.roles(&scan["groups"]), fresh_roles());
    assert_eq!(scan["reclaimable"], json!(6 * SIZE));
    let stores: Vec<(String, u64)> = scan["stores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["kind"].as_str().unwrap().to_string(), s["files"].as_u64().unwrap()))
        .collect();
    let expected = [("ollama", 2), ("huggingface", 2), ("lmstudio", 2), ("llama.cpp", 2), ("path", 5)];
    assert_eq!(stores, expected.map(|(k, n)| (k.to_string(), n)));
    assert_eq!(disk(&fx), before);
    assert_eq!(fx.run_files(), Vec::<String>::new());
}

#[test]
fn dry_run_matches_real_run_and_changes_nothing() {
    let Some(fx) = Fixture::new() else { return };
    let before = disk(&fx);
    let dry = fx.json(&["dedupe", "--dry-run"]);
    assert_eq!(dry["dry_run"], json!(true));
    assert_eq!(fx.roles(&dry["plan"]["groups"]), fresh_roles());
    assert_eq!(disk(&fx), before);
    assert_eq!(fx.run_files(), Vec::<String>::new());

    let real = fx.json(&["dedupe"]);
    let planned: Vec<(String, String)> =
        fresh_roles().into_iter().filter(|(_, r)| r == "target").map(|(p, _)| (p, "replaced".to_string())).collect();
    assert_eq!(fx.outcomes(&real), planned);
}

/// Clones leave every path looking exactly as before: same bytes, mode, mtime, xattrs, symlinks.
/// The targets do not pick up the keeper's mode or xattrs (each file has its own in the fixture).
#[test]
fn dedupe_replaces_targets_preserving_metadata_and_store_integrity() {
    let Some(fx) = Fixture::new() else { return };
    let before = disk(&fx);
    let report = fx.json(&["dedupe"]);
    assert_eq!(fx.outcomes(&report), six_replaced());
    assert_eq!(disk(&fx), before);
    assert_eq!(fs::metadata(fx.path(HF_C)).unwrap().mode() & 0o777, 0o444);

    let verify = fx.json(&["verify"]);
    assert_eq!(verify["problems"], json!([]));
    assert_eq!(verify["checked_files"], json!(9));
    assert_eq!(verify["claims"], json!(6));

    let again = fx.json(&["dedupe"]);
    assert_eq!(again["results"], json!([]));
    assert_eq!(again["run"], Value::Null);
    assert_eq!(fx.roles(&again["plan"]["groups"]), shared_roles());
    let scan = fx.json(&["scan"]);
    assert_eq!(scan["reclaimable"], json!(0));
    assert_eq!(fx.run_files().len(), 1);
    assert_eq!(disk(&fx), before);
}

#[test]
fn verify_fails_on_changed_managed_bytes_and_broken_store_links() {
    let Some(fx) = Fixture::new() else { return };
    fx.json(&["dedupe"]);
    let mut f = OpenOptions::new().write(true).open(fx.path(LLAMA_A)).unwrap();
    f.seek(SeekFrom::Start(7)).unwrap();
    f.write_all(b"x").unwrap();
    drop(f);
    let sc = &digests()[2];
    fs::remove_file(fx.home.join(format!("hf/hub/models--org--c/blobs/{sc}"))).unwrap();

    let verify = fx.json_code(&["verify"], 2);
    let problems: Vec<(String, String)> = verify["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let path = p.get("path").or_else(|| p.get("link")).unwrap().as_str().unwrap();
            (p["kind"].as_str().unwrap().to_string(), fx.name(path))
        })
        .collect();
    assert_eq!(
        problems,
        vec![
            ("content_changed".to_string(), LLAMA_A.to_string()),
            ("link_broken".to_string(), "hf/hub/models--org--c/snapshots/rev2/c.gguf".to_string()),
        ]
    );
}

#[test]
fn verify_deep_checks_every_ollama_blob_against_its_manifest() {
    let Some(fx) = Fixture::new() else { return };
    let mut f = OpenOptions::new().write(true).open(fx.path(OLLAMA_B)).unwrap();
    f.write_all(b"corrupt").unwrap();
    drop(f);
    assert_eq!(fx.json(&["verify"])["problems"], json!([]));
    let deep = fx.json_code(&["verify", "--deep"], 2);
    assert_eq!(deep["problems"].as_array().unwrap().len(), 1);
    assert_eq!(deep["problems"][0]["kind"], json!("claim_digest_mismatch"));
    assert_eq!(fx.name(deep["problems"][0]["path"].as_str().unwrap()), OLLAMA_B);
}

/// When an app re-downloads the keeper (new inode, same bytes), one op re-shares it with an
/// existing clone instead of re-cloning every target.
#[test]
fn redownloaded_keeper_costs_one_op() {
    let Some(fx) = Fixture::new() else { return };
    fx.json(&["dedupe"]);
    fs::remove_file(fx.path(LM_A)).unwrap();
    put(&fx.path(LM_A), &content("A"), 0o640, 1_700_000_099, Some("lm-a-again"));
    let report = fx.json(&["dedupe"]);
    assert_eq!(fx.outcomes(&report), pairs(&[(LM_A, "replaced")]));
    assert_eq!(fx.name(report["results"][0]["keeper"].as_str().unwrap()), OLLAMA_A);
    assert_eq!(fx.json(&["verify"])["problems"], json!([]));
    assert_eq!(fx.json(&["scan"])["reclaimable"], json!(0));
}

#[cfg(target_os = "macos")]
/// Undo restores bytes, mode, mtime and separate inodes. Per-path xattrs are lost in hardlink
/// mode, which the warning says.
#[test]
fn hardlink_mode_warns_and_links_and_undo_unlinks() {
    let Some(fx) = Fixture::new() else { return };
    let before = disk(&fx);
    let out = fx.run(&["--json", "dedupe", "--hardlink"]);
    assert!(out.status.success(), "{}", text(&out));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("If any app writes to one path, every other path changes too"), "{stderr}");
    assert!(same_inode(&fx, LM_A, OLLAMA_A) && same_inode(&fx, LM_A, LLAMA_A) && same_inode(&fx, LM_A, HF_A));
    assert!(same_inode(&fx, OLLAMA_B, LLAMA_B) && same_inode(&fx, OLLAMA_B, EXTRA_B));
    assert_eq!(fs::metadata(fx.path(LM_A)).unwrap().nlink(), 4);
    assert_eq!(fx.json(&["verify"])["problems"], json!([]));
    assert_eq!(fx.json(&["dedupe"])["results"], json!([]));

    fx.json(&["undo"]);
    assert_eq!(fs::metadata(fx.path(LM_A)).unwrap().nlink(), 1);
    let without_xattrs =
        |d: &Disk| d.0.iter().map(|l| l.split(" xattrs=").next().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(without_xattrs(&disk(&fx)), without_xattrs(&before));
}

/// A child process holding `path` open on fd 3. Killed on drop.
struct Holder(Child);

impl Holder {
    fn open(path: &Path) -> Holder {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec 3<\"$1\"; echo ready; exec sleep 600", "sh"])
            .arg(path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
        assert_eq!(line, "ready\n");
        Holder(child)
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn member<'a>(fx: &Fixture, groups: &'a Value, rel: &str) -> &'a Value {
    groups
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["members"].as_array().unwrap())
        .find(|m| fx.name(m["path"].as_str().unwrap()) == rel)
        .unwrap()
}

#[test]
fn refuses_file_open_by_a_process() {
    let Some(fx) = Fixture::new() else { return };
    let original = fs::metadata(fx.path(OLLAMA_A)).unwrap().ino();
    let holder = Holder::open(&fx.path(OLLAMA_A));
    let report = fx.json(&["dedupe"]);
    let m = member(&fx, &report["plan"]["groups"], OLLAMA_A);
    assert_eq!(m["role"], json!("refused"));
    assert_eq!(
        m["refusal"],
        json!({"reason": "open_by_process", "holders": [{"pid": holder.0.id(), "command": "sleep"}]})
    );
    assert_eq!(report["results"].as_array().unwrap().len(), 5);
    assert_eq!(fs::metadata(fx.path(OLLAMA_A)).unwrap().ino(), original);

    drop(holder);
    assert_eq!(fx.outcomes(&fx.json(&["dedupe"])), pairs(&[(OLLAMA_A, "replaced")]));
}

#[test]
fn undo_refuses_open_file_and_only_limits_scope() {
    let Some(fx) = Fixture::new() else { return };
    let before = disk(&fx);
    fx.json(&["dedupe"]);
    let only = fx.home.join(".ollama").display().to_string();
    let undo = fx.json(&["undo", "--only", &only]);
    assert_eq!(fx.outcomes(&undo), pairs(&[(OLLAMA_A, "restored")]));

    let holder = Holder::open(&fx.path(HF_C));
    let undo = fx.json_code(&["undo"], 2);
    assert_eq!(
        fx.outcomes(&undo),
        pairs(&[
            (LLAMA_A, "restored"),
            (HF_A, "restored"),
            (LLAMA_B, "restored"),
            (EXTRA_B, "restored"),
            (HF_C, "refused:open_by_process")
        ])
    );
    drop(holder);
    assert_eq!(fx.outcomes(&fx.json(&["undo"])), pairs(&[(HF_C, "restored")]));
    assert_eq!(fx.json(&["undo"])["results"], json!([]));
    assert_eq!(disk(&fx), before);
    assert_eq!(fx.roles(&fx.json(&["scan"])["groups"]), fresh_roles());
}

#[test]
fn refuses_everything_when_lsof_cannot_run() {
    let Some(fx) = Fixture::new() else { return };
    let before = disk(&fx);
    let out = fx.run_env(&["--json", "dedupe"], &[("ONEMODEL_LSOF", "/nonexistent/lsof")]);
    assert!(out.status.success(), "{}", text(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let expected: Vec<(String, String)> = fresh_roles()
        .into_iter()
        .map(|(p, r)| (p, if r == "target" { "refused:open_check_failed".to_string() } else { r }))
        .collect();
    assert_eq!(fx.roles(&report["plan"]["groups"]), expected);
    assert_eq!(report["results"], json!([]));
    assert_eq!(disk(&fx), before);
    assert_eq!(fx.run_files(), Vec::<String>::new());
}

#[test]
fn lmstudio_downloads_folder_from_settings_is_a_store() {
    let Some(fx) = Fixture::new() else { return };
    let folder = fx.home.join("lms-downloads");
    put(&folder.join("pub/repo/a-copy.gguf"), &content("A"), 0o644, 1_700_000_070, None);
    let settings = json!({"downloadsFolder": folder, "other": 1});
    put(&fx.home.join(".lmstudio/settings.json"), settings.to_string().as_bytes(), 0o644, 1_700_000_071, None);
    let scan = fx.json(&["scan"]);
    let roots: Vec<String> = scan["stores"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["kind"] == json!("lmstudio"))
        .map(|s| fx.name(s["root"].as_str().unwrap()))
        .collect();
    assert_eq!(roots, [".lmstudio/models", "lms-downloads"]);
    assert_eq!(member(&fx, &scan["groups"], "lms-downloads/pub/repo/a-copy.gguf")["role"], json!("target"));
}

#[test]
fn undo_rejects_an_unknown_run() {
    let Some(fx) = Fixture::new() else { return };
    fx.json(&["dedupe"]);
    let out = fx.run(&["undo", "--run", "1-1"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "onemodel: no run 1-1 in the journal\n");
    let run = fx.json(&["scan"])["runs"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(fx.outcomes(&fx.json(&["undo", "--run", &run])).len(), 6);
}
