# Glossary

Terms used in the documentation and command output. Each term links to the page that describes it in detail.

| Term | Meaning | See |
| --- | --- | --- |
| stream | A client identity: its token authenticates requests and its chain lists the keys it may use. | [configuration](configuration.md#streamsname), [security](security.md#clients-and-keys) |
| chain | The ordered list of keys a stream may try for a request. | [failover](failover.md#chains), [configuration](configuration.md#streamsname) |
| strategy | How a stream picks the next key in its chain: `sticky` stays on one key until it cannot serve, `round_robin` rotates. | [failover](failover.md#chains), [configuration](configuration.md#streamsname) |
| key | An upstream credential read from a file. A key can be available, cooling or out of rotation. | [configuration](configuration.md#keysname), [failover](failover.md#key-states) |
| upstream | A provider endpoint as Reiki sees it: a base URL, credential format and request rules. Clients reach it at `/<name>`. | [providers](providers.md), [configuration](configuration.md#upstreamname) |
| provider | The service behind an upstream, such as an LLM API. | [providers](providers.md) |
| route | The request path after the upstream prefix. Cooldowns can apply to one route and model pair. | [failover](failover.md#scoped-cooldowns) |
| model scope | A cooldown limited to one `(route, model)` combination, so the key stays usable for other models. | [failover](failover.md#scoped-cooldowns) |
| cooldown | A period during which a key, or one route and model on it, is skipped after a failure. | [failover](failover.md#cooldowns) |
| cooling | A key state where the key waits out a cooldown before it can be tried again. | [failover](failover.md#key-states) |
| disabled | A key state where the key is out of rotation until it is re-enabled or its key file changes on reload. Called `out_of_rotation` in `/v1/status`. | [failover](failover.md#re-enabling-keys), [monitoring](monitoring.md#post-keysenable) |
| available | A key state where the key can be selected by a chain. Called `ok` in `reiki keys`. | [failover](failover.md#key-states) |
| passthrough | An upstream mode that forwards the client's own credentials and query string without using a Reiki key or token check. | [providers](providers.md#passing-the-clients-own-credentials-through) |
| fallback | Another upstream tried once when the source upstream's chain cannot serve a request. | [failover](failover.md#fallback-upstream), [providers](providers.md#falling-back-to-another-upstream) |
| peer | Another Reiki instance whose published key health this instance can read. Peers do not route requests. | [peers](peers.md) |
| leaf | A short-lived certificate minted by the interception CA for one intercepted host. | [tunnel](tunnel.md#interception), [security](security.md#interception) |
| hub | A planned shared-key service described by `[hub]`, `[hubs.*]` and `[accounts.*]` tables. In this release the tables are parsed and validated only. | [hub](hub.md) |
| plugin | A compiled-in component that can inspect or change requests and responses. | [plugins](plugins.md) |
| hook | A plugin callback point, such as request hooks and response head or chunk hooks. | [plugins](plugins.md), [architecture](architecture.md) |
| placeholder | A stable token substituted for a credential or listed term before a request leaves, and restored in the response. | [placeholders](placeholders.md) |

## Key state names in the CLI and `/v1/status`

`reiki keys` and `GET /v1/status` describe the same key states with different names.

| `GET /v1/status` (`state`) | `reiki keys` table | Meaning |
| --- | --- | --- |
| `available` | `ok` | The key can be selected by a chain. |
| `cooling` | `cooling` | The key is waiting out a cooldown. |
| `out_of_rotation` | `disabled` | The key is out of rotation until it is re-enabled or its key file changes on reload. |

The mapping is implemented in `src/keys.rs` (`parse_status`) from the admin status document built in `src/admin.rs`.
