//! End-to-end tests of the `fstx` binary on real directories.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fstx"))
}

fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("fstx-cli-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

/// path -> content ("<dir>" for directories), excluding `.fstx`.
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if rel == ".fstx" {
                continue;
            }
            if p.is_dir() {
                out.insert(rel, "<dir>".into());
                walk(root, &p, out);
            } else {
                out.insert(
                    rel,
                    fs::read_to_string(&p).unwrap_or_else(|_| "<binary>".into()),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn apply(root: &Path, changes: &str, extra: &[&str]) -> Output {
    let mut child = bin()
        .args(["apply", "-C"])
        .arg(root)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(changes.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

fn seed(root: &Path) {
    fs::write(root.join("app.toml"), "name = \"demo\"\nversion = 1\n").unwrap();
    fs::write(root.join("old.txt"), "old").unwrap();
    fs::create_dir(root.join("cache")).unwrap();
    fs::write(root.join("cache/x"), "x").unwrap();
}

#[test]
fn applies_every_op_together() {
    let d = scratch();
    let root = d.path();
    seed(root);
    let out = apply(
        root,
        r#"{"ops":[
            {"op":"replace","path":"app.toml","find":"version = 1","replace":"version = 2"},
            {"op":"mkdir","path":"bin"},
            {"op":"write","path":"bin/run.sh","content":"echo run\n","mode":"755"},
            {"op":"write","path":"logo.bin","content_base64":"AAEC"},
            {"op":"rename","from":"old.txt","to":"archive/old.txt"}
        ]}"#,
        &[],
    );
    // rename into a missing directory is refused: nothing applied.
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!root.join("bin").exists());

    let out = apply(
        root,
        r#"{"ops":[
            {"op":"replace","path":"app.toml","find":"version = 1","replace":"version = 2"},
            {"op":"mkdir","path":"bin"},
            {"op":"write","path":"bin/run.sh","content":"echo run\n","mode":"755"},
            {"op":"write","path":"logo.bin","content_base64":"AAEC"},
            {"op":"mkdir","path":"archive"},
            {"op":"rename","from":"old.txt","to":"archive/old.txt"},
            {"op":"remove_all","path":"cache"}
        ]}"#,
        &["--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let j = json(&out);
    assert_eq!(j["ok"], true);
    assert_eq!(j["changes"].as_array().unwrap().len(), 7);
    assert_eq!(
        fs::read_to_string(root.join("app.toml")).unwrap(),
        "name = \"demo\"\nversion = 2\n"
    );
    assert_eq!(
        fs::metadata(root.join("bin/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(fs::read(root.join("logo.bin")).unwrap(), [0, 1, 2]);
    assert_eq!(
        fs::read_to_string(root.join("archive/old.txt")).unwrap(),
        "old"
    );
    assert!(!root.join("cache").exists());
    assert!(
        fs::read_dir(root.join(".fstx"))
            .unwrap()
            .all(|e| e.unwrap().file_name() == "lock")
    );
}

#[test]
fn any_failure_changes_nothing() {
    let cases = [
        // later op fails
        (
            r#"{"ops":[{"op":"write","path":"new","content":"n"},{"op":"remove","path":"missing"}]}"#,
            1,
        ),
        // replace text not found
        (
            r#"{"ops":[{"op":"write","path":"new","content":"n"},{"op":"replace","path":"app.toml","find":"nope","replace":"x"}]}"#,
            1,
        ),
        // replace text found twice, once expected
        (
            r#"{"ops":[{"op":"replace","path":"app.toml","find":"\n","replace":";"}]}"#,
            1,
        ),
        // precondition: content changed since the agent read it
        (
            r#"{"expect":{"app.toml":{"sha256":"00"}},"ops":[{"op":"write","path":"app.toml","content":"x"}]}"#,
            1,
        ),
        // precondition: must not exist
        (
            r#"{"expect":{"old.txt":{"exists":false}},"ops":[{"op":"write","path":"old.txt","content":"x"}]}"#,
            1,
        ),
        // escaping the root
        (
            r#"{"ops":[{"op":"write","path":"../escape","content":"x"}]}"#,
            1,
        ),
        (
            r#"{"ops":[{"op":"write","path":".fstx/x","content":"x"}]}"#,
            1,
        ),
        // malformed input
        (r#"{"ops":[{"op":"write","path":"a"}]}"#, 2),
        (
            r#"{"ops":[{"op":"write","path":"a","content":"x","content_base64":"eA=="}]}"#,
            2,
        ),
        (
            r#"{"ops":[{"op":"write","path":"a","content":"x","mode":"999"}]}"#,
            2,
        ),
        (r#"{"ops":[{"op":"frobnicate","path":"a"}]}"#, 2),
        (
            r#"{"ops":[{"op":"write","path":"a","content":"x","colour":"blue"}]}"#,
            2,
        ),
        (r#"not json"#, 2),
    ];
    for (changes, code) in cases {
        let d = scratch();
        seed(d.path());
        let before = tree(d.path());
        let out = apply(d.path(), changes, &["--json"]);
        assert_eq!(
            out.status.code(),
            Some(code),
            "{changes}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(json(&out)["ok"], false);
        assert_eq!(tree(d.path()), before, "{changes} changed the tree");
        assert!(!d.path().parent().unwrap().join("escape").exists());
    }
}

#[test]
fn matching_precondition_applies() {
    let d = scratch();
    seed(d.path());
    let hash = format!(
        "{:x}",
        sha2_hex(&fs::read(d.path().join("app.toml")).unwrap())
    );
    let changes = format!(
        r#"{{"expect":{{"app.toml":{{"sha256":"{hash}"}},"new.txt":{{"exists":false}}}},
            "ops":[{{"op":"replace","path":"app.toml","find":"demo","replace":"prod"}},
                   {{"op":"write","path":"new.txt","content":"n"}}]}}"#
    );
    let out = apply(d.path(), &changes, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        fs::read_to_string(d.path().join("app.toml"))
            .unwrap()
            .contains("prod")
    );
}

/// Hex-formattable SHA-256 via the system `sha256sum`, independent of the tool's own code.
fn sha2_hex(data: &[u8]) -> HexString {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    let out = child.wait_with_output().unwrap();
    HexString(
        String::from_utf8(out.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string(),
    )
}

struct HexString(String);
impl std::fmt::LowerHex for HexString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[test]
fn dry_run_changes_nothing() {
    let d = scratch();
    seed(d.path());
    let before = tree(d.path());
    let out = apply(
        d.path(),
        r#"{"ops":[{"op":"remove_all","path":"cache"},{"op":"write","path":"n","content":"n"}]}"#,
        &["--dry-run"],
    );
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("would apply 2 change(s)"));
    assert_eq!(tree(d.path()), before);
}

#[test]
fn from_file_is_relative_to_the_change_set() {
    let d = scratch();
    let root = d.path().join("root");
    fs::create_dir(&root).unwrap();
    let spec = d.path().join("spec");
    fs::create_dir(&spec).unwrap();
    fs::write(spec.join("payload.txt"), "payload").unwrap();
    fs::write(
        spec.join("changes.json"),
        r#"{"ops":[{"op":"write","path":"p.txt","from_file":"payload.txt"}]}"#,
    )
    .unwrap();
    let out = bin()
        .args(["apply", "-C"])
        .arg(&root)
        .arg(spec.join("changes.json"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read_to_string(root.join("p.txt")).unwrap(), "payload");
}

#[test]
fn sync_copies_a_dotfiles_tree_atomically() {
    let d = scratch();
    let src = d.path().join("dotfiles");
    let home = d.path().join("home");
    fs::create_dir_all(src.join(".config/app")).unwrap();
    fs::create_dir_all(src.join(".git/objects")).unwrap();
    fs::create_dir(&home).unwrap();
    fs::write(src.join(".bashrc"), "alias ll='ls -l'\n").unwrap();
    fs::write(src.join(".config/app/settings.json"), "{}").unwrap();
    fs::write(src.join(".git/HEAD"), "ref").unwrap();
    fs::write(src.join("install.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(src.join("install.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("/etc/passwd", src.join("link")).unwrap();
    fs::write(home.join(".bashrc"), "old").unwrap();
    fs::create_dir(home.join(".config")).unwrap();
    fs::write(home.join(".config/keep"), "untouched").unwrap();

    let out = bin()
        .args(["sync"])
        .arg(&src)
        .arg("-C")
        .arg(&home)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let j = json(&out);
    assert_eq!(j["skipped"], serde_json::json!(["link"]));
    assert_eq!(
        fs::read_to_string(home.join(".bashrc")).unwrap(),
        "alias ll='ls -l'\n"
    );
    assert_eq!(
        fs::read_to_string(home.join(".config/app/settings.json")).unwrap(),
        "{}"
    );
    assert_eq!(
        fs::read_to_string(home.join(".config/keep")).unwrap(),
        "untouched"
    );
    assert_eq!(
        fs::metadata(home.join("install.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert!(!home.join(".git").exists());
    assert!(!home.join("link").exists());
}

#[test]
fn sync_refuses_to_replace_a_directory_with_a_file() {
    let d = scratch();
    let src = d.path().join("src");
    let home = d.path().join("home");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(home.join("conf")).unwrap();
    fs::write(src.join("a"), "a").unwrap();
    fs::write(src.join("conf"), "file where a dir is").unwrap();
    let before = tree(&home);
    let out = bin()
        .args(["sync"])
        .arg(&src)
        .arg("-C")
        .arg(&home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(tree(&home), before);
}

#[test]
fn recover_and_inspect_report_json() {
    let d = scratch();
    seed(d.path());
    for cmd in ["recover", "inspect"] {
        let out = bin()
            .arg(cmd)
            .arg("-C")
            .arg(d.path())
            .arg("--json")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(json(&out)["ok"], true);
    }
}

/// The tool itself is SIGKILLed mid-apply many times; after `fstx recover`, all files agree.
#[test]
fn killed_apply_is_all_or_nothing() {
    let d = scratch();
    let root = d.path();
    let files = 40;
    let gen_ops = |g: u32| {
        let ops: Vec<String> = (0..files)
            .map(|i| {
                format!(
                    r#"{{"op":"write","path":"f{i}","content":"{}"}}"#,
                    g.to_string().repeat(1000)
                )
            })
            .collect();
        format!(r#"{{"ops":[{}]}}"#, ops.join(","))
    };
    assert!(apply(root, &gen_ops(0), &[]).status.success());
    let mut seed = 0x9E3779B97F4A7C15u64;
    for g in 1..=60u32 {
        let mut child = bin()
            .args(["apply", "-C"])
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(gen_ops(g).as_bytes())
            .unwrap();
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        std::thread::sleep(std::time::Duration::from_micros(seed % 30_000));
        let _ = child.kill();
        child.wait().unwrap();
        let out = bin().args(["recover", "-C"]).arg(root).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let contents: Vec<String> = (0..files)
            .map(|i| fs::read_to_string(root.join(format!("f{i}"))).unwrap())
            .collect();
        assert!(
            contents.iter().all(|c| *c == contents[0]),
            "torn state after kill {g}"
        );
    }
}
