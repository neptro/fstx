#![no_main]
//! Arbitrary user paths: no panic, and nothing ever lands in `.fstx` through the API.
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use fstx::sim::{SimConfig, SimFs};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let fs = SimFs::new(SimConfig::default());
    let Ok(mut tx) = fstx::Transaction::begin_on(Box::new(fs.clone()), &fstx::Options::new()) else { return };
    let p = OsStr::from_bytes(data);
    let _ = tx.write(p, b"x");
    let _ = tx.create_dir_all(p);
    let _ = tx.commit();
    assert!(fs.private_tree().is_empty());
});
