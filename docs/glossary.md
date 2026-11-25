# Glossary

Terms used in the documentation and command output. Each term links to the page that describes it in detail.

| Term | Meaning | See |
| --- | --- | --- |
| stream | A client identity: its token authenticates requests and its chain lists the keys it may use. | [keys](keys.md), [configuration](configuration.md) |
| chain | The ordered list of keys a stream may try for a request. | [failover](failover.md), [configuration](configuration.md) |
| strategy | How a stream picks the next key: `sticky` stays on one key until it cannot serve; `round_robin` rotates. | [failover](failover.md) |
| key | An upstream credential read from a file. It can be available, cooling or disabled. | [keys](keys.md), [failover](failover.md) |
| upstream | A provider endpoint as Reiki sees it: a base URL, credential format and request rules. Clients reach it at `/<name>`. | [providers](providers.md), [configuration](configuration.md) |
| provider | The service behind an upstream, such as an LLM API. | [providers](providers.md) |
| route | The request path after the upstream prefix. Cooldowns can apply to one route and model pair. | [failover](failover.md) |
| model scope | A cooldown limited to one `(route, model)` pair, so the key stays usable for other models. | [failover](failover.md) |
| cooldown | A period during which a key, or one route and model on it, is skipped after a failure. | [failover](failover.md) |
| cooling | A key state where the key waits out a cooldown before it can be tried again. | [failover](failover.md) |
| disabled | A key state where the key is out of rotation until it is re-enabled or its key file changes on reload. Called `out_of_rotation` in `/v1/status`. | [failover](failover.md), [monitoring](monitoring.md) |
| available | A key state where the key can be selected by a chain. Called `ok` in `reiki keys`. | [failover](failover.md) |
| passthrough | An upstream mode that forwards the client's own credentials and query string without using a Reiki key or token check. | [providers](providers.md) |
| fallback | Another upstream tried once when the source upstream's chain cannot serve a request. | [failover](failover.md), [providers](providers.md) |
| peer | Another Reiki instance whose published key health this instance can read. Peers do not route requests. | [provider health](provider-health.md) |
| leaf | A short-lived certificate minted by the interception CA for one intercepted host. | [tunnel](tunnel.md), [security](security.md) |
| plugin | A compiled-in component that can inspect or change requests and responses. Runtime Wasm components need `plugin-wasm`. | [plugins](plugins.md) |
| hook | A plugin callback point, such as request hooks and response head or chunk hooks. | [plugins](plugins.md), [overview](overview.md) |
| placeholder | A stable token substituted for a credential or listed term before a request leaves, restored in the response. | [plugins](plugins.md) |
| stream-admin token | The dedicated bearer token for dynamic stream management, distinct from the ordinary admin token. | [keys](keys.md), [configuration](configuration.md) |

`reiki keys` and `GET /v1/status` describe the same key states with different names: `ok` is `available`, `cooling` is `cooling`, and `disabled` is `out_of_rotation`. See [failover](failover.md) for the state transitions.
