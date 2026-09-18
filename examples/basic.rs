//! `cargo run --example basic -- <dir>`: applies a few changes to <dir> atomically.

fn main() -> fstx::Result<()> {
    let root = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: basic <directory>");
        std::process::exit(2)
    });
    let mut tx = fstx::Transaction::begin(&root)?;
    if !tx.recovered().rolled_back.is_empty() {
        println!(
            "rolled back interrupted transactions: {:?}",
            tx.recovered().rolled_back
        );
    }
    tx.create_dir_all("config")?;
    tx.write("config/app.toml", "version = 2\n")?;
    tx.write("README.txt", "updated atomically by fstx\n")?;
    println!(
        "staged; config/app.toml reads back as {:?}",
        String::from_utf8_lossy(&tx.read("config/app.toml")?)
    );
    tx.commit()?;
    println!("committed");
    Ok(())
}
