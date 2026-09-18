//! `cargo run --example crash_demo --features sim`
//!
//! Crashes a commit halfway through applying it (on the simulated filesystem), shows the
//! half-applied tree, then pulls the plug and runs recovery.

use fstx::sim::{SimConfig, SimFs};
use fstx::{Options, Transaction, inspect_on, recover_on};

fn show(label: &str, fs: &SimFs) {
    println!("{label}:");
    for (path, e) in fs.tree() {
        println!("  {path:<14} {}", String::from_utf8_lossy(&e.data));
    }
}

fn base() -> SimFs {
    let fs = SimFs::new(SimConfig::default());
    fs.put_file("config.toml", b"version = 1");
    fs.put_dir("plugins");
    fs.put_file("plugins/a.lua", b"-- old plugin");
    fs
}

fn tx(fs: &SimFs) -> fstx::Result<()> {
    let mut tx = Transaction::begin_on(Box::new(fs.clone()), &Options::new())?;
    tx.write("config.toml", "version = 2")?;
    tx.rename("plugins", "plugins-v2")?;
    tx.write("plugins-v2/b.lua", "-- new plugin")?;
    tx.commit()
}

fn main() {
    // Find a crash point in the middle of the apply phase.
    let mut budget = 0;
    let fs = loop {
        let fs = base();
        fs.crash_after(budget);
        let _ = tx(&fs);
        let e = fs.events();
        if e.contains(&"prepared") && !e.contains(&"committed") && fs.tree() != base().tree() {
            break fs;
        }
        budget += 1;
    };
    show("before", &base());
    show(
        &format!("\ncrashed after {budget} syscalls (half applied, visible in the page cache)"),
        &fs,
    );

    let after_power_loss = fs.durable();
    let insp = inspect_on(&after_power_loss).unwrap();
    println!(
        "\ninspect: {:?}",
        insp.transactions
            .iter()
            .map(|t| t.action)
            .collect::<Vec<_>>()
    );

    let report = recover_on(&after_power_loss, &Options::new()).unwrap();
    println!("recover: rolled back {:?}", report.rolled_back);
    show("\nafter power loss + recovery", &after_power_loss);
    assert_eq!(after_power_loss.tree(), base().tree());
}
