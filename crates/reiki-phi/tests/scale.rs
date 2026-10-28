//! Large bodies must scan in time linear in their size: a request can be megabytes, and the
//! check runs on the request path.

use reiki_phi::{scan, Kind};
use std::time::{Duration, Instant};

/// Far above a linear scan even in a debug build, far below the quadratic scans this guards against.
const LIMIT: Duration = Duration::from_secs(15);

#[test]
fn a_large_table_scans_in_linear_time() {
    let mut csv = String::from("ssn,name\n");
    for i in 0..40_000 {
        csv.push_str(&format!(
            "{:03}-45-{:04},Jane\n",
            100 + i % 800,
            1 + i % 9000
        ));
    }
    let started = Instant::now();
    let hits = scan(&csv, &Kind::DEFAULT);
    assert!(hits.len() >= 39_000);
    assert!(started.elapsed() < LIMIT, "took {:?}", started.elapsed());
}

#[test]
fn unclosed_inserts_do_not_rescan_the_rest_of_the_body() {
    let sql = "insert into t (".repeat(40_000);
    let started = Instant::now();
    scan(&sql, &Kind::DEFAULT);
    assert!(started.elapsed() < LIMIT, "took {:?}", started.elapsed());
}
