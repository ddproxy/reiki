# Claude Code client setup

Point Claude Code at an Anthropic Messages upstream through Reiki. Reiki holds the provider key; Claude Code sends a Reiki stream token as `x-api-key`. Prove the route with the admin counter before using it for real work.

## Configure the upstream

```toml
listen = "127.0.0.1:4097"

[admin]
listen = "127.0.0.1:4098"
token_sha256 = "<admin token hash>"

[plugins.metrics]

[upstream.anthropic]
base_url = "https://api.anthropic.com"
auth_header = "x-api-key"
auth_format = "{key}"

[keys.anthropic-a]
upstream = "anthropic"
file = "/etc/reiki/keys/anthropic-a"

[streams.client-a]
token_sha256 = "<client token hash>"
chain = ["anthropic-a"]
```

Create the key file and hashes as in the [quickstart](quickstart.md#2-write-the-config).

The client base URL is `http://127.0.0.1:4097/anthropic`. Set it for the session:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:4097/anthropic
claude
```

The repository shows `ANTHROPIC_BASE_URL` with `claude`, and says the stream token is sent as `x-api-key`; see [placeholders](placeholders.md#claude-code). It does not name the Claude Code setting that supplies that header. Configure your client so the outgoing request carries `x-api-key: <client token>`, then use the proof step below. Do not put an Anthropic key there.

> **`/v1` rule.** Keep `/v1` out of `[upstream.anthropic].base_url` and out of the client base URL. The SDK adds `/v1/messages`, and Reiki appends that path to `https://api.anthropic.com`. It passes the SDK's `anthropic-version` header through. Adding `/v1` yourself produces `/v1/v1/messages`. See [providers](providers.md#anthropic).

## OAuth login and passthrough

For a Claude Pro/Max login, mark the upstream `passthrough = true` and do not configure a `[streams]` entry for it. Reiki forwards `authorization` and `x-api-key` unchanged.

```toml
[upstream.anthropic-direct]
base_url = "https://api.anthropic.com"
passthrough = true
```

A passthrough upstream has no Reiki token, no Reiki key and no failover. It still applies plugins and the request log, which names the key `-`. Keep the listener on loopback or behind your own access control. See [providers](providers.md#passing-the-clients-own-credentials-through).

## Prove one request used Reiki

`[plugins.metrics]` serves `/v1/metrics` on the admin listener. Send one message from Claude Code, then read the counter:

```bash
curl -s -H "Authorization: Bearer $REIKI_ADMIN_TOKEN" \
  http://127.0.0.1:4098/v1/metrics | grep reiki_requests_total
```

One successful request on this stream produces a line like:

```text
reiki_requests_total{stream="client-a",upstream="anthropic",status="success"} 1
```

The counter proves that request reached that Reiki route. Request-log records carry the same `id`, the stream, the upstream, the serving key name and the attempts; `id` joins them to the service log. Enable `[plugins.requestlog]` with a `file` or `socket` before the test request. See [request log](requestlog.md#records) and [monitoring](monitoring.md#get-v1metrics).

With `[plugins.placeholders]`, text, tool results and stop sequences are masked on the way out and restored on the way back. Thinking blocks, signatures and base64 images or PDFs pass byte for byte and are not scanned; see [placeholders](placeholders.md#what-it-does) and [limits](limits.md).

## What can bypass Reiki

| Bypass | Effect | Status in this repo |
| --- | --- | --- |
| A session or process that does not inherit `ANTHROPIC_BASE_URL` | It uses the provider's own base URL and credentials; no Reiki token, key, plugin or counter applies | Client behaviour not verifiable here |
| A second client config, sub-agent or utility model with its own endpoint | Its calls do not reach this listener | Client behaviour not verifiable here |
| A passthrough upstream | Credentials go through Reiki unchanged; no Reiki key is held or rotated | Verified in [providers](providers.md#passing-the-clients-own-credentials-through) |
| A tunnel destination that is allowed but not intercepted | Bytes cross Reiki's CONNECT proxy without plugin inspection | Verified in [limits](limits.md) |

> **Unverified.** This repository contains no Claude Code source or client tests. It documents `ANTHROPIC_BASE_URL`, `claude`, the `x-api-key` stream token and the OAuth passthrough case, but it cannot confirm the current Claude Code config file, how environment variables are inherited by sub-processes, or how utility models are selected. Verify those in the client you run.
