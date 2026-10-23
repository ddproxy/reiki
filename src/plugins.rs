//! Assembles the compiled-in plugins named in the config into a [`PluginChain`].
//!
//! Plugins are compiled in, never loaded at runtime. [`builtin`] lists the ones
//! this binary contains; their crate features are enabled when reiki is built.
//! A plugin named in `[plugins.*]` but absent from [`builtin`] is a config
//! error, so a typo fails at startup instead of silently doing nothing.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use toml::Value;

use reiki_plugin::{ChunkCtx, Plugin, PluginError, RequestCtx, ResponseHeadCtx};

use crate::config::ConfigError;

/// What to do when a plugin declines a request with [`PluginError::Skip`].
/// [`PluginError::Reject`] always rejects regardless of this setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Log and continue with the remaining plugins, leaving the request alone.
    PassThrough,
    /// Stop and return 503 to the client.
    Reject,
}

/// A request the chain stopped before it reached an upstream.
#[derive(Debug)]
pub struct Blocked {
    pub status: StatusCode,
    pub message: String,
}

/// One compiled-in plugin and how to build it from its config table.
pub struct Registration {
    /// The `[plugins.<name>]` key this registers.
    pub name: &'static str,
    /// Parses the plugin's table. The table is passed through untouched, so the
    /// plugin sees every key, including `enabled` and `on_error`.
    pub build: fn(&toml::Table) -> Result<Box<dyn Plugin>, String>,
}

struct Entry {
    plugin: Box<dyn Plugin>,
    on_error: OnError,
}

/// The configured plugins in chain order, run on every request.
#[derive(Default)]
pub struct PluginChain {
    entries: Vec<Entry>,
}

impl fmt::Debug for PluginChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

impl PluginChain {
    /// Builds the chain from the `[plugins.*]` tables and the compiled-in
    /// registry. Names must be known; `enabled = false` drops a plugin without
    /// it being an error.
    pub fn build(
        configured: &BTreeMap<String, toml::Table>,
        registry: &[Registration],
    ) -> Result<Self, ConfigError> {
        for name in configured.keys() {
            if !registry.iter().any(|r| r.name == name) {
                return Err(ConfigError::UnknownPlugin {
                    name: name.clone(),
                    known: registry.iter().map(|r| r.name.to_string()).collect(),
                });
            }
        }

        let mut entries = Vec::new();
        for reg in registry {
            let Some(table) = configured.get(reg.name) else {
                continue;
            };
            if !enabled(reg.name, table)? {
                continue;
            }
            let on_error = on_error(reg.name, table)?;
            let plugin = (reg.build)(table).map_err(|message| ConfigError::PluginConfig {
                name: reg.name.to_string(),
                message,
            })?;
            entries.push(Entry { plugin, on_error });
        }
        Ok(Self { entries })
    }

    /// True when no plugin is configured, so the body path can skip the
    /// per-request work entirely.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Names of the active plugins, for startup logging.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.plugin.name())
    }

    /// Creates the per-request state for every plugin, in chain order.
    pub fn states(&self) -> Vec<Box<dyn Any + Send>> {
        self.entries.iter().map(|e| e.plugin.new_state()).collect()
    }

    /// Runs `on_request` on every plugin, in order, mutating the request in
    /// place. Stops at the first rejection.
    pub fn on_request(
        &self,
        states: &mut [Box<dyn Any + Send>],
        stream: &str,
        upstream: &str,
        method: &Method,
        headers: &mut HeaderMap,
        body: &mut Bytes,
    ) -> Result<(), Blocked> {
        for (entry, state) in self.entries.iter().zip(states.iter_mut()) {
            let mut ctx = RequestCtx {
                stream,
                upstream,
                method,
                headers: &mut *headers,
                body: &mut *body,
                state: &mut **state,
            };
            match entry.plugin.on_request(&mut ctx) {
                Ok(()) => {}
                Err(PluginError::Reject(status, message)) => {
                    tracing::warn!(
                        plugin = entry.plugin.name(),
                        status = status.as_u16(),
                        %message,
                        "plugin rejected the request"
                    );
                    return Err(Blocked { status, message });
                }
                Err(PluginError::Skip(message)) => match entry.on_error {
                    OnError::PassThrough => {
                        tracing::debug!(
                            plugin = entry.plugin.name(),
                            %message,
                            "plugin skipped the request"
                        );
                    }
                    OnError::Reject => {
                        tracing::warn!(
                            plugin = entry.plugin.name(),
                            %message,
                            "plugin skipped the request; rejecting per on_error"
                        );
                        return Err(Blocked {
                            status: StatusCode::SERVICE_UNAVAILABLE,
                            message,
                        });
                    }
                },
            }
        }
        Ok(())
    }

    /// Runs `on_response_head` on every plugin, in order.
    pub fn on_response_head(
        &self,
        states: &mut [Box<dyn Any + Send>],
        status: &mut StatusCode,
        headers: &mut HeaderMap,
    ) {
        for (entry, state) in self.entries.iter().zip(states.iter_mut()) {
            let mut ctx = ResponseHeadCtx {
                status: &mut *status,
                headers: &mut *headers,
                state: &mut **state,
            };
            entry.plugin.on_response_head(&mut ctx);
        }
    }

    /// Runs `on_response_chunk` on every plugin, in order, letting each rewrite
    /// `chunk` before the next sees it.
    pub fn on_response_chunk(
        &self,
        states: &mut [Box<dyn Any + Send>],
        chunk: &mut Bytes,
        is_last: bool,
    ) {
        for (entry, state) in self.entries.iter().zip(states.iter_mut()) {
            let mut ctx = ChunkCtx {
                chunk: &mut *chunk,
                is_last,
                state: &mut **state,
            };
            entry.plugin.on_response_chunk(&mut ctx);
        }
    }
}

fn enabled(name: &str, table: &toml::Table) -> Result<bool, ConfigError> {
    match table.get("enabled") {
        None => Ok(true),
        Some(Value::Boolean(value)) => Ok(*value),
        Some(other) => Err(ConfigError::PluginConfig {
            name: name.to_owned(),
            message: format!("`enabled` must be a boolean, got {}", other.type_str()),
        }),
    }
}

fn on_error(name: &str, table: &toml::Table) -> Result<OnError, ConfigError> {
    match table.get("on_error") {
        None => Ok(OnError::PassThrough),
        Some(Value::String(value)) => match value.as_str() {
            "pass_through" => Ok(OnError::PassThrough),
            "reject" => Ok(OnError::Reject),
            other => Err(ConfigError::PluginConfig {
                name: name.to_owned(),
                message: format!(
                    "`on_error` must be \"pass_through\" or \"reject\", got \"{other}\""
                ),
            }),
        },
        Some(other) => Err(ConfigError::PluginConfig {
            name: name.to_owned(),
            message: format!("`on_error` must be a string, got {}", other.type_str()),
        }),
    }
}

/// The plugins compiled into this binary.
///
/// Add a crate's plugin here behind its feature, for example:
///
/// ```ignore
/// #[cfg(feature = "plugin-secrets")]
/// plugins.push(Registration {
///     name: "secrets",
///     build: |table| reiki_secrets::from_table(table),
/// });
/// ```
pub fn builtin() -> Vec<Registration> {
    vec![
        #[cfg(feature = "plugin-requestlog")]
        Registration {
            name: reiki_requestlog::NAME,
            build: reiki_requestlog::build,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::CONTENT_LENGTH;
    use std::any::Any;

    struct Append(&'static str);
    impl Plugin for Append {
        fn name(&self) -> &str {
            "append"
        }
        fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
            let mut body = ctx.body.to_vec();
            body.extend_from_slice(self.0.as_bytes());
            *ctx.body = Bytes::from(body);
            Ok(())
        }
    }

    struct Swap {
        placeholder: &'static str,
        value: &'static str,
    }
    impl Plugin for Swap {
        fn name(&self) -> &str {
            "swap"
        }
        fn new_state(&self) -> Box<dyn Any + Send> {
            Box::new(Vec::<u8>::new())
        }
        fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
            let text = String::from_utf8_lossy(ctx.body).replace(self.value, self.placeholder);
            *ctx.body = Bytes::from(text);
            Ok(())
        }
        fn on_response_head(&self, ctx: &mut ResponseHeadCtx<'_>) {
            ctx.headers.remove(CONTENT_LENGTH);
        }
        fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
            let buffer = ctx.state.downcast_mut::<Vec<u8>>().unwrap();
            buffer.extend_from_slice(&ctx.chunk[..]);
            if ctx.is_last {
                let text = String::from_utf8_lossy(buffer).replace(self.placeholder, self.value);
                *ctx.chunk = Bytes::from(text);
                buffer.clear();
            } else {
                *ctx.chunk = Bytes::new();
            }
        }
    }

    struct Decline;
    impl Plugin for Decline {
        fn name(&self) -> &str {
            "decline"
        }
        fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
            Err(PluginError::Skip("no secret for this request".into()))
        }
    }

    struct Refuse;
    impl Plugin for Refuse {
        fn name(&self) -> &str {
            "refuse"
        }
        fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
            Err(PluginError::Reject(
                StatusCode::FORBIDDEN,
                "not allowed".into(),
            ))
        }
    }

    fn registry() -> Vec<Registration> {
        vec![
            Registration {
                name: "swap",
                build: |_| {
                    Ok(Box::new(Swap {
                        placeholder: "<PLACEHOLDER>",
                        value: "s3cr3t",
                    }) as Box<dyn Plugin>)
                },
            },
            Registration {
                name: "append",
                build: |_| Ok(Box::new(Append("-tail")) as Box<dyn Plugin>),
            },
            Registration {
                name: "decline",
                build: |_| Ok(Box::new(Decline) as Box<dyn Plugin>),
            },
            Registration {
                name: "refuse",
                build: |_| Ok(Box::new(Refuse) as Box<dyn Plugin>),
            },
        ]
    }

    fn configured(source: &str) -> BTreeMap<String, toml::Table> {
        toml::from_str(source).unwrap()
    }

    fn request(
        chain: &PluginChain,
        states: &mut [Box<dyn Any + Send>],
        body: &mut Bytes,
    ) -> Result<(), Blocked> {
        let mut headers = HeaderMap::new();
        chain.on_request(states, "default", "zen", &Method::POST, &mut headers, body)
    }

    #[test]
    fn unknown_plugin_names_are_rejected() {
        let err = PluginChain::build(&configured("[nope]"), &registry()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("nope"), "error was: {text}");
        assert!(text.contains("append"), "error was: {text}");
    }

    #[test]
    fn disabled_plugins_are_dropped() {
        let chain =
            PluginChain::build(&configured("[append]\nenabled = false\n"), &registry()).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn plugins_run_in_registry_order() {
        // Registry order is swap, append; the config lists them the other way.
        let chain = PluginChain::build(&configured("[append]\n[swap]\n"), &registry()).unwrap();
        assert_eq!(chain.names().collect::<Vec<_>>(), vec!["swap", "append"]);
    }

    #[test]
    fn a_plugin_transforms_the_request_body() {
        let chain = PluginChain::build(&configured("[append]"), &registry()).unwrap();
        let mut states = chain.states();
        let mut body = Bytes::from_static(b"hi");
        request(&chain, &mut states, &mut body).unwrap();
        assert_eq!(&body[..], b"hi-tail");
    }

    #[test]
    fn reject_stops_the_chain_with_its_status() {
        let chain = PluginChain::build(&configured("[append]\n[refuse]\n"), &registry()).unwrap();
        let mut states = chain.states();
        let mut body = Bytes::new();
        let blocked = request(&chain, &mut states, &mut body).unwrap_err();
        assert_eq!(blocked.status, StatusCode::FORBIDDEN);
        assert_eq!(blocked.message, "not allowed");
        // The plugin before it still ran.
        assert_eq!(&body[..], b"-tail");
    }

    #[test]
    fn skip_passes_through_by_default() {
        let chain = PluginChain::build(&configured("[decline]\n[append]\n"), &registry()).unwrap();
        let mut states = chain.states();
        let mut body = Bytes::new();
        request(&chain, &mut states, &mut body).unwrap();
        assert_eq!(&body[..], b"-tail");
    }

    #[test]
    fn skip_rejects_when_on_error_says_so() {
        let chain = PluginChain::build(
            &configured("[decline]\non_error = \"reject\"\n"),
            &registry(),
        )
        .unwrap();
        let mut states = chain.states();
        let mut body = Bytes::new();
        let blocked = request(&chain, &mut states, &mut body).unwrap_err();
        assert_eq!(blocked.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_bad_on_error_value_is_a_config_error() {
        let err = PluginChain::build(
            &configured("[append]\non_error = \"explode\"\n"),
            &registry(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("explode"), "error was: {err}");
    }

    #[test]
    fn response_bytes_split_across_chunks_are_reassembled() {
        let chain = PluginChain::build(&configured("[swap]"), &registry()).unwrap();
        let mut states = chain.states();
        let mut status = StatusCode::OK;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, "31".parse().unwrap());
        chain.on_response_head(&mut states, &mut status, &mut headers);
        assert!(headers.get(CONTENT_LENGTH).is_none());

        let payload = b"echo <PLACEHOLDER> end";
        for split in 0..=payload.len() {
            let mut release = Vec::new();
            let (first, second) = payload.split_at(split);
            let mut chunk = Bytes::copy_from_slice(first);
            chain.on_response_chunk(&mut states, &mut chunk, false);
            release.extend_from_slice(&chunk);
            let mut chunk = Bytes::copy_from_slice(second);
            chain.on_response_chunk(&mut states, &mut chunk, true);
            release.extend_from_slice(&chunk);
            assert_eq!(release, b"echo s3cr3t end", "split at {split}");
        }
    }
}
