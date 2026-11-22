//! Reference guest that proves the host exposes no WASI filesystem capability.
//!
//! Every operation targets paths outside any preopen. A host with no preopens
//! must refuse all four; any success is reported as a capability leak.

wit_bindgen::generate!({
    world: "plugin",
    path: "../../crates/reiki-wasm/wit",
});

use std::fs::{File, OpenOptions};

use crate::reiki::plugin::types;

struct FsProbe;

fn open_failed() -> bool {
    File::open("/tmp/reiki-fs-probe-read").is_err()
}

fn truncate_failed() -> bool {
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open("/tmp/reiki-fs-probe-truncate")
        .is_err()
}

fn rename_failed() -> bool {
    std::fs::rename("/tmp/reiki-fs-probe-from", "/tmp/reiki-fs-probe-to").is_err()
}

fn hard_link_failed() -> bool {
    std::fs::hard_link("/tmp/reiki-fs-probe-link", "/tmp/reiki-fs-probe-link-2").is_err()
}

impl Guest for FsProbe {
    fn init(_config: Vec<u8>) -> Result<(), String> {
        Ok(())
    }

    fn on_request(_req: &Request) -> Result<(types::Verdict, Option<types::RequestEdit>), String> {
        let all_refused = open_failed() && truncate_failed() && rename_failed() && hard_link_failed();
        if all_refused {
            Ok((types::Verdict::Pass, None))
        } else {
            Ok((
                types::Verdict::Reject((403, "fs-capability-leak".to_owned())),
                None,
            ))
        }
    }
}

export!(FsProbe);
