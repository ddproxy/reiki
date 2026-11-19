//! The daemon probes the state directory before it binds a listener.

#[cfg(unix)]
#[test]
fn a_read_only_state_directory_stops_startup_and_names_the_cause() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let dir = std::env::temp_dir().join(format!("reiki-state-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("reiki.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Root can write through mode bits, so a read-only directory cannot be reproduced.
    let root_bypasses = std::fs::File::create(dir.join(".root-check")).is_ok();
    let result = if root_bypasses {
        None
    } else {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
            .arg("--config")
            .arg(&config)
            .env_remove("RUST_LOG")
            .env_remove("REIKI_LISTEN")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                break Some(child.wait_with_output().unwrap());
            }
            assert!(Instant::now() < deadline, "reiki did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let Some(out) = result else {
        return;
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains(&dir.display().to_string()), "{stderr}");
    assert!(stderr.contains("not writable"), "{stderr}");
    assert!(stderr.contains("Permission denied"), "{stderr}");
}
