#![no_main]
//! Arbitrary progress-log bytes must never cause a panic in inspect or recovery.
use fstx::sim::{SimConfig, SimFs};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let fs = SimFs::new(SimConfig::default());
    fs.put_dir(".fstx");
    fs.put_dir(".fstx/tx-1");
    fs.put_file(".fstx/tx-1/progress", data);
    let _ = fstx::inspect_on(&fs);
    let _ = fstx::recover_on(&fs, &fstx::Options::new());
});
