# Tool policy

The `toolpolicy` plugin inspects tool calls in model responses before the client acts on them. It understands OpenAI Chat (`/chat/completions`) and Anthropic Messages (`/messages`), both whole JSON responses and server-sent event streams. It is off until `[plugins.toolpolicy]` exists; a stream can limit it further with the host's `streams` key.

This is a response-side guard, not a sandbox. It can tell a harness "this call was blocked"; it cannot control what the harness does with that text, nor stop a tool that the harness runs outside the proxied conversation.

## What it checks

The plugin reads the request to learn two things: the tools the harness declared, and the hosts the user or system actually mentioned. `tool_result` blocks and assistant prose never become provenance, so a tool's own output cannot approve its next target.

| Rule id | Severity | What matched |
| --- | --- | --- |
| `undeclared_tool` | high | The response calls a tool that the request did not declare (case-insensitive name match). |
| `unapproved_host` | high | A network-looking call names a host that is neither in `allow_hosts` nor in user/system provenance. |
| `proc_environ` | high | A path under `/proc/<pid>/environ` or `/proc/self/environ`. |
| `container_escape` | high | `unshare`, `nsenter`, `chroot` or `mount --bind`. |
| `proxy_bypass` | medium | Proxy variables, `--noproxy`, `*_BASE_URL` overrides or `unset` of proxy settings. |
| `credential_file` | high | Known credential paths such as `~/.ssh`, `.git-credentials`, `.npmrc`, `.aws/credentials`, `.password-store` or `/etc/reiki`. |
| `self_protection` | high | Text that would stop, kill, reload or reconfigure Reiki or a protected helper tool. |

Arguments are checked as written and again after shell quote removal, so `c''url` and `uns\\hare` are not hidden by splitting. Hosts are read from URLs, bare IPv4/IPv6 addresses, bracketed IPv6 with a port, and host-shaped words after a network command. An allow-list entry is a suffix match: `example.org` allows `docs.example.org` and `example.org` but not `evil-example.org`. The default allow-list is empty.

A call is flagged when any rule matches. In `report` mode the response is unchanged. In `block` mode a finding at or above `block_severity` replaces the call with a plain-text notice; other findings are still reported and journaled. The flagged tool name and arguments are never forwarded as a tool call.

## Configure

```toml
[plugins.toolpolicy]
mode = "report"              # "report" (default) or "block"
block_severity = "high"      # "high" (default) or "medium"
allow_hosts = []             # suffix matches; empty by default
check_declared = true
check_hosts = true
check_tripwires = true
journal = "/var/lib/reiki/toolpolicy-findings.jsonl"
journal_max_bytes = 1048576
upstreams = []               # empty means every upstream
# streams = ["agent"]        # host key; limits which streams run the plugin
```

Bad values fail at startup and name the key. `journal` is optional; without it the plugin still reports and blocks but writes no findings file. The plugin holds the journal and counters, so a change needs a restart; reload reports it as needing one rather than swapping the running instance.

`mode = "block"` makes the plugin fail closed: if a hook fails, the request is rejected with 503 under the default `on_error` policy. An explicit `on_error = "pass_through"` overrides that and is logged as a warning at startup.

## Streams and unparsed responses

Text deltas pass through as they arrive. Only tool-call fragments are held, until their arguments are complete; a call is then either released unchanged or replaced with a notice. Each call is capped at 64 KiB of arguments and each response at 64 calls. A larger call, an invalid JSON argument, a malformed event or an unparseable recognized request is "unparsed".

In `block` mode, unparsed means held calls are dropped and a notice is appended. Text already sent earlier in that stream cannot be retracted, so the notice is appended rather than spliced into past output. In `report` mode the bytes pass through and `reiki_toolpolicy_unparsed_total` increases. Traffic on other routes is not parsed and is not counted.

## Journal

Each flagged call writes one JSON line with `timestamp`, `stream`, `upstream`, `tool`, `rules`, `severity`, `action` and `arguments`. Arguments are cut to 2 KiB; credential-shaped substrings and values of sensitive fields such as `api_key` or `TOKEN=` become `[redacted]`. The file is created with mode `0600` and rotated to `<path>.1` when it would exceed `journal_max_bytes`.

## Metrics

The plugin serves its own admin path `GET /v1/toolpolicy/metrics`, with the same bearer token as the other admin routes. It exports `reiki_toolpolicy_calls_total{stream,upstream,action}` (`passed`, `reported` or `blocked`), `reiki_toolpolicy_findings_total{rule,severity}` and `reiki_toolpolicy_unparsed_total{stream,upstream}`. A finding is counted even when it is below `block_severity` and does not block.

## Observe, then enforce

1. Add `[plugins.toolpolicy]` with `mode = "report"` and configure `journal`.
2. Run a normal workload. Watch the journal and `/v1/toolpolicy/metrics` for false positives, especially undeclared tools caused by a harness that does not send a `tools` array.
3. Add only the hosts that are genuinely needed to `allow_hosts`. Treat a write-capable allowed host as a grant: this plugin checks the host, not the HTTP method or the file a tool writes. Keep write-capable destinations behind a separate network control or a narrow credential where possible.
4. Switch to `mode = "block"` and restart. Start with `block_severity = "high"`; lower it to `"medium"` only after proxy-bypass findings are understood.

## What it does not stop

- Shell escapes that do not match the listed tripwires, encoded payloads the decoder does not understand, or a tool that acts without emitting a structured call.
- A client that ignores the replacement notice and runs the original call from another channel.
- Hosts mentioned only in tool results or assistant text, or traffic on routes other than OpenAI Chat and Anthropic Messages.
- Writes to an allow-listed host, or exfiltration through a service that the user explicitly named in their own text.
- Bytes of a streamed answer that were already delivered before an unparsed condition was detected.
