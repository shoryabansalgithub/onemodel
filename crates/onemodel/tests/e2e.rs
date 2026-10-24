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
const SIZE: usize = 4 * MIB;
const XATTR: &str = "user.onemodel.test";

fn content(seed: &str) -> Vec<u8> {
    let mut out = vec![0u8; SIZE];
    blake3::Hasher::new().update(seed.as_bytes()).finalize_xof().fill(&mut out);
    out
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Disk(Vec<String>);

fn disk(fx: &Fixture) -> Disk {
    let mut lines = Vec::new();
    let mut inodes: BTreeMap<(u64, u64), Vec<String>> = BTreeMap::new();
    for entry in walkdir::WalkDir::new(&fx.home).min_depth(1).follow_links(false) {
        let path = entry.unwrap().into_path();
        let rel = fx.name(&path.to_string_lossy());
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path).unwrap();
            let resolves = fs::metadata(&path).is_ok_and(|m| m.is_file());
            lines.push(format!("link {rel} -> {} resolves={resolves}", fx.name(&target.to_string_lossy())));
        } else if meta.is_dir() {
            lines.push(format!("dir  {rel}"));
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

    let human = String::from_utf8(fx.run(&["scan"]).stdout).unwrap();
    assert!(human.contains("keep    ~/.lmstudio/models/pub/repo/a.gguf\n"), "{human}");
    assert!(human.contains(&format!("clone   ~/{}\n", LLAMA_A)), "{human}");
    assert!(human.ends_with("reclaimable 25.2 MB (6 to clone)\n"), "{human}");
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
fn dedupe_human_output_is_short_and_names_each_file() {
    let Some(fx) = Fixture::new() else { return };
    let out = fx.run(&["dedupe"]);
    assert!(out.status.success(), "{}", text(&out));
    let human = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = human.lines().collect();
    assert_eq!(lines.len(), 7, "{human}");
    assert!(lines.contains(&format!("replaced  ~/{LLAMA_A}  (clone of ~/{LM_A})").as_str()), "{human}");
    assert!(lines[6].starts_with("freed "), "{human}");
    let verify = String::from_utf8(fx.run(&["verify"]).stdout).unwrap();
    assert_eq!(verify, "ok: 9 managed files (37.7 MB) hashed, 6 store claims checked in 5 stores\n");
    let again = String::from_utf8(fx.run(&["dedupe"]).stdout).unwrap();
    assert_eq!(again, "nothing to replace; 6 files are already shared\n");
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
#[test]
fn savings_are_measured_from_free_space() {
    let Some(mut fx) = Fixture::empty() else { return };
    let vol = Volume::attach(&fx, "APFS", 200);
    fx.extra_args = vec!["--path".into(), vol.mount.display().to_string()];
    let big: Vec<u8> = content("big").repeat(10);
    put(&vol.mount.join("one.gguf"), &big, 0o644, 1_700_000_001, None);
    put(&vol.mount.join("two.gguf"), &big, 0o644, 1_700_000_002, None);

    let report = fx.json(&["dedupe"]);
    assert_eq!(report["results"].as_array().unwrap().len(), 1);
    let freed = freed_on(&report, &vol.mount);
    let dup = big.len() as i64;
    assert!((freed - dup).abs() < 4 * MIB as i64, "freed {freed} bytes for a {dup}-byte duplicate");
    assert_eq!(fs::read(vol.mount.join("two.gguf")).unwrap(), big);

    let filler = vol.mount.join("filler");
    let mut f = File::create(&filler).unwrap();
    while statfs_free(&vol.mount) > 30 * MIB as u64 {
        f.write_all(&[1u8; MIB]).unwrap();
        f.sync_all().unwrap();
    }
    drop(f);
    let refused = fx.json_code(&["undo"], 2);
    assert_eq!(refused["results"][0]["refusal"]["reason"], json!("not_enough_space"));
    fs::remove_file(&filler).unwrap();

    let undo = fx.json(&["undo"]);
    assert_eq!(undo["results"][0]["outcome"], json!("restored"));
    let used = -freed_on(&undo, &vol.mount);
    assert!((used - dup).abs() < 4 * MIB as i64, "undo used {used} bytes for a {dup}-byte file");
    eprintln!("APFS volume: dedupe freed {freed} and undo used {used} bytes for a {dup}-byte duplicate");
    assert_eq!(fs::read(vol.mount.join("two.gguf")).unwrap(), big);
}

#[cfg(target_os = "macos")]
fn freed_on(report: &Value, mount: &Path) -> i64 {
    let space = report["space"].as_array().unwrap();
    let d = space.iter().find(|d| Path::new(d["mount"].as_str().unwrap()) == mount).unwrap();
    d["free_after"].as_i64().unwrap() - d["free_before"].as_i64().unwrap()
}

#[cfg(target_os = "macos")]
fn statfs_free(path: &Path) -> u64 {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `c` is NUL-terminated and `st` is a valid out-pointer.
    assert_eq!(unsafe { libc::statfs(c.as_ptr(), st.as_mut_ptr()) }, 0);
    // SAFETY: statfs returned 0.
    let st = unsafe { st.assume_init() };
    st.f_bavail * st.f_bsize as u64
}

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
    let human = String::from_utf8(fx.run(&["dedupe", "--dry-run"]).stdout).unwrap();
    assert!(human.contains(&format!("open by sleep (pid {})", holder.0.id())), "{human}");

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
fn undo_refused_after_a_crash_leaves_no_temp_copy() {
    let Some(fx) = Fixture::new() else { return };
    fx.json(&["dedupe"]);
    killed(&fx, &["undo"], "undo-after-copy:1");
    let temp = disk(&fx).0.into_iter().find(|l| l.contains(".onemodel-")).expect("the killed undo left its copy");
    let dir = temp.split_whitespace().nth(1).unwrap().rsplit_once('/').unwrap().0.to_string();
    let target = [OLLAMA_A, LLAMA_A, HF_A, LLAMA_B, EXTRA_B, HF_C]
        .into_iter()
        .find(|t| t.rsplit_once('/').unwrap().0 == dir)
        .unwrap();
    let holder = Holder::open(&fx.path(target));
    let undo = fx.json_code(&["undo"], 2);
    assert_eq!(fx.outcomes(&undo).iter().filter(|(_, o)| o == "refused:open_by_process").count(), 1);
    let temps: Vec<String> = disk(&fx).0.into_iter().filter(|l| l.contains(".onemodel-")).collect();
    assert_eq!(temps, Vec::<String>::new());
    drop(holder);
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

#[test]
fn refuses_store_mid_download() {
    let sa = &digests()[0];
    let cases: [(&str, String, &[&str]); 4] = [
        ("ollama", format!(".ollama/models/blobs/sha256-{sa}-partial-0"), &[OLLAMA_A, OLLAMA_B]),
        ("huggingface", "hf/hub/models--org--z/blobs/abc.incomplete".to_string(), &[HF_A, HF_C]),
        ("llama.cpp", "Library/Caches/llama.cpp/x.gguf.downloadInProgress".to_string(), &[LLAMA_A, LLAMA_B]),
        ("lmstudio", ".lmstudio/models/pub/repo/new.gguf.part".to_string(), &[LM_A, LM_C]),
    ];
    for (store, marker, frozen) in cases {
        let Some(fx) = Fixture::new() else { return };
        put(&fx.home.join(&marker), b"partial", 0o644, 1_700_000_050, None);
        put(&fx.home.join("extra/x.gguf.crdownload"), b"partial", 0o644, 1_700_000_051, None);
        let before = disk(&fx);
        let report = fx.json(&["dedupe"]);
        let refused: Vec<String> = fx
            .roles(&report["plan"]["groups"])
            .into_iter()
            .filter(|(_, r)| r == "refused:store_downloading")
            .map(|(p, _)| p)
            .collect();
        let mut expected: Vec<String> = frozen.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(refused, expected, "{store}");
        let m = member(&fx, &report["plan"]["groups"], frozen[0]);
        assert_eq!(m["refusal"]["store"], json!(store));
        assert_eq!(m["refusal"]["marker"], json!(fx.home.join(&marker)));
        assert!(fx.outcomes(&report).iter().all(|(p, o)| o == "replaced" && !frozen.contains(&p.as_str())));
        assert!(
            fx.outcomes(&report).iter().any(|(p, _)| p == EXTRA_B),
            "{store}: a .crdownload in --path must not freeze it"
        );
        let after = disk(&fx);
        for rel in frozen {
            let line = |d: &Disk| d.0.iter().find(|l| l.starts_with(&format!("file {rel} "))).cloned();
            assert_eq!(line(&after), line(&before));
        }
    }
}

#[test]
fn refuses_multiply_linked_target() {
    let Some(fx) = Fixture::new() else { return };
    fs::hard_link(fx.path(LLAMA_B), fx.root.join("outside-1")).unwrap();
    fs::hard_link(fx.path(EXTRA_B), fx.root.join("outside-2")).unwrap();
    let report = fx.json(&["dedupe"]);
    let groups = &report["plan"]["groups"];
    assert_eq!(member(&fx, groups, LLAMA_B)["role"], json!("keeper"));
    assert_eq!(member(&fx, groups, EXTRA_B)["refusal"], json!({"reason": "multiply_linked", "nlink": 2}));
    assert_eq!(fx.outcomes(&report).iter().filter(|(p, _)| p == OLLAMA_B).count(), 1);
    assert_eq!(fs::metadata(fx.path(EXTRA_B)).unwrap().nlink(), 2);
}

#[cfg(target_os = "macos")]
#[test]
fn refuses_files_with_an_acl() {
    let Some(fx) = Fixture::new() else { return };
    let out = Command::new("/bin/chmod").args(["+a", "everyone allow read"]).arg(fx.path(LLAMA_B)).output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let report = fx.json(&["dedupe"]);
    assert_eq!(member(&fx, &report["plan"]["groups"], LLAMA_B)["refusal"], json!({"reason": "has_acl"}));
    assert_eq!(report["results"].as_array().unwrap().len(), 5);
}

#[cfg(target_os = "macos")]
struct Volume {
    mount: PathBuf,
}

#[cfg(target_os = "macos")]
impl Volume {
    fn attach(fx: &Fixture, fs_type: &str, mib: u32) -> Volume {
        let image = fx.root.join(format!("{}.dmg", fs_type.replace('+', "plus")));
        let mount = fx.root.join(format!("vol-{}", fs_type.replace('+', "plus")));
        let out = Command::new("/usr/bin/hdiutil")
            .args(["create", "-quiet", "-size", &format!("{mib}m"), "-fs", fs_type, "-volname", "onemodel-test"])
            .arg(&image)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        fs::create_dir_all(&mount).unwrap();
        let out = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&image)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        Volume { mount }
    }
}

#[cfg(target_os = "macos")]
impl Drop for Volume {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/hdiutil").args(["detach", "-quiet", "-force"]).arg(&self.mount).output();
    }
}

#[cfg(target_os = "macos")]
#[test]
fn refuses_across_volumes_and_without_clone_support() {
    let Some(mut fx) = Fixture::new() else { return };
    let vol = Volume::attach(&fx, "HFS+", 40);
    fx.extra_args = vec!["--path".into(), vol.mount.display().to_string()];
    put(&vol.mount.join("a.gguf"), &content("A"), 0o644, 1_700_000_061, None);
    put(&vol.mount.join("x1.gguf"), &content("X"), 0o644, 1_700_000_062, None);
    put(&vol.mount.join("x2.gguf"), &content("X"), 0o644, 1_700_000_063, None);
    let report = fx.json(&["dedupe"]);
    let groups = &report["plan"]["groups"];
    let a = member(&fx, groups, &vol.mount.join("a.gguf").display().to_string());
    assert_eq!(a["refusal"]["reason"], json!("different_volume"));
    assert_eq!(member(&fx, groups, &vol.mount.join("x1.gguf").display().to_string())["role"], json!("keeper"));
    assert_eq!(
        member(&fx, groups, &vol.mount.join("x2.gguf").display().to_string())["refusal"],
        json!({"reason": "no_clone_support", "fs_type": "hfs"})
    );
    assert_eq!(fx.outcomes(&report), six_replaced());
    let human = String::from_utf8(fx.run(&["scan"]).stdout).unwrap();
    assert!(human.contains("the hfs filesystem does not support clones"), "{human}");
    assert!(human.contains("no other copy on this volume, and clones cannot cross volumes"), "{human}");
}

#[test]
fn refuses_target_changed_after_plan() {
    let Some(fx) = Fixture::new() else { return };
    let out = fx.run_env(&["dedupe"], &[("ONEMODEL_CRASH_AT", "after-plan:1")]);
    assert_eq!(out.status.signal(), Some(9), "{}", text(&out));
    fs::set_permissions(fx.path(EXTRA_B), fs::Permissions::from_mode(0o600)).unwrap();
    let report = fx.json(&["dedupe"]);
    assert_eq!(report["resumed"], json!(true));
    let expected: Vec<(String, String)> = six_replaced()
        .into_iter()
        .map(|(p, o)| if p == EXTRA_B { (p, "refused:changed_since_scan".to_string()) } else { (p, o) })
        .collect();
    assert_eq!(fx.outcomes(&report), expected);
    assert_eq!(fs::metadata(fx.path(EXTRA_B)).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn refuses_when_bytes_differ_despite_plan() {
    let Some(fx) = Fixture::new() else { return };
    let out = fx.run_env(&["dedupe"], &[("ONEMODEL_CRASH_AT", "after-plan:1")]);
    assert_eq!(out.status.signal(), Some(9), "{}", text(&out));
    let mut keeper = OpenOptions::new().write(true).open(fx.path(OLLAMA_B)).unwrap();
    keeper.seek(SeekFrom::Start(3_000_000)).unwrap();
    keeper.write_all(b"changed").unwrap();
    drop(keeper);
    let report = fx.json(&["dedupe"]);
    let mismatches: Vec<&Value> =
        report["results"].as_array().unwrap().iter().filter(|r| r["outcome"] == json!("refused")).collect();
    assert_eq!(mismatches.len(), 2);
    for r in mismatches {
        assert_eq!(r["refusal"], json!({"reason": "content_mismatch", "offset": 3_000_000}));
    }
    assert_eq!(fs::read(fx.path(LLAMA_B)).unwrap(), content("B"));
    assert_eq!(fs::read(fx.path(EXTRA_B)).unwrap(), content("B"));
}

#[test]
fn second_instance_is_rejected() {
    use std::os::fd::AsRawFd;
    let Some(fx) = Fixture::new() else { return };
    fs::create_dir_all(&fx.state).unwrap();
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(fx.state.join("lock")).unwrap();
    // SAFETY: the descriptor is open for the duration of the call.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let out = fx.run(&["dedupe"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("another onemodel"), "{}", text(&out));
    assert_eq!(fx.run_files(), Vec::<String>::new());
}

const DEDUPE_POINTS: [(&str, u32); 8] = [
    ("after-plan", 1),
    ("after-clone", 6),
    ("after-metadata", 6),
    ("after-compare", 6),
    ("after-staged", 6),
    ("after-rename", 6),
    ("after-done", 6),
    ("before-end", 1),
];

const UNDO_POINTS: [(&str, u32); 3] = [("undo-after-copy", 6), ("undo-after-staged", 6), ("undo-after-rename", 6)];

fn cases(points: &[(&str, u32)]) -> Vec<String> {
    points.iter().flat_map(|(p, hits)| (1..=*hits).map(move |n| format!("{p}:{n}"))).collect()
}

fn matrix(specs: Vec<String>, case: impl Fn(&str) + Sync) {
    let queue = Mutex::new(specs.clone());
    let failures = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                while let Some(spec) = queue.lock().unwrap().pop() {
                    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(&spec))) {
                        let msg = e
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                        failures.lock().unwrap().push(format!("{spec}: {}", msg.unwrap_or_default()));
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{} of {} cases failed:\n{}", failures.len(), specs.len(), failures.join("\n\n"));
    eprintln!("{} kill/resume cases passed", specs.len());
}

fn killed(fx: &Fixture, args: &[&str], spec: &str) {
    let out = fx.run_env(args, &[("ONEMODEL_CRASH_AT", spec)]);
    assert_eq!(out.status.signal(), Some(9), "{spec} did not kill {args:?}\n{}", text(&out));
}

#[test]
fn sigkill_at_every_dedupe_point_then_resume_matches_uninterrupted_run() {
    let Some(reference) = Fixture::new() else { return };
    let original = disk(&reference);
    assert_eq!(reference.outcomes(&reference.json(&["dedupe"])), six_replaced());
    let expected = (disk(&reference), reference.roles(&reference.json(&["scan"])["groups"]));
    assert_eq!(expected.1, shared_roles());

    matrix(cases(&DEDUPE_POINTS), |spec| {
        let fx = Fixture::new().unwrap();
        killed(&fx, &["dedupe"], spec);
        let resumed = fx.json(&["dedupe"]);
        assert_eq!(resumed["resumed"], json!(true));
        assert_eq!(fx.outcomes(&resumed), six_replaced());
        assert_eq!((disk(&fx), fx.roles(&fx.json(&["scan"])["groups"])), expected);
        assert!(!disk(&fx).0.iter().any(|l| l.contains(".onemodel-")));
        assert_eq!(fx.json(&["verify"])["problems"], json!([]));
        assert_eq!(fx.json(&["dedupe"])["results"], json!([]));
        let undo = fx.json(&["undo"]);
        assert_eq!(undo["results"].as_array().unwrap().len(), 6);
        assert_eq!(disk(&fx), original);
        assert_eq!(fx.roles(&fx.json(&["scan"])["groups"]), fresh_roles());
    });
}

#[test]
fn sigkill_at_every_undo_point_then_rerun_matches_uninterrupted_undo() {
    let Some(reference) = Fixture::new() else { return };
    let original = disk(&reference);
    reference.json(&["dedupe"]);
    let undo = reference.json(&["undo"]);
    assert_eq!(reference.outcomes(&undo).len(), 6);
    assert!(reference.outcomes(&undo).iter().all(|(_, o)| o == "restored"));
    assert_eq!(disk(&reference), original);

    matrix(cases(&UNDO_POINTS), |spec| {
        let fx = Fixture::new().unwrap();
        fx.json(&["dedupe"]);
        killed(&fx, &["undo"], spec);
        let rerun = fx.json(&["undo"]);
        assert!(fx.outcomes(&rerun).iter().all(|(_, o)| o == "restored"), "{rerun}");
        assert_eq!(disk(&fx), original);
        assert_eq!(fx.json(&["undo"])["results"], json!([]));
        assert_eq!(fx.json(&["verify"])["problems"], json!([]));
        assert_eq!(fx.roles(&fx.json(&["scan"])["groups"]), fresh_roles());
    });
}

#[test]
fn sigkill_in_hardlink_mode_then_resume_matches_uninterrupted_run() {
    let Some(reference) = Fixture::new() else { return };
    assert_eq!(reference.outcomes(&reference.json(&["dedupe", "--hardlink"])), six_replaced());
    let expected = disk(&reference);
    let points: Vec<(&str, u32)> = DEDUPE_POINTS.into_iter().filter(|(p, _)| *p != "after-metadata").collect();
    matrix(cases(&points), |spec| {
        let fx = Fixture::new().unwrap();
        killed(&fx, &["dedupe", "--hardlink"], spec);
        let resumed = fx.json(&["dedupe"]);
        assert_eq!((resumed["resumed"].clone(), resumed["mode"].clone()), (json!(true), json!("hardlink")));
        assert_eq!(fx.outcomes(&resumed), six_replaced());
        assert_eq!(disk(&fx), expected);
        assert_eq!(fx.json(&["verify"])["problems"], json!([]));
    });
}

#[test]
fn sigkill_then_undo_restores_original_state() {
    matrix(cases(&[("after-staged", 6), ("after-rename", 6)]), |spec| {
        let fx = Fixture::new().unwrap();
        let original = disk(&fx);
        killed(&fx, &["dedupe"], spec);
        fx.json(&["undo"]);
        assert_eq!(disk(&fx), original);
        assert_eq!(fx.json(&["verify"])["problems"], json!([]));
        let fresh = fx.json(&["dedupe"]);
        assert_eq!(fresh["resumed"], json!(false));
        assert_eq!(fx.outcomes(&fresh), six_replaced());
    });
}
