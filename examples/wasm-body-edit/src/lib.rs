wit_bindgen::generate!({
    world: "plugin",
    path: "../../crates/reiki-wasm/wit",
});

use std::cell::RefCell;

use crate::reiki::plugin::types;

thread_local! {
    static MODE: RefCell<String> = const { RefCell::new(String::new()) };
}

struct BodyEdit;

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

impl Guest for BodyEdit {
    fn init(config: Vec<u8>) -> Result<(), String> {
        let mode = config_mode(&config);
        MODE.with(|m| *m.borrow_mut() = mode.clone());
        if mode == "init-trap" {
            return Err("init refused by fixture".to_owned());
        }
        Ok(())
    }

    fn on_request(req: &Request) -> Result<(types::Verdict, Option<types::RequestEdit>), String> {
        let mode = MODE.with(|m| m.borrow().clone());
        if mode == "trap" {
            unreachable!("fixture trap");
        }
        if mode == "fuel" {
            spin_forever();
        }
        if mode == "memory" {
            exhaust_memory();
        }

        let body = reiki::plugin::request_body_read::body(req).unwrap_or_default();
        let enabled = reiki::plugin::request_body_edit::enabled();
        reiki::plugin::log::log(2, "edited", &[body.len() as u32]);

        let detail = if mode == "vocab-bad" {
            "client.tool-name"
        } else {
            "rule.model-redacted"
        };
        let allowed = reiki::plugin::finding_detail_vocab::allowed(detail);
        reiki::plugin::finding::report(&types::Finding {
            kind: "wasm.edit".to_owned(),
            detail: Some(detail.to_owned()),
            count: 1,
            blocked: !allowed || mode == "vocab-bad",
        });

        if mode == "reject" {
            return Ok((
                types::Verdict::Reject((422, "bad-model".to_owned())),
                None,
            ));
        }

        let (pointer, literal) = match mode.as_str() {
            "bad-literal" => ("/model", "missing-literal"),
            "bad-pointer" => ("model", "redacted-model"),
            _ => ("/model", "redacted-model"),
        };
        let edit = types::RequestEdit {
            ops: if enabled {
                vec![types::EditOp::Set((
                    pointer.to_owned(),
                    literal.to_owned(),
                ))]
            } else {
                Vec::new()
            },
        };
        Ok((types::Verdict::Pass, Some(edit)))
    }
}

export!(BodyEdit);
