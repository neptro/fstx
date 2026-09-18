//! One test per row of the recovery decision table (DESIGN.md §4), plus capability probing
//! and the read-only guarantee of `inspect`.

use fstx::sim::{SimConfig, SimFs};
use fstx::{Error, Options, RecoveryAction, Transaction, inspect_on, recover_on};

fn base() -> SimFs {
    let fs = SimFs::new(SimConfig::default());
    fs.put_file("a", b"old");
    fs.put_dir("d");
    fs.put_file("d/x", b"x");
    fs
}

fn ops(tx: &mut Transaction) -> fstx::Result<()> {
    tx.write("a", b"new")?;
    tx.rename("d", "e")?;
    tx.write("e/y", b"y")
}

/// Runs the transaction until the first call after `event` has happened, then kills the process.
fn crash_after_event(event: &str, extra: u64) -> (SimFs, SimFs) {
    for budget in 0.. {
        let fs = base();
        fs.crash_after(budget);
        let _ = Transaction::begin_on(Box::new(fs.clone()), &Options::new()).and_then(|mut tx| {
            ops(&mut tx)?;
            tx.commit()
        });
        if fs.events().iter().any(|e| *e == event) {
            let fs = base();
            fs.crash_after(budget + extra);
            let _ = Transaction::begin_on(Box::new(fs.clone()), &Options::new()).and_then(|mut tx| {
                ops(&mut tx)?;
                tx.commit()
            });
            return (fs.process_kill(), before_after());
        }
    }
    unreachable!()
}

fn before_after() -> SimFs {
    let fs = base();
    let mut tx = Transaction::begin_on(Box::new(fs.clone()), &Options::new()).unwrap();
    ops(&mut tx).unwrap();
    tx.commit().unwrap();
    fs
}

fn tx_name(fs: &SimFs) -> String {
    fs.private_tree().keys().find(|k| k.starts_with("tx-") && !k.contains('/')).expect("tx dir").clone()
}

fn assert_untouched(fs: &SimFs, f: impl FnOnce(&SimFs) -> fstx::Result<fstx::RecoveryReport>) -> Error {
    let tree = fs.tree();
    let name = tx_name(fs);
    let txdir = |fs: &SimFs| fs.private_tree().into_iter().filter(|(k, _)| k.starts_with(&name)).collect::<Vec<_>>();
    let before = txdir(fs);
    let err = f(fs).expect_err("must refuse");
    assert!(matches!(err, Error::RecoveryRequired { .. }), "{err}");
    assert_eq!(fs.tree(), tree, "tree touched");
    assert_eq!(txdir(fs), before, "transaction directory touched");
    err
}

fn recover(fs: &SimFs) -> fstx::Result<fstx::RecoveryReport> {
    recover_on(fs, &Options::new())
}

#[test]
fn never_prepared_is_discarded() {
    let fs = base();
    let before = fs.tree();
    let mut tx = Transaction::begin_on(Box::new(fs.clone()), &Options::new()).unwrap();
    ops(&mut tx).unwrap();
    std::mem::forget(tx); // process dies before commit
    let fs = fs.process_kill();
    assert_eq!(inspect_on(&fs).unwrap().transactions[0].action, RecoveryAction::Discard);
    let r = recover(&fs).unwrap();
    assert_eq!(r.discarded.len(), 1);
    assert_eq!(fs.tree(), before);
    assert!(fs.private_tree().is_empty());
}

#[test]
fn torn_journal_tmp_only_is_discarded() {
    let fs = base();
    let before = fs.tree();
    let mut tx = Transaction::begin_on(Box::new(fs.clone()), &Options::new()).unwrap();
    ops(&mut tx).unwrap();
    std::mem::forget(tx);
    let name = tx_name(&fs);
    fs.put_file(&format!(".fstx/{name}/journal.tmp"), b"{\"format\":1,\"tx\"");
    assert_eq!(recover(&fs).unwrap().discarded, vec![name]);
    assert_eq!(fs.tree(), before);
}

#[test]
fn backup_without_journal_requires_intervention() {
    let fs = base();
    let mut tx = Transaction::begin_on(Box::new(fs.clone()), &Options::new()).unwrap();
    ops(&mut tx).unwrap();
    std::mem::forget(tx);
    let name = tx_name(&fs);
    fs.put_file(&format!(".fstx/{name}/backup/0"), b"someone's data");
    assert_untouched(&fs, recover);
    assert_eq!(inspect_on(&fs).unwrap().transactions[0].action, RecoveryAction::RecoveryRequired);
}

#[test]
fn corrupt_journal_requires_intervention_and_touches_nothing() {
    let (fs, _) = crash_after_event("prepared", 3);
    let name = tx_name(&fs);
    fs.tamper_file(&format!(".fstx/{name}/journal"), b"{\"format\":1} garbage");
    let err = assert_untouched(&fs, recover);
    assert!(err.to_string().contains("checksum"), "{err}");
    // begin refuses too, and still touches nothing.
    assert_untouched(&fs, |fs| Transaction::begin_on(Box::new(fs.clone()), &Options::new()).map(|_| Default::default()));
}

#[test]
fn prepared_uncommitted_rolls_back() {
    let (fs, _) = crash_after_event("prepared", 2);
    assert_eq!(inspect_on(&fs).unwrap().transactions[0].action, RecoveryAction::RollBack);
    let r = recover(&fs).unwrap();
    assert_eq!(r.rolled_back.len(), 1);
    assert_eq!(fs.tree(), base().tree());
    assert!(fs.private_tree().is_empty());
}

#[test]
fn committed_is_only_cleaned_up() {
    let (fs, after) = crash_after_event("committed", 0);
    assert_eq!(inspect_on(&fs).unwrap().transactions[0].action, RecoveryAction::CleanUpCommitted);
    let r = recover(&fs).unwrap();
    assert_eq!(r.completed.len(), 1);
    assert_eq!(fs.tree(), after.tree());
}

#[test]
fn committed_plus_rollback_marker_requires_intervention() {
    let (fs, _) = crash_after_event("committed", 0);
    let name = tx_name(&fs);
    fs.put_file(&format!(".fstx/{name}/ROLLING_BACK"), b"");
    assert_untouched(&fs, recover);
}

#[test]
fn foreign_entry_at_a_token_location_requires_intervention() {
    let (fs, _) = crash_after_event("prepared", 0);
    // An outside writer drops a file at the backup slot of a base entry not yet detached.
    let insp = inspect_on(&fs).unwrap();
    let tok = insp.transactions[0]
        .tokens
        .iter()
        .find(|t| t.position.as_ref().unwrap().index == 0 && !t.locations[0].starts_with(".fstx"))
        .expect("a base token still at its origin");
    fs.put_file(&tok.locations[1].to_string_lossy(), b"intruder");
    let err = assert_untouched(&fs, recover);
    assert!(err.to_string().contains("unknown entry"), "{err}");
}

#[test]
fn inspect_never_mutates() {
    for (event, extra) in [("prepared", 0), ("prepared", 3), ("committed", 0)] {
        let (fs, _) = crash_after_event(event, extra);
        let m = fs.mutations();
        let c = fs.calls();
        inspect_on(&fs).unwrap();
        assert_eq!((fs.mutations(), fs.calls()), (m, c));
    }
}

#[test]
fn probe_rejects_filesystems_without_required_semantics() {
    for (cfg, what) in [
        (SimConfig { noreplace: false, ..SimConfig::default() }, "no-replace"),
        (SimConfig { stable_ids: false, ..SimConfig::default() }, "stable"),
    ] {
        let fs = SimFs::new(cfg);
        match Transaction::begin_on(Box::new(fs), &Options::new()) {
            Err(Error::UnsupportedFilesystem { missing }) => assert!(missing.iter().any(|m| m.contains(what)), "{missing:?}"),
            other => panic!("expected UnsupportedFilesystem, got {other:?}"),
        }
    }
}

#[test]
fn case_insensitive_filesystem_rejects_colliding_names() {
    let fs = SimFs::new(SimConfig { case_insensitive: true, ..SimConfig::default() });
    fs.put_file("Readme", b"r");
    let mut tx = Transaction::begin_on(Box::new(fs.clone()), &Options::new()).unwrap();
    assert!(tx.case_insensitive());
    assert!(matches!(tx.write("README", b"x"), Err(Error::CaseCollision { .. })));
    assert!(matches!(tx.create_dir_all("readme"), Err(Error::CaseCollision { .. })));
    tx.write("Readme", b"ok").unwrap();
    tx.write("other", b"o").unwrap();
    tx.commit().unwrap();
    assert_eq!(fs.tree()["Readme"].data, b"ok");
}
