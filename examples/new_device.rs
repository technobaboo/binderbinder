use std::fs::{remove_file, set_permissions};
use std::os::unix::fs::PermissionsExt;

use binderbinder::fs::Binderfs;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--check") {
        let path = std::path::PathBuf::from("/dev/binderfs/testbinder");
        if path.exists() {
            println!("OK: testbinder exists at {}", path.display());
            std::process::exit(0);
        } else {
            eprintln!("ERROR: testbinder not found");
            std::process::exit(1);
        }
    }

    println!("binderbinder - New Device Example");
    println!("==================================\n");

    if !is_root() {
        eprintln!("ERROR: This must be run as root!");
        eprintln!("Run with: sudo ./target/debug/examples/new_device");
        std::process::exit(1);
    }

    println!("[1] Mounting binderfs at /dev/binderfs");

    let binderfs = Binderfs::mount_default().expect("Could not mount binderfs");

    println!("    OK: binderfs mounted/ready");

    let testbinder_path = binderfs.path().join("testbinder");

    println!("\n[2] Removing existing testbinder device if present");

    if testbinder_path.exists() {
        remove_file(&testbinder_path).expect("Could not remove existing testbinder");
        println!("    OK: Removed existing testbinder");
    } else {
        println!("    OK: No existing testbinder found");
    }

    println!("\n[3] Creating new testbinder device");

    let _fd = binderfs
        .create_device("testbinder")
        .expect("Could not create testbinder");
    println!("    OK: Created testbinder device");

    println!("\n[4] Setting permissions to 666 (all users read/write)");

    set_permissions(&testbinder_path, PermissionsExt::from_mode(0o666))
        .expect("Could not set permissions");
    println!("    OK: Permissions set to 666");

    println!("\n==================================");
    println!("SUCCESS: testbinder device is ready!");
    println!("\nPath: {}", testbinder_path.display());
    println!("\nRun as regular user: /dev/binderfs/testbinder");
}

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}
