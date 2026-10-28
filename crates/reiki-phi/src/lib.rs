//! Reiki plugin that stops protected health information (PHI) and nonpublic personal
//! information (NPPI) from leaving in an outbound request.
//!
//! In `block` mode a request that holds a recognised identifier is rejected before any key is
//! used. In `report` mode it goes through and the finding is logged. Either way the log and the
//! rejection name the kinds and counts found, never the values.

mod columnar;
mod detect;

use reiki_detect::guard::{self, Spec};
use reiki_plugin::Plugin;

pub use detect::{scan, Hit, Kind};

/// The `[plugins.phi]` table this plugin reads.
pub const NAME: &str = "phi";

impl guard::Kind for Kind {
    fn label(self) -> &'static str {
        Kind::label(self)
    }

    fn parse(label: &str) -> Option<Self> {
        Kind::parse(label)
    }

    fn defaults() -> &'static [Self] {
        &Kind::DEFAULT
    }
}

/// Builds the plugin from its config table.
pub fn build(table: &toml::Table) -> Result<Box<dyn Plugin>, String> {
    guard::build(
        Spec {
            name: NAME,
            finding: "possible protected data",
            scan,
        },
        table,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::StatusCode;
    use http::{HeaderMap, HeaderValue, Method};
    use reiki_plugin::{PluginError, RequestCtx};

    fn plugin(text: &str) -> Result<Box<dyn Plugin>, String> {
        build(&text.parse().unwrap())
    }

    fn send(
        p: &dyn Plugin,
        upstream: &str,
        headers: &mut HeaderMap,
        body: &str,
    ) -> Result<(), PluginError> {
        let mut payload = Bytes::from(body.to_owned());
        let mut state = ();
        p.on_request(&mut RequestCtx::new(
            "ct",
            upstream,
            &Method::POST,
            "/x",
            headers,
            &mut payload,
            &mut state,
        ))
    }

    #[test]
    fn block_mode_rejects_and_names_kinds_without_values() {
        let p = plugin("mode = \"block\"").unwrap();
        let body = "{\"q\":\"patient ssn 123-45-6789 card 4111 1111 1111 1111\"}";
        let err = send(p.as_ref(), "zen", &mut HeaderMap::new(), body).unwrap_err();
        let PluginError::Reject(status, message) = err else {
            panic!("{err}")
        };
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(message, "phi: request blocked, it contains ssn x1, card x1");
        assert!(!message.contains("123-45") && !message.contains("4111"));
    }

    #[test]
    fn report_mode_lets_the_request_through() {
        let p = plugin("mode = \"report\"").unwrap();
        assert!(send(p.as_ref(), "zen", &mut HeaderMap::new(), "ssn 123-45-6789").is_ok());
    }

    #[test]
    fn clean_bodies_and_other_upstreams_pass() {
        let p = plugin("mode = \"block\"\nupstreams = [\"zen\"]").unwrap();
        assert!(send(
            p.as_ref(),
            "zen",
            &mut HeaderMap::new(),
            "{\"q\":\"hello\"}"
        )
        .is_ok());
        assert!(send(
            p.as_ref(),
            "forgejo",
            &mut HeaderMap::new(),
            "ssn 123-45-6789"
        )
        .is_ok());
        assert!(send(p.as_ref(), "zen", &mut HeaderMap::new(), "ssn 123-45-6789").is_err());
    }

    #[test]
    fn an_encoded_body_fails_closed_in_block_mode_only() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        let block = plugin("mode = \"block\"").unwrap();
        assert!(matches!(
            send(block.as_ref(), "zen", &mut headers, "x"),
            Err(PluginError::Reject(StatusCode::UNSUPPORTED_MEDIA_TYPE, _))
        ));
        let report = plugin("mode = \"report\"").unwrap();
        assert!(send(report.as_ref(), "zen", &mut headers, "x").is_ok());
    }

    #[test]
    fn kinds_narrow_and_extend_the_default_set() {
        let ssn_only = plugin("mode = \"block\"\nkinds = [\"ssn\"]").unwrap();
        assert!(send(
            ssn_only.as_ref(),
            "zen",
            &mut HeaderMap::new(),
            "card 4111 1111 1111 1111"
        )
        .is_ok());
        let with_email = plugin("mode = \"block\"\nkinds = [\"email\"]").unwrap();
        assert!(send(
            with_email.as_ref(),
            "zen",
            &mut HeaderMap::new(),
            "a@example.com"
        )
        .is_err());
    }

    #[test]
    fn the_config_is_strict() {
        assert!(plugin("").is_err(), "mode is required");
        assert!(plugin("mode = \"maybe\"").is_err());
        assert!(plugin("mode = \"block\"\nkinds = [\"bogus\"]").is_err());
        assert!(plugin("mode = \"block\"\nkinds = []").is_err());
        assert!(plugin("mode = \"block\"\nbogus = 1").is_err());
        assert!(plugin("mode = \"block\"\nupstreams = [1]").is_err());
    }

    fn reported(p: &dyn Plugin, body: &str) -> Vec<reiki_plugin::Reported> {
        let mut headers = HeaderMap::new();
        let mut payload = Bytes::from(body.to_owned());
        let mut state = ();
        let mut ctx = RequestCtx::new(
            "ct",
            "zen",
            &Method::POST,
            "/x",
            &mut headers,
            &mut payload,
            &mut state,
        );
        let _ = p.on_request(&mut ctx);
        ctx.reported
    }

    #[test]
    fn findings_are_reported_with_their_kind_count_and_whether_the_request_was_blocked() {
        let body = "ssn 123-45-6789 and 234-56-7890";
        let blocked = reported(plugin("mode = \"block\"").unwrap().as_ref(), body);
        assert_eq!(blocked.len(), 1, "{blocked:?}");
        assert_eq!(
            (blocked[0].kind, blocked[0].count, blocked[0].blocked),
            ("ssn", 2, true)
        );
        let seen = reported(plugin("mode = \"report\"").unwrap().as_ref(), body);
        assert_eq!(
            (seen[0].kind, seen[0].count, seen[0].blocked),
            ("ssn", 2, false)
        );
        assert!(reported(plugin("mode = \"block\"").unwrap().as_ref(), "nothing here").is_empty());
    }
}
