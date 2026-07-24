//! Raw ioctl-level check, hermetic via the shared node pool (see
//! `tests/support/mod.rs`) instead of a hardcoded `/dev/binderfs/testbinder`.

mod support;

use binderbinder::sys::BinderVersion;
use support::PoolNode;

#[test]
fn version_ioctl_reports_protocol_8() {
    let node = PoolNode::acquire();
    let file = std::fs::File::open(&node.path).expect("open pool node");
    let version = BinderVersion {
        protocol_version: 0,
    };
    let result = unsafe { rustix::ioctl::ioctl(&file, version) };
    assert!(result.is_ok());
    assert_eq!(result.unwrap().protocol_version, 8);
}
