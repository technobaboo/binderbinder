use crate::sys::BinderfsDevice;
use std::{ffi::OsStr, os::unix::{ffi::OsStrExt, io::OwnedFd}, path::Path, process::Command};

pub const BINDERFS_DEV_MAJOR: u64 = 0;
pub const BINDERFS_DEV_MINOR: u32 = 0;
pub const DEFAULT_BINDERFS_PATH: &str = "/dev/binderfs";

pub struct Binderfs {
    path: std::path::PathBuf,
    control_fd: OwnedFd,
}
fn is_mounted(path: &Path) -> bool {
    let output = Command::new("findmnt")
        .arg("-M")
        .arg(path)
        .arg("--pairs")
        .output()
        .expect("Failed to check for binderfs, might be missing findmnt");
    let str = String::from_utf8_lossy(&output.stdout);
    str.contains("SOURCE=\"binder\"") && str.contains("FSTYPE=\"binder\"")
}

impl Binderfs {
    pub fn mount_default() -> std::io::Result<Self> {
        Self::mount(DEFAULT_BINDERFS_PATH)
    }

    pub fn mount(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            std::fs::create_dir_all(path)?;
        }

        if !path.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "binderfs path is not a directory",
            ));
        }

        // ideally we would check if the target dir is alread mounted as a binderfs but idk how to
        // do that rn
        if !is_mounted(path) {
            Command::new("mount")
                .arg("-t")
                .arg("binder")
                .arg("binder")
                .arg(path)
                .status()?;
        }

        let control_fd = OwnedFd::from(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.join("binder-control"))?,
        );

        Ok(Binderfs {
            path: path.to_path_buf(),
            control_fd,
        })
    }

    pub fn create_device(&self, name: impl AsRef<OsStr>) -> std::io::Result<OwnedFd> {
        let name = name.as_ref();
        if name.len() > 255 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Device name too long",
            ));
        }

        let mut device = BinderfsDevice::default();

        let name_bytes = name.as_bytes();
        device.name[..name_bytes.len()].copy_from_slice(name_bytes);

        unsafe {
            rustix::ioctl::ioctl(&self.control_fd, device)?;
        }

        let device_path = self.path.join(name);
        let fd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device_path)?;

        Ok(OwnedFd::from(fd))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn control_fd(&self) -> &OwnedFd {
        &self.control_fd
    }
}
