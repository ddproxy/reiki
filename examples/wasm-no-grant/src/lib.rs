wit_bindgen::generate!({
    world: "plugin",
    path: "../../crates/reiki-wasm/wit",
});

use std::cell::RefCell;

use crate::reiki::plugin::types;

thread_local! {
    static MODE: RefCell<String> = const { RefCell::new(String::new()) };
}

struct NoGrant;

/// The operator's `[plugins.<name>.config]` table is serialized as TOML. The reference
/// guests use one `mode` string to select the behaviour a conformance case needs.
fn config_mode(config: &[u8]) -> String {
    let text = String::from_utf8_lossy(config);
    for line in text.lines().rev() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "mode" {
            return value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_owned();
        }
    }
    String::new()
}

fn spin_forever() -> ! {
    let mut n = 0u64;
    loop {
        n = n.wrapping_add(1);
        std::hint::black_box(n);
    }
}

fn exhaust_memory() {
    let mut pages: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut page = vec![0u8; 64 * 1024];
        page[0] = (pages.len() & 0xff) as u8;
        pages.push(page);
        std::hint::black_box(&pages);
    }
}

impl Guest for NoGrant {
    fn init(config: Vec<u8>) -> Result<(), String> {
        let mode = config_mode(&config);
        MODE.with(|m| *m.borrow_mut() = mode.clone());
        match mode.as_str() {
            "init-trap" => Err("init refused by fixture".to_owned()),
            "init-fuel" => spin_forever(),
            _ => Ok(()),
        }
    }

    fn on_request(req: &Request) -> Result<(types::Verdict, Option<types::RequestEdit>), String> {
        let mode = MODE.with(|m| m.borrow().clone());
        let _ = req.stream_name();

        match mode.as_str() {
            "trap" => unreachable!("fixture trap"),
            "fuel" => spin_forever(),
            "memory" => exhaust_memory(),
            "deadline" => {
                // request-state is default-allowed and checks the host deadline on every call.
                for _ in 0..20_000 {
                    let _ = std::hint::black_box(reiki::plugin::request_state::get("deadline"));
                }
            }
            "findings" => {
                reiki::plugin::finding::report(&types::Finding {
                    kind: "wasm.rule".to_owned(),
                    detail: None,
                    count: 2,
                    blocked: false,
                });
                reiki::plugin::log::log(2, "hit", &[7]);
                let _ = reiki::plugin::reject::has("blocked");
            }
            "reject" => {
                let _ = reiki::plugin::reject::has("blocked");
                return Ok((
                    types::Verdict::Reject((403, "blocked".to_owned())),
                    None,
                ));
            }
            "skip-control" => {
                return Ok((
                    types::Verdict::Skip("line\nbreak\u{7}reason".to_owned()),
                    None,
                ));
            }
            _ => {}
        }

        Ok((types::Verdict::Pass, None))
    }
}

export!(NoGrant);
