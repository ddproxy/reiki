//! Large bodies must scan in time linear in their size: a request can be megabytes, and the
//! check runs on the request path.

use reiki_finance::{scan, Kind};
use std::time::{Duration, Instant};

/// Far above a linear scan even in a debug build, far below the quadratic scans this guards against.
const LIMIT: Duration = Duration::from_secs(15);

fn timed(body: &str) -> usize {
    let started = Instant::now();
    let hits = scan(body, &Kind::DEFAULT).len();
    assert!(started.elapsed() < LIMIT, "took {:?}", started.elapsed());
    hits
}

#[test]
fn a_large_iban_table_scans_in_linear_time() {
    let mut csv = String::from("iban,name\n");
    for _ in 0..20_000 {
        csv.push_str("GB82WEST12345698765432,Jane\n");
    }
    assert!(timed(&csv) >= 20_000);
}

#[test]
fn many_unlabelled_bics_beside_many_ibans_scan_in_linear_time() {
    assert!(timed(&"GB82WEST12345698765432 DEUTDEFF ".repeat(20_000)) >= 20_000);
}

#[test]
fn unclosed_inserts_do_not_rescan_the_rest_of_the_body() {
    timed(&"insert into t (".repeat(40_000));
}

#[test]
fn many_cards_with_a_cvv_label_scan_in_linear_time() {
    assert!(timed(&"4111 1111 1111 1111 cvv 123 ".repeat(20_000)) >= 20_000);
}
