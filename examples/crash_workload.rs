//! Workload for `ci/crash-real/run.sh`: `init <root>`, `run <root> <n>`, `check <root>`.
//! Every commit rewrites all files to one generation number; `check` recovers and
//! verifies that all files agree.

use std::path::Path;

const FILES: usize = 16;

fn generation(root: &Path) -> u64 {
    let gens: Vec<u64> = (0..FILES)
        .map(|i| std::fs::read_to_string(root.join(format!("f{i}"))).unwrap().trim().parse().unwrap())
        .collect();
    assert!(gens.iter().all(|&g| g == gens[0]), "torn state: {gens:?}");
    gens[0]
}

fn main() -> fstx::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: crash_workload init|run|check <root> [n]");
        std::process::exit(2);
    }
    let root = Path::new(&args[2]);
    match args[1].as_str() {
        "init" => {
            std::fs::create_dir_all(root)?;
            let mut tx = fstx::Transaction::begin(root)?;
            for i in 0..FILES {
                tx.write(format!("f{i}"), "0")?;
            }
            tx.commit()?;
        }
        "run" => {
            let n: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
            for g in 1..=n {
                let mut tx = fstx::Transaction::begin(root)?;
                for i in 0..FILES {
                    tx.write(format!("f{i}"), g.to_string())?;
                }
                tx.commit()?;
            }
        }
        "check" => {
            fstx::recover(root)?;
            println!("generation {}", generation(root));
        }
        other => panic!("unknown command {other}"),
    }
    Ok(())
}
