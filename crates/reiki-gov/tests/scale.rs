//! Large bodies must scan in time linear in their size: a request can be megabytes, and the
//! check runs on the request path.

use reiki_gov::{scan, Kind};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(15);

#[test]
fn banners_and_partial_markers_scan_in_linear_time() {
    let mut kinds = Kind::DEFAULT.to_vec();
    kinds.extend([
        Kind::Ein,
        Kind::Itin,
        Kind::Cage,
        Kind::Uei,
        Kind::Edipi,
        Kind::Eccn,
    ]);
    let banners =
        "TOP SECRET//SI//NOFORN CUI//SP-CTI ORCON ECCN 5A002 EIN 12-3456789 ".repeat(20_000);
    let partial = "CUI//".repeat(80_000) + &"SECRET//".repeat(80_000) + &"REL TO ".repeat(80_000);
    for body in [banners, partial] {
        let started = Instant::now();
        scan(&body, &kinds);
        assert!(started.elapsed() < LIMIT, "took {:?}", started.elapsed());
    }
}
