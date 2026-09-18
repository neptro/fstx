//! `fstx`: apply file changes atomically from the command line.

mod changes;

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde_json::json;

use changes::{ChangeSet, Done, FailKind, Failure};

#[derive(Parser)]
#[command(
    name = "fstx",
    version,
    about = "Apply file changes atomically: all of them, or none, even across crashes."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Common {
    /// Directory the changes apply to.
    #[arg(short = 'C', long, default_value = ".")]
    root: PathBuf,
    /// Print machine-readable JSON on stdout.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Apply a JSON change set (a file, or `-` for stdin) as one transaction.
    Apply {
        /// The change set; `-` reads stdin.
        #[arg(default_value = "-")]
        changes: String,
        /// Validate and stage everything, then discard: nothing in the tree changes.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Copy every file under SRC into the root as one transaction (e.g. dotfiles).
    Sync {
        /// Source directory.
        src: PathBuf,
        /// Names to skip at any depth (repeatable).
        #[arg(long, default_values_t = [".git".to_string()])]
        exclude: Vec<String>,
        /// Stage everything, then discard: nothing in the tree changes.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Finish or roll back transactions interrupted by a crash.
    Recover {
        #[command(flatten)]
        common: Common,
    },
    /// Show interrupted transactions and what recovery would do (read-only).
    Inspect {
        #[command(flatten)]
        common: Common,
    },
}

fn fail(json_out: bool, f: &Failure) -> ExitCode {
    if json_out {
        println!(
            "{}",
            json!({ "ok": false, "kind": f.kind, "op": f.op, "error": f.message })
        );
    }
    match f.op {
        Some(i) => eprintln!("fstx: op {i}: {}", f.message),
        None => eprintln!("fstx: {}", f.message),
    }
    eprintln!("fstx: nothing was changed");
    ExitCode::from(f.kind.exit_code() as u8)
}

fn report(common: &Common, dry_run: bool, done: &[Done], skipped: &[String]) {
    if common.json {
        println!(
            "{}",
            json!({ "ok": true, "dry_run": dry_run, "root": common.root, "changes": done, "skipped": skipped })
        );
        return;
    }
    let verb = if dry_run { "would apply" } else { "applied" };
    println!(
        "{verb} {} change(s) to {}",
        done.len(),
        common.root.display()
    );
    for d in done {
        match &d.to {
            Some(to) => println!("  {:<10} {} -> {to}", d.op, d.path),
            None => println!("  {:<10} {}", d.op, d.path),
        }
    }
    for s in skipped {
        println!("  skipped    {s} (not a regular file or directory)");
    }
    if dry_run {
        println!("dry run: nothing was changed");
    }
}

/// Stages with `stage`, then commits (or discards on a dry run).
fn run_tx(
    common: &Common,
    dry_run: bool,
    stage: impl FnOnce(&mut fstx::Transaction) -> Result<(Vec<Done>, Vec<String>), Failure>,
) -> Result<(Vec<Done>, Vec<String>), Failure> {
    let mut tx = fstx::Transaction::begin(&common.root).map_err(|e| Failure::from_fstx(None, e))?;
    let out = stage(&mut tx)?;
    if !dry_run {
        tx.commit().map_err(|e| Failure::from_fstx(None, e))?;
    }
    Ok(out)
}

fn read_changes(arg: &str) -> Result<(ChangeSet, PathBuf), Failure> {
    let (text, base) = if arg == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| Failure::new(FailKind::InvalidInput, None, format!("stdin: {e}")))?;
        (s, PathBuf::from("."))
    } else {
        let path = PathBuf::from(arg);
        let s = std::fs::read_to_string(&path)
            .map_err(|e| Failure::new(FailKind::InvalidInput, None, format!("{arg}: {e}")))?;
        let base = path.parent().map(PathBuf::from).unwrap_or_default();
        (s, base)
    };
    let cs = serde_json::from_str(&text).map_err(|e| {
        Failure::new(
            FailKind::InvalidInput,
            None,
            format!("invalid change set: {e}"),
        )
    })?;
    Ok((cs, base))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Apply {
            changes,
            dry_run,
            common,
        } => {
            let result = read_changes(&changes).and_then(|(cs, base)| {
                run_tx(&common, dry_run, |tx| {
                    Ok((changes::stage(tx, &cs, &base)?, Vec::new()))
                })
            });
            match result {
                Ok((done, skipped)) => {
                    report(&common, dry_run, &done, &skipped);
                    ExitCode::SUCCESS
                }
                Err(f) => fail(common.json, &f),
            }
        }
        Command::Sync {
            src,
            exclude,
            dry_run,
            common,
        } => {
            match run_tx(&common, dry_run, |tx| {
                changes::stage_sync(tx, &src, &exclude)
            }) {
                Ok((done, skipped)) => {
                    report(&common, dry_run, &done, &skipped);
                    ExitCode::SUCCESS
                }
                Err(f) => fail(common.json, &f),
            }
        }
        Command::Recover { common } => match fstx::recover(&common.root) {
            Ok(r) => {
                if common.json {
                    println!(
                        "{}",
                        json!({
                            "ok": true,
                            "rolled_back": r.rolled_back,
                            "completed": r.completed,
                            "discarded": r.discarded,
                            "garbage_removed": r.garbage_removed,
                        })
                    );
                } else if r.rolled_back.is_empty()
                    && r.completed.is_empty()
                    && r.discarded.is_empty()
                {
                    println!("nothing to recover in {}", common.root.display());
                } else {
                    println!(
                        "recovered {}: {} rolled back, {} completed, {} discarded",
                        common.root.display(),
                        r.rolled_back.len(),
                        r.completed.len(),
                        r.discarded.len()
                    );
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(common.json, &Failure::from_fstx(None, e)),
        },
        Command::Inspect { common } => match fstx::inspect(&common.root) {
            Ok(insp) => {
                let txs: Vec<_> = insp
                    .transactions
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "action": format!("{:?}", t.action),
                            "reason": t.reason,
                            "entries": t.tokens.len(),
                        })
                    })
                    .collect();
                if common.json {
                    println!(
                        "{}",
                        json!({ "ok": true, "transactions": txs, "garbage": insp.garbage })
                    );
                } else if insp.transactions.is_empty() {
                    println!("no interrupted transactions in {}", common.root.display());
                } else {
                    for t in &insp.transactions {
                        println!(
                            "{}: {:?}{}",
                            t.name,
                            t.action,
                            t.reason
                                .as_deref()
                                .map(|r| format!(" ({r})"))
                                .unwrap_or_default()
                        );
                    }
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(common.json, &Failure::from_fstx(None, e)),
        },
    }
}
