#![cfg(target_os = "linux")]
//! A hostile thread keeps swapping a directory component with a symlink to the outside
//! while transactions write through it. Operations may fail (the racer is a writer outside
//! the contract), but nothing may ever be created outside the root.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::scratch;
use fstx::Transaction;

#[test]
fn swapped_component_never_escapes_the_root() {
    let d = scratch();
    let outside = scratch();
    let root = d.path().to_path_buf();
    fs::create_dir(root.join("d")).unwrap();
    let stop = Arc::new(AtomicBool::new(false));

    let racer = {
        let (root, outside, stop) = (root.clone(), outside.path().to_path_buf(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = fs::rename(root.join("d"), root.join("d.real"));
                let _ = symlink(&outside, root.join("d"));
                std::thread::sleep(std::time::Duration::from_micros(300));
                let _ = fs::remove_file(root.join("d"));
                let _ = fs::rename(root.join("d.real"), root.join("d"));
                std::thread::sleep(std::time::Duration::from_micros(3_000));
            }
        })
    };

    let mut ok = 0;
    for i in 0..300 {
        let r = Transaction::begin(&root).and_then(|mut tx| {
            tx.write(format!("d/f{i}"), "x")?;
            tx.create_dir_all(format!("d/sub{i}/deep"))?;
            tx.commit()
        });
        if r.is_ok() {
            ok += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    racer.join().unwrap();

    let escaped: Vec<_> = fs::read_dir(outside.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(escaped.is_empty(), "files escaped the root: {escaped:?}");
    eprintln!("{ok}/300 transactions committed under the race");
    // Both paths must have been exercised for the test to mean anything.
    assert!(ok > 0 && ok < 300, "race did not interleave ({ok}/300)");
}
