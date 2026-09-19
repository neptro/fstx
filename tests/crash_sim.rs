//! Crash-consistency checking on the simulated filesystem (DESIGN.md §5, tests table).
//!
//! For each scenario and profile:
//! 1. run the transaction once without crashing to get the before/after snapshots;
//! 2. crash the commit at **every** counted syscall; for each crash take every
//!    power-loss outcome permitted by C1–C5 (all combinations when few enough, else a
//!    deterministic sample) plus a process kill;
//! 3. from each outcome, run recovery to completion and check the invariant; then crash
//!    recovery itself at every syscall and recurse (bounded depth, memoized).
//!
//! Checked everywhere: tree == before or after (after if `commit` returned Ok), `.fstx`
//! empty after a completed recovery, no protocol violations, and — once a recovery run has
//! stabilized — the durable potential Φ never increases (Lemma 3).

use std::collections::{BTreeMap, HashMap};

use fstx::sim::{Fate, Profile, SimConfig, SimFs, TreeEntry};
use fstx::vfs::Vfs;
use fstx::{Options, RecoveryAction, Transaction, inspect_on, recover_on};

type Tree = BTreeMap<String, TreeEntry>;
type Ops = fn(&mut Transaction) -> fstx::Result<()>;

struct Scenario {
    name: &'static str,
    base: fn(&SimFs),
    ops: Ops,
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "replace + create",
            base: |fs| {
                fs.put_file("a", b"old-a");
                fs.put_dir("d");
            },
            ops: |tx| {
                tx.write("a", b"new-a")?;
                tx.write("d/b", b"new-b")
            },
        },
        Scenario {
            name: "move dir + edit inside",
            base: |fs| {
                fs.put_dir("a");
                fs.put_file("a/x", b"x");
                fs.put_file("a/y", b"y");
            },
            ops: |tx| {
                tx.rename("a", "z")?;
                tx.remove("z/x")?;
                tx.write("z/n", b"n")
            },
        },
        Scenario {
            name: "swap identical files",
            base: |fs| {
                fs.put_file("a", b"same");
                fs.put_file("b", b"same");
            },
            ops: |tx| {
                tx.rename("a", "t")?;
                tx.rename("b", "a")?;
                tx.rename("t", "b")
            },
        },
        Scenario {
            name: "remove tree + new nested dirs",
            base: |fs| {
                fs.put_dir("old");
                fs.put_dir("old/sub");
                fs.put_file("old/sub/f", b"f");
                fs.put_symlink("old/link");
            },
            ops: |tx| {
                tx.remove_dir_all("old")?;
                tx.create_dir_all("new/deep")?;
                tx.write("new/deep/g", b"g")
            },
        },
    ]
}

fn run_tx(fs: &SimFs, ops: Ops) -> fstx::Result<()> {
    let mut tx = Transaction::begin_on(Box::new(fs.clone()), &Options::new())?;
    ops(&mut tx)?;
    tx.commit()
}

/// Deterministic outcome enumeration: all fate combinations if at most `cap`, else a sample.
fn outcomes(fs: &SimFs, cap: usize) -> Vec<SimFs> {
    let ops = fs.volatile_ops();
    let choices: Vec<Vec<Fate>> = ops
        .iter()
        .map(|&weak| {
            if weak {
                vec![Fate::Drop, Fate::Apply, Fate::BothNames]
            } else {
                vec![Fate::Drop, Fate::Apply]
            }
        })
        .collect();
    let total: u128 = choices.iter().map(|c| c.len() as u128).product();
    let mut combos: Vec<Vec<Fate>> = Vec::new();
    if total <= cap as u128 {
        for mut idx in 0..total {
            combos.push(
                choices
                    .iter()
                    .map(|c| {
                        let f = c[(idx % c.len() as u128) as usize];
                        idx /= c.len() as u128;
                        f
                    })
                    .collect(),
            );
        }
    } else {
        combos.push(vec![Fate::Drop; ops.len()]);
        combos.push(vec![Fate::Apply; ops.len()]);
        let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ ops.len() as u64;
        while combos.len() < cap {
            combos.push(
                choices
                    .iter()
                    .map(|c| {
                        seed = seed
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        c[((seed >> 33) as usize) % c.len()]
                    })
                    .collect(),
            );
        }
    }
    let mut out = vec![fs.process_kill()];
    out.extend(
        combos
            .iter()
            .enumerate()
            .map(|(i, f)| fs.power_loss(f, (i % 4) as u8)),
    );
    out
}

/// Φ = (phase rank, Σ token positions) of the durable image (DESIGN.md §4, Lemma 3).
fn phi(fs: &SimFs) -> (u8, usize) {
    let d = fs.durable();
    let private = d.private_tree();
    let insp = inspect_on(&d).expect("inspect");
    let Some(tx) = insp.transactions.first() else {
        return (0, 0);
    };
    let has = |m: &str| private.keys().any(|k| k == &format!("{}/{m}", tx.name));
    let rank = if has("COMMITTED") || has("ROLLED_BACK") {
        1
    } else if has("ROLLING_BACK") {
        2
    } else {
        3
    };
    let remaining = tx
        .tokens
        .iter()
        .map(|t| t.position.as_ref().map(|p| p.index).unwrap_or(0))
        .sum();
    (rank, remaining)
}

struct Ctx {
    before: Tree,
    after: Tree,
    caps: Vec<usize>,
    memo: HashMap<String, usize>,
    recoveries: usize,
}

fn check_final(ctx: &Ctx, fs: &SimFs, committed_ok: bool, trail: &str) {
    let tree = fs.tree();
    let is_before = tree == ctx.before;
    let is_after = tree == ctx.after;
    assert!(
        is_before || is_after,
        "{trail}: tree is neither before nor after:\n{tree:#?}"
    );
    if committed_ok {
        assert!(
            is_after,
            "{trail}: commit returned Ok but recovery produced the before-state"
        );
    }
    assert!(
        fs.private_leftovers().is_empty(),
        "{trail}: leftovers {:?}",
        fs.private_leftovers()
    );
    assert!(fs.violations().is_empty(), "{trail}: {:?}", fs.violations());
}

fn explore(
    ctx: &mut Ctx,
    state: &SimFs,
    committed_ok: bool,
    depth: usize,
    bound: Option<(u8, usize)>,
    trail: &str,
) {
    let key = state.fingerprint();
    if ctx.memo.get(&key).is_some_and(|&d| d <= depth) {
        return;
    }
    ctx.memo.insert(key, depth);
    assert!(
        state.violations().is_empty(),
        "{trail}: {:?}",
        state.violations()
    );

    // Recovery to completion.
    let clean = state.process_kill();
    let start = clean.calls();
    let insp = inspect_on(&clean).expect("inspect");
    for tx in &insp.transactions {
        assert_ne!(
            tx.action,
            RecoveryAction::RecoveryRequired,
            "{trail}: {:?}",
            tx.reason
        );
    }
    recover_on(&clean, &Options::new()).unwrap_or_else(|e| panic!("{trail}: recovery failed: {e}"));
    ctx.recoveries += 1;
    check_final(ctx, &clean, committed_ok, trail);
    let n = clean.calls() - start;

    if depth >= ctx.caps.len() {
        return;
    }
    for j in 0..n {
        let s = state.process_kill();
        s.crash_after(j);
        let _ = recover_on(&s, &Options::new());
        ctx.recoveries += 1;
        let mut next_bound = bound;
        let durable_phi = phi(&s);
        if let Some(b) = bound {
            assert!(
                durable_phi <= b,
                "{trail}/r{j}: Φ rose from {b:?} to {durable_phi:?}"
            );
        }
        if let Some(stab) = s.durable_at_stabilize() {
            let at_stab = phi(&stab);
            assert!(
                durable_phi <= at_stab,
                "{trail}/r{j}: Φ rose after stabilize {at_stab:?} -> {durable_phi:?}"
            );
            next_bound = Some(next_bound.map_or(durable_phi, |b| b.min(durable_phi)));
        }
        for (k, o) in outcomes(&s, ctx.caps[depth]).into_iter().enumerate() {
            if let Some(b) = next_bound {
                let p = phi(&o);
                assert!(
                    p <= b,
                    "{trail}/r{j}/o{k}: outcome Φ {p:?} above bound {b:?}"
                );
            }
            explore(
                ctx,
                &o,
                committed_ok,
                depth + 1,
                next_bound,
                &format!("{trail}/r{j}/o{k}"),
            );
        }
    }
}

fn check_scenario(sc: &Scenario, profile: Profile, apply_cap: usize, caps: Vec<usize>) -> usize {
    let cfg = SimConfig {
        profile,
        ..SimConfig::default()
    };
    let reference = SimFs::new(cfg);
    (sc.base)(&reference);
    let before = reference.tree();
    run_tx(&reference, sc.ops).expect("reference run");
    let after = reference.tree();
    assert_ne!(before, after);
    let total = reference.calls();
    let mut ctx = Ctx {
        before,
        after,
        caps,
        memo: HashMap::new(),
        recoveries: 0,
    };

    for i in 0..=total {
        let fs = SimFs::new(cfg);
        (sc.base)(&fs);
        fs.crash_after(i);
        let committed_ok = run_tx(&fs, sc.ops).is_ok();
        assert!(
            fs.violations().is_empty(),
            "{} crash@{i}: {:?}",
            sc.name,
            fs.violations()
        );
        for (k, o) in outcomes(&fs, apply_cap).into_iter().enumerate() {
            explore(
                &mut ctx,
                &o,
                committed_ok,
                0,
                None,
                &format!("{} {profile:?} crash@{i}/o{k}", sc.name),
            );
        }
    }
    ctx.recoveries
}

#[test]
fn every_crash_point_strict() {
    for sc in scenarios() {
        let n = check_scenario(&sc, Profile::Strict, 64, vec![4]);
        eprintln!("{} (Strict): {n} recoveries checked", sc.name);
    }
}

#[test]
fn every_crash_point_weak() {
    for sc in scenarios() {
        let n = check_scenario(&sc, Profile::Weak, 64, vec![4]);
        eprintln!("{} (Weak): {n} recoveries checked", sc.name);
    }
}

/// Bounded model checking of P3 on the smallest plan: every crash point of the commit,
/// then every crash point of recovery, then every crash point of *that* recovery
/// (depth 2), memoized. Slow (minutes); run with `--ignored`. The unbounded argument is
/// Lemma 3 in DESIGN.md; `random_deep_nesting` stresses long crash chains.
#[test]
#[ignore]
fn nested_recovery_crashes_bounded() {
    let sc = tiny();
    for profile in [Profile::Strict, Profile::Weak] {
        let n = check_scenario(&sc, profile, 4, vec![3, 1]);
        eprintln!("tiny ({profile:?}): {n} recoveries checked");
    }
}

fn tiny() -> Scenario {
    Scenario {
        name: "tiny",
        base: |fs| fs.put_file("a", b"old"),
        ops: |tx| {
            tx.write("a", b"new")?;
            tx.write("b", b"b")
        },
    }
}

/// Stress: long random chains of crashes during recovery (up to 20 in a row, each a random
/// syscall with a random power-loss outcome or process kill), then a clean recovery.
#[test]
fn random_deep_nesting() {
    let mut seed = 0xD1B5_4A32_D192_ED03u64;
    let mut rnd = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n.max(1)
    };
    for sc in scenarios().into_iter().chain([tiny()]) {
        for profile in [Profile::Strict, Profile::Weak] {
            let cfg = SimConfig {
                profile,
                ..SimConfig::default()
            };
            let reference = SimFs::new(cfg);
            (sc.base)(&reference);
            let before = reference.tree();
            run_tx(&reference, sc.ops).unwrap();
            let ctx = Ctx {
                before,
                after: reference.tree(),
                caps: vec![],
                memo: HashMap::new(),
                recoveries: 0,
            };
            for walk in 0..150 {
                let fs = SimFs::new(cfg);
                (sc.base)(&fs);
                fs.crash_after(rnd(reference.calls() + 1));
                let committed_ok = run_tx(&fs, sc.ops).is_ok();
                let mut outs = outcomes(&fs, 16);
                let mut state = outs.swap_remove(rnd(outs.len() as u64) as usize);
                let crashes = 1 + rnd(20);
                for _ in 0..crashes {
                    let probe = state.process_kill();
                    let start = probe.calls();
                    let _ = recover_on(&probe, &Options::new());
                    let n = probe.calls() - start;
                    let s = state.process_kill();
                    s.crash_after(rnd(n + 1));
                    let _ = recover_on(&s, &Options::new());
                    let mut outs = outcomes(&s, 8);
                    state = outs.swap_remove(rnd(outs.len() as u64) as usize);
                }
                let fin = state.process_kill();
                recover_on(&fin, &Options::new())
                    .unwrap_or_else(|e| panic!("{} walk {walk}: {e}", sc.name));
                check_final(
                    &ctx,
                    &fin,
                    committed_ok,
                    &format!("{} {profile:?} walk {walk}", sc.name),
                );
            }
        }
    }
}

/// Lemma 2 in isolation: classification matches the simulator's ground truth for every
/// crash point of the apply phase.
#[test]
fn classification_matches_ground_truth() {
    let sc = &scenarios()[1];
    let reference = SimFs::new(SimConfig::default());
    (sc.base)(&reference);
    run_tx(&reference, sc.ops).unwrap();
    for i in 0..=reference.calls() {
        let fs = SimFs::new(SimConfig::default());
        (sc.base)(&fs);
        fs.crash_after(i);
        let _ = run_tx(&fs, sc.ops);
        for o in outcomes(&fs, 64) {
            let insp = inspect_on(&o).unwrap();
            for tx in &insp.transactions {
                for tok in &tx.tokens {
                    let pos = tok.position.as_ref().expect("classified");
                    // Ground truth: the entry at the classified location has the token's identity.
                    let loc = fstx::RelPath::root();
                    let path = &tok.locations[pos.index];
                    let rel = path.iter().fold(loc, |acc, c| acc.join(c));
                    let m = o
                        .stat(&rel)
                        .unwrap()
                        .expect("present at classified location");
                    assert_eq!(m.id, tok.id);
                    // And it is not also at any other location, unless both-names.
                    for (k, other) in tok.locations.iter().enumerate() {
                        if k == pos.index || (pos.both_names && k == pos.index + 1) {
                            continue;
                        }
                        let rel = other
                            .iter()
                            .fold(fstx::RelPath::root(), |acc, c| acc.join(c));
                        assert!(o.stat(&rel).unwrap().is_none_or(|m| m.id != tok.id));
                    }
                }
            }
        }
    }
}

#[test]
#[ignore]
fn quick_smoke() {
    for profile in [Profile::Strict, Profile::Weak] {
        for sc in scenarios() {
            let t = std::time::Instant::now();
            let n = check_scenario(&sc, profile, 8, vec![]);
            eprintln!(
                "{} {profile:?}: {n} recoveries in {:?}",
                sc.name,
                t.elapsed()
            );
        }
    }
}
