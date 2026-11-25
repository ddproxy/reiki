# Clients

A client points at a Reiki listener and sends a Reiki stream token instead of a provider key. Reiki selects the upstream from the first path segment, replaces the credential with a key from the stream's chain, and applies plugins. Verify the route with a counter or the request log before relying on it. This repository contains no client source or version-specific client tests; verify client settings in the client you run.

## Common setup

```toml
listen = "127.0.0.1:4097"

[admin]
listen = "127.0.0.1:4098"
token_sha256 = "<admin token hash>"

[plugins.metrics]

[upstream.example]
base_url = "https://api.example.com/v1"

[keys.example-a]
upstream = "example"
file = "/etc/reiki/keys/example-a"

[streams.client-a]
token_sha256 = "<client token hash>"
chain = ["example-a"]
```

Create the key file and hashes as in the [quickstart](quickstart.md#2-write-the-config). Set the client base URL to `http://127.0.0.1:4097/<upstream name>`; the first path segment selects `[upstream.<name>]`.

Clients may send the stream token as `Authorization: Bearer <token>`, `x-api-key`, `x-goog-api-key` or `?key=`; the first form found wins. On key-backed routes Reiki removes client credential headers and the `key` query parameter before setting the upstream credential. A `passthrough` upstream forwards the client's own credentials and query unchanged. See [security](security.md#clients-and-keys) and [providers](providers.md).

**`/v1` rule.** Reiki appends the client's request path to the upstream `base_url`. With `base_url = "https://opencode.ai/zen/v1"`, send `/zen/chat/completions`, not `/zen/v1/chat/completions`. Anthropic is the opposite: the upstream `base_url` is `https://api.anthropic.com` and the SDK adds `/v1/messages`. Do not add `/v1` to the upstream or client URL. See [OpenCode Zen](providers.md#opencode-zen) and [Anthropic](providers.md#anthropic).

## OpenCode

```text
provider.opencode.options.baseURL = "http://127.0.0.1:4097/zen"
provider.opencode.options.apiKey   = "<client token>"
```

`apiKey` is the Reiki stream token, not a provider key. The repository names these two settings; it does not include a client config file or client tests. See [OpenCode Zen](providers.md#opencode-zen).

## Claude Code

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:4097/anthropic
claude
```

Configure the client so outgoing requests carry `x-api-key: <client token>`; do not put a provider key there. Use `[upstream.anthropic]` with `base_url = "https://api.anthropic.com"`, `auth_header = "x-api-key"` and `auth_format = "{key}"`, a key file under `[keys]`, and a `[streams]` entry whose token Claude Code sends.

For a Claude Pro/Max OAuth login, mark the upstream `passthrough = true` and omit the `[streams]` entry: Reiki forwards `authorization` and `x-api-key` unchanged, with no Reiki key, token or failover. Keep the listener on loopback or behind your own access control. See [passing the client's own credentials through](providers.md#passing-the-clients-own-credentials-through).

With `placeholders` enabled, message text, tool results and stop sequences are masked on the way out and restored in responses. Thinking blocks, signatures and base64 images or PDFs pass byte for byte and are not scanned. See [plugins](plugins.md).

## Prove one request used Reiki

`[plugins.metrics]` serves `GET /v1/metrics` on the admin listener. Send one request from the client, then:

```bash
curl -s -H "Authorization: Bearer $REIKI_ADMIN_TOKEN" \
  http://127.0.0.1:4098/v1/metrics | grep reiki_requests_total
```

A successful request produces a counter labelled by stream, upstream and status:

```text
reiki_requests_total{stream="client-a",upstream="example",status="success"} 1
```

The counter proves that request reached that route, not that every client request used Reiki. Two more checks agree: `reiki keys` with the admin token shows `LAST` changing after an upstream attempt for a key in the stream's chain, and a [request log](requestlog.md#records) record carries the same `id`, stream, upstream, serving key name and attempts. Enable `[plugins.requestlog]` with a `file` or `socket` before the test request. See [monitoring](monitoring.md#get-v1metrics) and [keys](keys.md).

## What can bypass Reiki

| Bypass | Effect |
| --- | --- |
| A process that does not inherit the client base URL and token | It calls the provider directly; no Reiki token, key, plugin or counter applies |
| A second client config, sub-agent or utility model with its own endpoint | Its requests do not reach this listener |
| An upstream marked `passthrough = true` | Credentials pass through unchanged; no Reiki key or failover applies |
| A tunnel destination that is allowed but not intercepted | Bytes cross the CONNECT proxy without plugin inspection |

The first two are client behaviour and are not verified here. The last two are documented in [providers](providers.md#passing-the-clients-own-credentials-through) and [tunnel](tunnel.md#trust-boundary). See also [what Reiki does not protect against](security.md#what-reiki-does-not-protect-against).
