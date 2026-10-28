//! `reiki scan` runs the detection plugins over files or stdin and never prints the matched text.

use std::io::Write;
use std::process::{Command, Stdio};

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn reiki(args: &[&str], stdin: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_reiki"))
        .arg("scan")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Run {
        code: out.status.code().unwrap(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn findings_print_positions_and_kinds_but_never_the_value() {
    let run = reiki(&[], "first line\nthe ssn is 123-45-6789 ok\n");
    assert_eq!(run.code, 1);
    assert_eq!(run.stdout, "-:2:12 phi ssn\n");
    assert!(
        run.stderr.contains("1 finding(s): phi ssn x1"),
        "{}",
        run.stderr
    );
    assert!(!run.stdout.contains("123-45") && !run.stderr.contains("123-45"));
}

#[test]
fn clean_input_exits_zero() {
    let run = reiki(&[], "nothing sensitive here\n");
    assert_eq!(run.code, 0);
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("no findings"));
}

#[test]
fn plugin_and_kind_options_narrow_the_scan() {
    let text = "cusip 037833100 and, well past any keyword window, ssn 123-45-6789\n";
    let col = |needle: &str| text.find(needle).unwrap() + 1;
    assert_eq!(
        reiki(&["--plugin", "finance"], text).stdout,
        format!("-:1:{} finance cusip\n", col("037833100"))
    );
    assert_eq!(
        reiki(&["--kinds", "ssn"], text).stdout,
        format!("-:1:{} phi ssn\n", col("123-45-6789"))
    );
    let all = reiki(&["--all", "--plugin", "phi"], "mail jane@example.com\n");
    assert_eq!(all.stdout, "-:1:6 phi email\n");
    assert_eq!(
        reiki(&["--plugin", "phi"], "mail jane@example.com\n").code,
        0
    );
}

#[test]
fn json_output_has_one_object_per_finding() {
    let run = reiki(&["--json"], "ssn 123-45-6789\n");
    assert_eq!(
        run.stdout,
        "{\"file\":\"-\",\"line\":1,\"col\":5,\"plugin\":\"phi\",\"kind\":\"ssn\"}\n"
    );
}

#[test]
fn bad_options_and_missing_files_exit_two() {
    assert_eq!(reiki(&["--plugin", "nope"], "x").code, 2);
    assert_eq!(reiki(&["--kinds", "bogus"], "x").code, 2);
    let run = reiki(&["/definitely/not/a/file"], "");
    assert_eq!(run.code, 2);
    assert!(run.stderr.contains("cannot read"));
}

#[test]
fn files_are_scanned_and_named_in_the_output() {
    let dir = std::env::temp_dir().join(format!("reiki-scan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("data.csv");
    std::fs::write(&file, "id,ssn\n1,123456789\n").unwrap();
    let path = file.to_str().unwrap();
    let run = reiki(&[path], "");
    assert_eq!(run.code, 1);
    assert_eq!(run.stdout, format!("{path}:2:3 phi ssn\n"));
    let _ = std::fs::remove_dir_all(&dir);
}
