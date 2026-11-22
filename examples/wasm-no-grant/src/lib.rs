wit_bindgen::generate!({
    world: "plugin",
    path: "../../crates/reiki-wasm/wit",
});

struct NoGrant;

impl Guest for NoGrant {
    fn init(_config: Vec<u8>) -> Result<(), String> {
        Ok(())
    }

    fn on_request(_req: reiki::plugin::request_meta::Request) -> Result<(types::Verdict, Option<types::RequestEdit>), String> {
        // No gated capabilities requested; default-allowed imports only.
        let _name = reiki::plugin::request_meta::stream_name(&_req);
        Ok((types::Verdict::Pass, None))
    }
}

export!(NoGrant);
