# Observe, then enforce

Use this order when adding outbound controls: mask known values, watch detections without blocking, check samples offline, preview the config change, then reject. Each step has a separate proof so a false positive stops before it reaches users.

Plugins are compiled into the binary. An unknown `[plugins.NAME]` is a startup error, and plugin order comes from the build, not the config file. The default build registers `phi` before `placeholders` before `requestlog`. See [plugins](plugins.md#choose-and-order-plugins-at-build-time).

## 1. Mask known values first

```toml
[plugins.placeholders]
terms = ["Project Falcon", "Acme Corp"]
builtin = true
rehydrate = true
on_error = "reject"
```

`builtin` masks credential shapes; `terms` masks exact strings, case-insensitively. `rehydrate` restores placeholders in responses. `on_error = "reject"` is explicit here; it is also the default for `placeholders`, which fails closed. See [placeholders](placeholders.md#configure) and [plugins](plugins.md#use-a-plugin).

> **Fail-open case.** `on_error = "pass_through"` lets a request through when `placeholders` reports it could not mask the body. Use it only where unmasked egress is acceptable.

With a `[plugins.requestlog]` table, no body setting is needed: `body = "summary"` is the default and records counts, not prompt text. `capture_bodies` also defaults to `false`; recording body text requires `capture_bodies = true` together with `body = "full"`, and logs a warning at startup. See [request log](requestlog.md#configure).

## 2. Report PHI without blocking

```toml
[plugins.phi]
mode = "report"
kinds = ["ssn", "card", "routing", "account", "dob", "medical_id", "npi", "icd10", "column"]
on_error = "reject"
```

`report` logs the finding and lets the request through. Add `field`, `email` and `phone` only after checking samples: they match fields, commit metadata and source code more often than the default kinds. Restrict `upstreams` when only some routes need the check. See [PHI and NPPI](phi.md#configure).

The request log gets a `finding` record with `action: "reported"`. It names the kind and count, never the matched text. By default it also records a body summary, not the body. With `capture_bodies = true` and `body = "full"`, it still records the original identifiers: `phi` reports them and lets the request through, and the request log does not mask detected identifiers. Leave body capture off, or set `body = "none"`, during this phase. See [request log](requestlog.md#findings-from-detection-plugins).

> **Not covered.** `phi` reads only the request body as text. Headers, the query string and responses are not checked. A body with a `Content-Encoding` other than `identity`, or one that is not valid UTF-8, is logged as unchecked in `report` mode.

## 3. Scan samples offline

```bash
reiki scan --plugin phi --kinds ssn,card samples/*.txt
cat prompt.json | reiki scan --plugin phi
reiki scan --plugin phi --all --json notes.txt
```

Use `--kinds` to match the `kinds` list in the config, and `--all` to include kinds that are off by default. A finding prints as `FILE:LINE:COL PLUGIN KIND`; matched text is never printed. A summary goes to stderr. See [cli](cli.md#scan).

| Exit | Meaning |
| --- | --- |
| `0` | No findings. |
| `1` | At least one finding. |
| `2` | Unknown plugin or kind, unreadable file, or usage error. |

`scan` reads bytes as UTF-8 with lossy conversion. The proxy plugin refuses a request body that is not valid UTF-8 instead, so sample results are a guide, not a perfect replay. See [plugins](plugins.md#tune-a-detector-with-reiki-scan).

## 4. Preview the change

Change the config, then ask the running instance what it would do:

```bash
reiki --config /etc/reiki/reiki.toml reload --dry-run \
  --token-file /etc/reiki/admin.token
```

Expected answer shape:

```json
{"ok":true,"dry_run":true,"generation":3,"keys":1,"streams":1,"changed":["plugins phi"],"restart_needed":[]}
```

`dry_run` applies nothing. `changed` lists what differs; `restart_needed` lists settings that wait for a restart. A config that fails validation keeps the running state. A bad `dry_run` query on the admin API fails closed with `400`. See [monitoring](monitoring.md#post-v1reload) and [upgrades](upgrades.md#reload-vs-restart).

Plugin table changes rebuild the plugin. `streams` alone applies live without a rebuild. Plugin order is build-time only; it cannot be changed by reload. See [plugins](plugins.md#use-a-plugin).

## 5. Switch to block

```toml
[plugins.phi]
mode = "block"
kinds = ["ssn", "card", "routing", "account", "dob", "medical_id", "npi", "icd10", "column"]
on_error = "reject"
```

Preview, then apply:

```bash
reiki --config /etc/reiki/reiki.toml reload --dry-run \
  --token-file /etc/reiki/admin.token
reiki --config /etc/reiki/reiki.toml reload \
  --token-file /etc/reiki/admin.token
```

| Result | Client sees | Evidence |
| --- | --- | --- |
| Identifier found | `422` | `finding` record with `action: "blocked"`; no request or response record for that request |
| Non-UTF-8 body or non-`identity` `Content-Encoding` | `415` | No finding; the body could not be read |
| Plugin skip under `on_error = "reject"` | `503` | `reiki_plugin_rejections_total{plugin="phi",...}` increases |
| Clean request | The upstream's response | `reiki_requests_total{...}` increases |

The rejection happens before a key is used, so no upstream call is made. See [PHI and NPPI](phi.md#behavior).

Proof from the admin listener, with `[plugins.metrics]` enabled:

```bash
curl -s -H "Authorization: Bearer $REIKI_ADMIN_TOKEN" \
  http://127.0.0.1:4098/v1/metrics
```

Look for `reiki_findings_total{...,action="blocked"}` and `reiki_plugin_rejections_total{plugin="phi",...}`. Neither series carries matched identifier text. See [monitoring](monitoring.md#get-v1metrics).

## 6. Roll back

Set `mode = "report"` again, or remove the `[plugins.phi]` table, then preview and reload. A plugin table removed from the config stops rebuilding on the next reload; a binary rebuilt without the plugin rejects the table at startup. Keep the previous config until the proof checks pass.

> **Observation is not enforcement.** `report` mode, a narrow `upstreams` list, and `phi`'s body-only scope all leave traffic unblocked by design. See [what Reiki does not protect against](limits.md).
