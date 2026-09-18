#![no_main]
//! Plants arbitrary bytes as a transaction's journal and runs inspect + recovery on the
//! simulator: no panic, and on refusal the tree is untouched.
use fstx::sim::{SimConfig, SimFs};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let fs = SimFs::new(SimConfig::default());
    fs.put_file("a", b"a");
    fs.put_dir(".fstx");
    fs.put_dir(".fstx/tx-1");
    fs.put_dir(".fstx/tx-1/staged");
    fs.put_dir(".fstx/tx-1/backup");
    fs.put_file(".fstx/tx-1/journal", data);
    let before = fs.tree();
    let _ = fstx::inspect_on(&fs);
    if fstx::recover_on(&fs, &fstx::Options::new()).is_err() {
        assert_eq!(fs.tree(), before);
    }
});
