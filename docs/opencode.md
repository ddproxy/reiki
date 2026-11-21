# OpenCode client setup

Reiki can sit between OpenCode and an OpenAI-compatible upstream such as OpenCode Zen. The client sends a Reiki stream token; Reiki sets the provider key on each request. Check the path and the token in Reiki before you trust the setup.

## Configure the upstream

```toml
listen = "127.0.0.1:4097"

[admin]
listen = "127.0.0.1:4098"
token_sha256 = "<admin token hash>"

[plugins.metrics]

[upstream.zen]
base_url = "https://opencode.ai/zen/v1"

[keys.zen-a]
upstream = "zen"
file = "/etc/reiki/keys/zen-a"

[streams.client-a]
token_sha256 = "<client token hash>"
chain = ["zen-a"]
```

Create the key file and hashes as in the [quickstart](quickstart.md#2-write-the-config). Keep the admin token for the proof command below.

## Point OpenCode at Reiki

The repository names two OpenCode settings:

```text
provider.opencode.options.baseURL = "http://127.0.0.1:4097/zen"
provider.opencode.options.apiKey   = "<client token>"
```

`apiKey` is the Reiki stream token, not a provider key. Set the client base URL to `http://127.0.0.1:4097/<upstream name>`; the first path segment selects `[upstream.<name>]`. See [providers](providers.md#opencode-zen).

> **`/v1` rule.** Reiki appends the request path to the upstream's `base_url`. With `base_url = "https://opencode.ai/zen/v1"`, send client requests to `/zen/chat/completions`, not `/zen/v1/chat/completions`. Adding `/v1` on the client produces `/v1/v1/...` upstream. For Anthropic, the opposite applies: the upstream `base_url` is `https://api.anthropic.com`, and the SDK adds `/v1/messages`. See [providers](providers.md#anthropic).

## Prove one request used Reiki

`[plugins.metrics]` serves `/metrics` on the admin listener. Send one request from OpenCode, then read the counter:

```bash
curl -s -H "Authorization: Bearer $REIKI_ADMIN_TOKEN" \
  http://127.0.0.1:4098/metrics | grep reiki_requests_total
```

One successful request on this stream produces a line like:

```text
reiki_requests_total{stream="client-a",upstream="zen",status="success"} 1
```

The counter is one per request, labelled by stream, serving upstream and client-visible status. It proves that request reached that Reiki route. It does not prove that every OpenCode request used Reiki. See [monitoring](monitoring.md#get-metrics).

Two more checks agree with it:

| Check | What it proves |
| --- | --- |
| `REIKI_ADMIN_TOKEN=... reiki keys` | `LAST` changes after an upstream attempt for the key in the stream's chain |
| Request log with `stream`, `upstream`, `key` and `id` on one request | The request passed through Reiki and used that key; `id` joins the service log |

Enable `[plugins.requestlog]` with a `file` or `socket` before sending the test request; see [request log](requestlog.md#records).

## What can bypass Reiki

| Bypass | Effect | Status in this repo |
| --- | --- | --- |
| A provider configured in the client with its own API key and base URL | That provider is called directly; no Reiki token, key, plugin or counter applies | General rule: upstreams come from Reiki's config; the exact OpenCode settings are not in this repo |
| A sub-agent or second client process with a different config or environment | Its requests do not reach this listener if it does not inherit the base URL and token | Client behaviour not verifiable here |
| A utility or auxiliary model configured outside the Reiki upstream | Its calls use a different base URL and key | Client behaviour not verifiable here |
| An upstream marked `passthrough = true` | The request crosses Reiki, but no Reiki token is checked and the client's own credentials and query string go upstream; no Reiki key or failover applies | Verified in [providers](providers.md#passing-the-clients-own-credentials-through) |
| A tunnel destination that is allowed but not intercepted | Bytes cross Reiki's CONNECT proxy without plugin inspection | Verified in [limits](limits.md) |

> **Unverified.** This repository has no OpenCode source, config examples for version 2.3.x, or client tests. It confirms only `provider.opencode.options.baseURL` and `apiKey` as named in [providers](providers.md#opencode-zen) and the README. It cannot confirm the file that holds those settings, the full OpenCode 2.3.x config shape, or how that version routes sub-agents and utility models. Check those in the client you run.
