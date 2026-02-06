use binderbinder::fs::Binderfs;
use binderbinder::sys::BinderVersion;

fn main() {
    println!("binderbinder - Phase 1 Test");
    println!("============================\n");

    let binder_path = "/tmp/binder_test";

    std::fs::create_dir_all(binder_path).expect("Failed to create test directory");

    println!("[1] Mounting binderfs at {}", binder_path);

    let binderfs = match Binderfs::mount(binder_path) {
        Ok(b) => {
            println!("    OK: binderfs mounted");
            b
        }
        Err(e) => {
            println!("    ERROR: {}", e);
            println!("\nNOTE: binderfs may not be available on this kernel.");
            println!("This test requires a kernel with binderfs support.");
            let _ = std::fs::remove_dir_all(binder_path);
            std::process::exit(1);
        }
    };

    println!("[2] Creating binder device 'binder'");

    let binder_fd = match binderfs.create_device("binder") {
        Ok(fd) => {
            println!("    OK: Binder device created");
            fd
        }
        Err(e) => {
            println!("    ERROR: {}", e);
            std::process::exit(1);
        }
    };

    println!("[3] Calling BINDER_VERSION ioctl");

    let version = BinderVersion {
        protocol_version: 0,
    };
    let result = unsafe { rustix::ioctl::ioctl(&binder_fd, version) };

    match result {
        Ok(v) => {
            println!("    OK: Binder version = {}", v.protocol_version);
            println!("\n============================");
            println!("SUCCESS: All tests passed!");
        }
        Err(e) => {
            println!("    ERROR: ioctl failed - {}", e);
            println!("\n============================");
            println!("FAILED: Could not communicate with binder device");
        }
    }

    println!("\n[Cleanup] Unmounting binderfs");
    drop(binder_fd);
    drop(binderfs);
    let _ = std::fs::remove_dir_all(binder_path);
    println!("    OK: Cleanup complete");
}
