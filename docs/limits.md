# What Reiki does not protect against

Reiki controls traffic that reaches its listeners. It is not a sandbox for the client, a secret store for the host, or a proof that every model call used the proxy. Read this page before treating a Reiki control as a boundary.

| Threat | Covered by | Not covered |
| --- | --- | --- |
| An agent or another host reading a key file | Directory mode `0700`, file mode `0600`, `umask 077`, and `reiki keys --check` warnings on group or other read bits | Any process running as the Reiki user, or as root, can read the file. The daemon reads, trims and length-checks key files at startup and on reload; it does not check their mode. |
| A client getting an upstream key | Reiki removes client credential headers and the `key` query parameter, then sets the upstream credential per attempt | A `passthrough` upstream forwards the client's own credentials and query string. An upstream echo of a key in a form outside the scrubber's list is a limit; see the encoding row below. |
| A plugin reading or changing a request | Plugins are compiled in and run inside Reiki; they never receive the upstream key | A plugin is trusted code. It sees the client token, request headers, path and body, and can rewrite bodies or reject requests. Panics are caught, not sandboxed. Only compile in plugins you have read. |
| Traffic to arbitrary destinations | Config-only upstreams and a `[tunnel]` allow-list | An allowed CONNECT destination stays opaque unless `[tunnel.intercept]` lists its host. No token is required on the tunnel socket, so restrict network access. `allow = ["*"]` is legal and leaves every destination it permits (port 443 by default) unread by plugins. |
| Credentials echoed in responses | The scrubber replaces the key, its formatted credential, JSON and percent escapes, ASCII whitespace/control splits, and base64 in standard and URL-safe forms, padded or unpadded, including base64 at offsets 1 and 2 | Other encodings and compressed bodies are not covered. A key embedded more than two bytes into a larger base64 value is not covered. Reiki refuses a response still carrying a coding it cannot decode when a key is in play, rather than relaying it unscrubbed. |
| An identifier in a request body | `phi`, `finance` and `controlled` can report or block structured kinds; `placeholders` masks listed terms and credential shapes | Detectors read request bodies only. Headers, the query string and responses are not checked. Base64 `image` and `document` blocks and `thinking`, `signature` and `redacted_thinking` payloads pass byte for byte and are not scanned. See [phi](phi.md#what-it-does-not-cover) and [placeholders](placeholders.md#limits). |
| A prompt or logged identifier reaching a log reader | `requestlog` records summaries by default; `capture_bodies` defaults to `false`, and captured bodies are masked in paths, query strings, headers and JSON and capped per entry | Masking is pattern based; it will not catch a password in prose or personal data. Enabling `capture_bodies` warns at startup. With a detector in `report` mode, leave body capture off, or set `body = "none"`, so detected identifiers are not written. |
| A plugin failure during masking | `placeholders` fails closed: the default `on_error` is `reject`, and a skip answers `503` | `on_error = "pass_through"` lets the skipped request continue with the body unmasked. That is an explicit fail-open choice. |
| Traffic that never reaches Reiki | A stream token, one route per upstream prefix, and counters on that route | A client that configures a provider with its own key and base URL, a sub-agent with a different config, or a utility model with its own endpoint does not pass through Reiki. This repository cannot confirm the exact OpenCode or Claude Code settings that cause those calls. See [OpenCode](opencode.md) and [Claude Code](claude-code.md). |
| One client affecting shared keys | `hold_unauthorized` can delay a disable until a later key serves the request, and separate instances give separate key state | A stream-token holder can still influence key health: a `401`, or a `403` or `400` the upstream attributes to the key, takes that key out of rotation for every stream that lists it. Run separate instances for mutually untrusted clients. |

## Key files are not a host boundary

Write the file with mode `0600`, keep its directory private, and run `reiki keys --check` in deployment checks. That command checks existence, readability, non-empty content and group/other read bits. It prints no key value. See [keys](keys.md#key-files).

The daemon's `load_keys` reads the file as UTF-8, trims it and requires at least 8 bytes. It does not inspect the file's mode at startup or reload. A process with the Reiki user's privileges can read the same file. Treat the host and that account as part of the trust boundary.

## Plugins are trusted code

A plugin runs in the Reiki process without a sandbox. It receives request headers including the client token, the path and the body before an upstream key is attached. It may rewrite the body, set headers on a `passthrough` route, or reject a request. It cannot receive an upstream key from a hook, but it runs with Reiki's privileges. See [plugins](plugins.md#what-a-plugin-can-see-and-do).

A panic in a hook is caught and logged so the process keeps running; earlier changes can remain and plugin state can be half-updated. A panic in `on_request` is a skip, decided by `on_error`. That is error handling, not isolation.

## Opaque tunnels

`[tunnel]` copies bytes for allowed CONNECT destinations without reading them. Plugins, detectors, the scrubber and the request log see only intercepted hosts in `[tunnel.intercept]` or keyed API routes. Plain CONNECT has no token, so restrict who can reach the listener.

## Encodings have a fixed list

The response scrubber generates a finite set of forms for each key. It covers the raw value, one ASCII whitespace or control byte at each boundary and between every character, common JSON escapes, percent encodings, standard and URL-safe base64 with or without padding, and base64 at byte offsets 1 and 2. It also drops a failure-ring snippet that still contains a key after ASCII whitespace and control bytes are removed. See [security](security.md#clients-and-keys).

It does not decode arbitrary encodings. Base64 inside a larger value more than two bytes from the start is outside the list. On the outbound side, `placeholders` scans raw UTF-8 text: base64 `image` and `document` blocks and thinking signatures pass untouched. A model that paraphrases a placeholder instead of repeating it defeats restoration.

## What would need another control

| Goal | Reiki page alone is not enough |
| --- | --- |
| Prove no client call went to a provider directly | Enforce network egress, client config and process boundaries outside Reiki |
| Keep keys from every process on the host | Use a separate account, host or secret store; file modes alone do not separate same-user processes |
| Scan every outbound byte | Restrict destinations to intercepted hosts and read-only request paths; opaque tunnels and unscanned block types remain |
| Trust a third-party plugin | Review and build the plugin yourself |

> **Unverified here.** This repository has no OpenCode or Claude Code source, no 2.3.x client config examples, and no client tests. The bypass rows describe traffic that does not reach Reiki; they do not assert which client setting causes it.
