use std::fs::{remove_file, set_permissions};
use std::os::unix::fs::PermissionsExt;

use binderbinder::fs::Binderfs;
use binderbinder::test_pool::{node_path, pool_size, POOL_PREFIX};
use tracing::error;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().collect();
    let size = args
        .iter()
        .position(|a| a == "--pool-size")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(pool_size);

    if args.iter().any(|a| a == "--check") {
        let mount = binderbinder::test_pool::mount_path();
        let missing: Vec<_> = (0..size).map(node_path).filter(|p| !p.exists()).collect();
        if missing.is_empty() {
            println!("OK: all {size} pool nodes present under {}", mount.display());
            std::process::exit(0);
        } else {
            error!("missing pool nodes: {:?}", missing);
            std::process::exit(1);
        }
    }

    println!("binderbinder - Test Pool Setup");
    println!("===============================\n");

    if !is_root() {
        error!("This must be run as root!");
        eprintln!("Run with: sudo cargo run --example setup_test_pool");
        std::process::exit(1);
    }

    println!("[1] Mounting binderfs at /dev/binderfs");
    let binderfs = Binderfs::mount_default().expect("Could not mount binderfs");
    println!("    OK: binderfs mounted/ready");

    println!(
        "\n[2] Provisioning {size} pool device nodes ({POOL_PREFIX}0..{POOL_PREFIX}{})",
        size - 1
    );
    for i in 0..size {
        let name = format!("{POOL_PREFIX}{i}");
        let path = binderfs.path().join(&name);

        if path.exists() {
            remove_file(&path).unwrap_or_else(|e| panic!("Could not remove stale {name}: {e}"));
        }

        let _fd = binderfs
            .create_device(&name)
            .unwrap_or_else(|e| panic!("Could not create {name}: {e}"));

        set_permissions(&path, PermissionsExt::from_mode(0o666))
            .unwrap_or_else(|e| panic!("Could not chmod {name}: {e}"));

        println!("    OK: {name} ready");
    }

    println!("\n===============================");
    println!(
        "SUCCESS: {size} pool nodes ready under {}",
        binderfs.path().display()
    );
    println!("\nRun the stress suite as a regular user with: cargo nextest run");
}

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}
