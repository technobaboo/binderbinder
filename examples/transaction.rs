use binderbinder::{BinderDevice, BinderRef, Payload};
use std::os::fd::AsRawFd;

extern crate libc;

const ECHO_CODE: u32 = 1;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Parent: opening binder device...");
    let file = std::fs::File::open("/dev/binderfs/testbinder")?;
    let raw_fd = file.as_raw_fd();

    let device = BinderDevice::open(file).await?;
    eprintln!("Parent: binder opened");

    eprintln!("Parent: setting context manager...");
    let obj = device.register_object(0x1234);
    let _cm_ref = device.set_context_manager(obj).await?;
    eprintln!("Parent: context manager set!");

    match unsafe { libc::fork() } {
        -1 => panic!("fork failed"),
        0 => {
            eprintln!("Child: using inherited fd {}...", raw_fd);
            let device = BinderDevice::from_fd_sync(raw_fd);
            eprintln!("Child: device created (sync mode)");

            eprintln!("Child: waiting 500ms for parent to be ready...");
            std::thread::sleep(std::time::Duration::from_millis(500));

            eprintln!("Child: sending transaction to context manager (handle 0)...");
            let payload = Payload::with_data(b"hello from child".to_vec());
            match device.transact_sync_blocking(BinderRef(0), ECHO_CODE, payload) {
                Ok(reply) => {
                    eprintln!("Child received: {:?}", String::from_utf8_lossy(&reply.data));
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Child error: {:?}", e);
                    std::process::exit(1);
                }
            }
        }
        pid => {
            eprintln!("Parent: waiting for transaction...");
            let mut listener = device.register_object(0x1234);
            if let Some(tx) = listener.recv_transaction().await {
                let data = tx.payload.data.clone();
                eprintln!("Parent received: {:?}", String::from_utf8_lossy(&data));
                tx.reply(Payload::with_data(data));
                eprintln!("Parent: reply sent!");
            } else {
                eprintln!("Parent: no transaction received");
            }

            eprintln!("Parent: waiting for child...");
            let _ = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
            eprintln!("Parent: child exited");
        }
    }

    Ok(())
}
