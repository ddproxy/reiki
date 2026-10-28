//! Reiki plugin that keeps sensitive values out of the requests sent upstream.
//!
//! Credential-shaped strings and a configured list of terms are swapped for stable
//! placeholders before the request leaves, and the originals are put back into the
//! response before the client sees it. Plugins listed after this one, such as the
//! request log, only ever see the masked traffic.

mod config;
mod mask;
mod restore;

use std::any::Any;
use std::collections::HashMap;

use bytes::Bytes;
use http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use http::StatusCode;
use reiki_plugin::{ChunkCtx, Plugin, PluginError, RequestCtx, ResponseHeadCtx};

pub use config::Config;
use mask::Masker;
use restore::Restorer;

/// The `[plugins.placeholders]` table this plugin reads.
pub const NAME: &str = "placeholders";

/// Builds the plugin from its config table.
pub fn build(table: &toml::Table) -> Result<Box<dyn Plugin>, String> {
    let mut config = Config::parse(table).map_err(|e| format!("{NAME}: {e}"))?;
    if let (false, Some(path)) = (config.dry_run, config.terms_file.clone()) {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("{NAME}: cannot read terms_file {}: {e}", path.display()))?;
        let listed: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect();
        let mut terms = std::mem::take(&mut config.terms);
        terms.extend(listed);
        config.terms = config::validate_terms(terms).map_err(|e| format!("{NAME}: {e}"))?;
        if !config.builtin && config.terms.is_empty() {
            return Err(format!(
                "{NAME}: terms_file lists no terms and `builtin` is off"
            ));
        }
    }
    Ok(Box::new(Placeholders::new(config)))
}

pub struct Placeholders {
    config: Config,
    masker: Masker,
}

/// What one request masked, so its response can be put back.
struct State {
    reverse: HashMap<String, String>,
    restorer: Restorer,
    restoring: bool,
}

impl Placeholders {
    pub fn new(config: Config) -> Self {
        let masker = Masker::new(&config.terms, config.builtin);
        Self { config, masker }
    }
}

impl Plugin for Placeholders {
    fn name(&self) -> &str {
        NAME
    }

    fn new_state(&self) -> Box<dyn Any + Send> {
        Box::new(State {
            reverse: HashMap::new(),
            restorer: Restorer::new(
                self.config.max_response_bytes,
                self.config.restore_tool_args,
            ),
            restoring: false,
        })
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let Some(state) = ctx.state.downcast_mut::<State>() else {
            return Ok(());
        };
        // An encoded or non-text body cannot be masked, so refuse it rather than send it raw.
        let encoded = ctx.headers.contains_key(CONTENT_ENCODING);
        let Some(text) = std::str::from_utf8(ctx.body).ok().filter(|_| !encoded) else {
            return Err(PluginError::Reject(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "placeholders: the request body is encoded or not UTF-8 text, so it cannot be masked"
                    .to_owned(),
            ));
        };
        if let Some(masked) = self.masker.mask(text, &mut state.reverse) {
            *ctx.body = Bytes::from(masked);
            tracing::debug!(
                restorable = state.reverse.len(),
                "placeholders: masked the request"
            );
        }
        Ok(())
    }

    fn on_response_head(&self, ctx: &mut ResponseHeadCtx<'_>) {
        let Some(state) = ctx.state.downcast_mut::<State>() else {
            return;
        };
        if !self.config.rehydrate || state.reverse.is_empty() {
            return;
        }
        let content_type = ctx
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let encoded = ctx.headers.contains_key(CONTENT_ENCODING);
        state.restorer.start(content_type.as_deref(), encoded);
        state.restoring = true;
    }

    fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
        let Some(state) = ctx.state.downcast_mut::<State>() else {
            return;
        };
        if state.restoring {
            state
                .restorer
                .feed(&mut *ctx.chunk, ctx.is_last, &state.reverse);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, Method, StatusCode};
    use std::path::PathBuf;

    fn plugin(text: &str) -> Box<dyn Plugin> {
        build(&text.parse().unwrap()).unwrap()
    }

    /// Fixtures are assembled at runtime so no token-shaped literal sits in the source.
    fn fake(prefix: &str, tail: &str) -> String {
        [prefix, tail].concat()
    }

    /// The first placeholder in a masked body.
    fn first_placeholder(masked: &str) -> String {
        let start = masked.find("[[REIKI_").unwrap();
        let end = masked[start..].find("]]").unwrap() + start + 2;
        masked[start..end].to_owned()
    }

    /// The masked body the upstream would get.
    fn send(plugin: &dyn Plugin, state: &mut Box<dyn Any + Send>, body: &str) -> String {
        let mut headers = HeaderMap::new();
        let mut payload = Bytes::from(body.to_owned());
        plugin
            .on_request(&mut RequestCtx::new(
                "ct",
                "zen",
                &Method::POST,
                "/zen/chat/completions",
                &mut headers,
                &mut payload,
                state.as_mut(),
            ))
            .unwrap();
        String::from_utf8(payload.to_vec()).unwrap()
    }

    /// What the client would get for an upstream body delivered in `parts`.
    fn receive(
        plugin: &dyn Plugin,
        state: &mut Box<dyn Any + Send>,
        content_type: &str,
        parts: &[&str],
    ) -> String {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_str(content_type).unwrap());
        let mut status = StatusCode::OK;
        plugin.on_response_head(&mut ResponseHeadCtx::new(
            "k",
            &mut status,
            &mut headers,
            state.as_mut(),
        ));
        let mut out = String::new();
        for part in parts {
            let mut chunk = Bytes::from((*part).to_owned());
            plugin.on_response_chunk(&mut ChunkCtx::new(&mut chunk, false, state.as_mut()));
            out.push_str(&String::from_utf8_lossy(&chunk));
        }
        let mut last = Bytes::new();
        plugin.on_response_chunk(&mut ChunkCtx::new(&mut last, true, state.as_mut()));
        out.push_str(&String::from_utf8_lossy(&last));
        out
    }

    #[test]
    fn a_term_is_masked_going_out_and_restored_coming_back() {
        let p = plugin("terms = [\"Project Falcon\"]\nbuiltin = false");
        let mut state = p.new_state();
        let sent = send(
            p.as_ref(),
            &mut state,
            "{\"q\":\"status of project falcon?\"}",
        );
        assert!(!sent.to_lowercase().contains("falcon"), "{sent}");
        let placeholder = first_placeholder(&sent);
        let reply = format!("{{\"a\":\"{placeholder} is on track\"}}");
        let got = receive(p.as_ref(), &mut state, "application/json", &[&reply]);
        assert_eq!(got, "{\"a\":\"project falcon is on track\"}");
    }

    #[test]
    fn a_credential_is_masked_and_a_private_key_is_removed() {
        let p = plugin("");
        let mut state = p.new_state();
        let token = fake("ghp_", "0123456789abcdef0123456789abcdef0123");
        let body = format!(
            "{{\"t\":\"{token}\",\"k\":\"-----BEGIN PRIVATE KEY-----\\nAA\\n-----END PRIVATE KEY-----\"}}"
        );
        let sent = send(p.as_ref(), &mut state, &body);
        assert!(
            !sent.contains(&token) && !sent.contains("BEGIN PRIVATE KEY"),
            "{sent}"
        );
        assert!(sent.contains("[[REIKI_PRIVATE_KEY_REMOVED]]"));
    }

    #[test]
    fn a_split_placeholder_in_a_stream_is_restored() {
        let p = plugin("terms = [\"Falcon\"]\nbuiltin = false");
        let mut state = p.new_state();
        let sent = send(p.as_ref(), &mut state, "{\"q\":\"Falcon\"}");
        let placeholder = first_placeholder(&sent);
        let (a, b) = placeholder.split_at(7);
        let one = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"Is {a}\"}}}}]}}\n\n");
        let two = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{b} ok\"}}}}]}}\n\n");
        let got = receive(
            p.as_ref(),
            &mut state,
            "text/event-stream",
            &[&one, &two, "data: [DONE]\n\n"],
        );
        assert!(got.contains("Is ") && got.contains("Falcon ok"), "{got}");
        assert!(!got.contains("REIKI"), "{got}");
    }

    #[test]
    fn rehydrate_off_leaves_placeholders_in_the_response() {
        let p = plugin("terms = [\"Falcon\"]\nbuiltin = false\nrehydrate = false");
        let mut state = p.new_state();
        let sent = send(p.as_ref(), &mut state, "{\"q\":\"Falcon\"}");
        let placeholder = first_placeholder(&sent);
        let got = receive(
            p.as_ref(),
            &mut state,
            "application/json",
            &[&format!("x {placeholder}")],
        );
        assert_eq!(got, format!("x {placeholder}"));
    }

    #[test]
    fn a_request_with_nothing_to_mask_costs_the_response_nothing() {
        let p = plugin("terms = [\"Falcon\"]");
        let mut state = p.new_state();
        let sent = send(p.as_ref(), &mut state, "{\"q\":\"hello\"}");
        assert_eq!(sent, "{\"q\":\"hello\"}");
        let got = receive(
            p.as_ref(),
            &mut state,
            "application/json",
            &["[[REIKI_TERM_aaaaaaaa]]"],
        );
        assert_eq!(got, "[[REIKI_TERM_aaaaaaaa]]", "not ours, so not touched");
    }

    #[test]
    fn requests_do_not_share_what_they_masked() {
        let p = plugin("terms = [\"Falcon\"]\nbuiltin = false");
        let (mut one, mut two) = (p.new_state(), p.new_state());
        let sent = send(p.as_ref(), &mut one, "{\"q\":\"Falcon\"}");
        let placeholder = first_placeholder(&sent);
        send(p.as_ref(), &mut two, "{\"q\":\"nothing here\"}");
        let got = receive(p.as_ref(), &mut two, "application/json", &[&placeholder]);
        assert_eq!(got, placeholder, "the second request never masked it");
    }

    #[test]
    fn terms_come_from_a_file_with_comments_and_blank_lines() {
        let dir: PathBuf = std::env::temp_dir().join(format!("reiki-ph-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("terms.txt");
        std::fs::write(&file, "# clients\nAcme Corp\n\n  Osprey Labs  \n").unwrap();
        let p = plugin(&format!(
            "terms_file = \"{}\"\nbuiltin = false",
            file.display()
        ));
        let mut state = p.new_state();
        let sent = send(
            p.as_ref(),
            &mut state,
            "{\"q\":\"Acme Corp and osprey labs\"}",
        );
        assert_eq!(sent.matches("[[REIKI_TERM_").count(), 2, "{sent}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_body_that_cannot_be_masked_is_refused_not_sent_raw() {
        let p = plugin("terms = [\"Falcon\"]");
        let mut state = p.new_state();
        let mut headers = HeaderMap::new();
        let mut binary = Bytes::from_static(&[0xff, 0xfe, 0x00]);
        let refused = p.on_request(&mut RequestCtx::new(
            "ct",
            "zen",
            &Method::POST,
            "/zen/x",
            &mut headers,
            &mut binary,
            state.as_mut(),
        ));
        assert!(matches!(
            refused,
            Err(PluginError::Reject(StatusCode::UNSUPPORTED_MEDIA_TYPE, _))
        ));

        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        let mut text = Bytes::from_static(b"{}");
        let refused = p.on_request(&mut RequestCtx::new(
            "ct",
            "zen",
            &Method::POST,
            "/zen/x",
            &mut headers,
            &mut text,
            state.as_mut(),
        ));
        assert!(matches!(refused, Err(PluginError::Reject(..))));
    }

    #[test]
    fn a_missing_terms_file_stops_the_plugin_from_building() {
        let table = "terms_file = \"/nonexistent/terms.txt\"".parse().unwrap();
        assert!(build(&table).is_err());
        let dry = "terms_file = \"/nonexistent/terms.txt\"\ndry_run = true"
            .parse()
            .unwrap();
        assert!(build(&dry).is_ok(), "reiki check must not need the file");
    }
}
