#![cfg(target_os = "linux")]
//! Real processes, real kills: a child commits transactions in a loop (each rewrites 20
//! files to the same generation number, renames and deletes some), and is SIGKILLed at a
//! random moment. After `recover`, every file must agree on one generation.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{leftovers, scratch};
use fstx::Transaction;

const FILES: usize = 20;
const CHILD_ENV: &str = "FSTX_SIGKILL_CHILD_ROOT";

/// The child's body. A no-op unless the parent started us with the env var.
#[test]
fn sigkill_child_entry() {
    let Ok(root) = std::env::var(CHILD_ENV) else {
        return;
    };
    let root = Path::new(&root);
    fstx::recover(root).unwrap();
    for generation in generation_of(root) + 1..=u64::MAX {
        let mut tx = Transaction::begin(root).unwrap();
        for i in 0..FILES {
            tx.write(format!("f{i}"), generation.to_string().repeat(512))
                .unwrap();
        }
        let (from, to) = if generation % 2 == 0 {
            ("odd", "even")
        } else {
            ("even", "odd")
        };
        tx.rename(format!("dir-{from}"), format!("dir-{to}"))
            .unwrap();
        tx.write(format!("dir-{to}/gen"), generation.to_string())
            .unwrap();
        tx.commit().unwrap();
    }
}

fn generation_of(root: &Path) -> u64 {
    let mut gens = Vec::new();
    for i in 0..FILES {
        let s = fs::read_to_string(root.join(format!("f{i}"))).unwrap();
        gens.push(s[..s.len() / 512].parse::<u64>().unwrap());
    }
    let g = gens[0];
    assert!(gens.iter().all(|&x| x == g), "torn state: {gens:?}");
    let dir = if g % 2 == 0 { "dir-even" } else { "dir-odd" };
    let other = if g % 2 == 0 { "dir-odd" } else { "dir-even" };
    assert!(
        !root.join(other).exists(),
        "both directories exist at generation {g}"
    );
    if g > 0 {
        assert_eq!(
            fs::read_to_string(root.join(dir).join("gen")).unwrap(),
            g.to_string()
        );
    }
    g
}

#[test]
fn random_sigkills_leave_old_or_new() {
    let d = scratch();
    let root = d.path();
    let mut tx = Transaction::begin(root).unwrap();
    for i in 0..FILES {
        tx.write(format!("f{i}"), "0".repeat(512)).unwrap();
    }
    tx.create_dir_all("dir-even").unwrap();
    tx.commit().unwrap();

    let exe = std::env::current_exe().unwrap();
    let mut seed = 0x2545F4914F6CDD1Du64;
    let mut last = 0;
    let mut advanced = 0;
    for _ in 0..200 {
        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "sigkill_child_entry",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        std::thread::sleep(Duration::from_micros(5_000 + seed % 150_000));
        child.kill().unwrap(); // SIGKILL
        child.wait().unwrap();

        fstx::recover(root).unwrap();
        let g = generation_of(root);
        assert!(g >= last, "generation went backwards: {last} -> {g}");
        if g > last {
            advanced += 1;
        }
        last = g;
        assert!(leftovers(root).is_empty(), "{:?}", leftovers(root));
    }
    eprintln!("200 kills; the committed generation advanced {advanced} times (now {last})");
    assert!(advanced > 0);
}
