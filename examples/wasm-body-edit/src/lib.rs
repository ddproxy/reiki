wit_bindgen::generate!({
    world: "plugin",
    path: "../../crates/reiki-wasm/wit",
});

struct BodyEdit;

impl Guest for BodyEdit {
    fn init(_config: Vec<u8>) -> Result<(), String> {
        Ok(())
    }

    fn on_request(req: reiki::plugin::request_meta::Request) -> Result<(types::Verdict, Option<types::RequestEdit>), String> {
        let _ = reiki::plugin::request_body_read::body(&req);
        let edit = types::RequestEdit {
            ops: vec![
                types::EditOp::Set(("/model".to_string(), "redacted-model".to_string())),
            ],
        };
        Ok((types::Verdict::Pass, Some(edit)))
    }
}

export!(BodyEdit);
