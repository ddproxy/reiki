# Quickstart

This walkthrough starts one Reiki instance with one upstream, one key, one stream and an admin listener. It ends with three proofs: `check` accepts the config, a wrong stream token gets `401`, and the admin commands answer from the live process.

The `example` upstream is a placeholder. Use a real `base_url` and model from [providers](providers.md) before you expect a successful upstream response.

## 1. Prepare the host

Install the binary and create the directories from the [deploy layout](deploy.md):

```bash
useradd --system --home /nonexistent --shell /usr/sbin/nologin reiki
install -d -m 0750 -o reiki -g reiki /etc/reiki
install -d -m 0700 -o reiki -g reiki /etc/reiki/keys
reiki --version
```

Create one key file and both tokens. Config takes hashes; CLI calls take raw tokens.

```bash
umask 077
printf '%s' "$PROVIDER_KEY" > /etc/reiki/keys/example-a
chmod 0600 /etc/reiki/keys/example-a

ADMIN_TOKEN=$(openssl rand -hex 32)
CLIENT_TOKEN=$(openssl rand -hex 32)
printf '%s' "$ADMIN_TOKEN" > /etc/reiki/admin.token
chmod 0600 /etc/reiki/admin.token

printf '%s' "$CLIENT_TOKEN" | reiki hash-token
printf '%s' "$ADMIN_TOKEN"  | reiki hash-token
```

Put each printed hash in the config below. Keep the raw tokens for the client and the admin CLI calls.

> Key files are mode `0600`. With the default `strict_key_permissions = true`, startup and reload refuse a file that group or other users can read or write; `false` warns instead. `reiki keys --check` reports such a file as a warning and exits `1`. Same-user processes can still read the file. See [keys](keys.md) and [security](security.md).

## 2. Write the config

`/etc/reiki/reiki.toml`:

```toml
listen = "127.0.0.1:4097"

[admin]
listen = "127.0.0.1:4098"
token_sha256 = "<64 hex from the admin token>"

[upstream.example]
base_url = "https://api.example.com/v1"

[keys.example-a]
upstream = "example"
file = "/etc/reiki/keys/example-a"

[streams.client-a]
token_sha256 = "<64 hex from the client token>"
chain = ["example-a"]
```

The client listener is plain HTTP on loopback. The admin listener also needs a bearer token and has no TLS option; put TLS in front if it must be reached off-host. See [deploy](deploy.md).

## 3. Validate and start

```bash
reiki --config /etc/reiki/reiki.toml check
reiki --config /etc/reiki/reiki.toml keys --check
reiki --config /etc/reiki/reiki.toml
```

Expected `check` output for this config, with peers off:

```text
caps: client 1024 (256 per address), admin 64 (16 per address); loopback exempt from the per-address caps
peers: disabled
ok: 1 upstream(s), 1 key(s), 1 stream(s), listening on 127.0.0.1:4097, admin on 127.0.0.1:4098
```

`keys --check` prints `example-a: ok` when the file is clean; a warning also exits `1`. Start in the foreground and use `SIGTERM` to stop it before installing a service unit. See [deploy](deploy.md).

## 4. Check both listeners

From another terminal:

```bash
curl -s http://127.0.0.1:4097/__reiki/healthz
export REIKI_ADMIN_TOKEN="$(cat /etc/reiki/admin.token)"
curl -s -H "Authorization: Bearer $REIKI_ADMIN_TOKEN" \
  http://127.0.0.1:4098/v1/status
```

The health probe needs no token and answers `ok` while Reiki is up. `/v1/status` returns key health, stream chains and reload state; it carries key names, never key values. See [monitoring](monitoring.md).

## 5. Negative test: a wrong stream token

```bash
curl -s -o /tmp/reiki-401-body -w '%{http_code}\n' \
  http://127.0.0.1:4097/example/chat/completions \
  -H 'Authorization: Bearer wrong-client-token' \
  -H 'Content-Type: application/json' \
  -d '{"model":"example-model","messages":[{"role":"user","content":"ping"}]}'
cat /tmp/reiki-401-body
```

Expected output:

```text
401
{"ok":false,"error":{"code":"unauthorized","message":"unknown token"}}
```

A request with no token gets the same status with `missing token`. After ten bad tokens from one address in a minute, the next request gets `429` with `Retry-After`. The test proves the request reached the client listener and was checked against a stream hash; it does not prove the upstream works. See [failover](failover.md) for the full client-error table.

## 6. Read live state

```bash
REIKI_ADMIN_TOKEN="$ADMIN_TOKEN" reiki keys
REIKI_ADMIN_TOKEN="$ADMIN_TOKEN" reiki failures --limit 5
```

Fresh output before any upstream attempt:

```text
KEY        UPSTREAM  STATE  COOLDOWN  LAST  STREAMS
example-a  example   ok     -         -     client-a
```

```text
no failures
```

After a successful upstream attempt, `LAST` holds that status code. After a classified failure, `reiki failures` prints a row from the in-memory ring; the ring starts empty on restart and is never written to disk. See [keys](keys.md) and [cli](cli.md).

## Where to go next

- Point a client at Reiki: [clients](clients.md).
- Read the trust boundary: [security](security.md).
- Diagnose a symptom: [troubleshooting](troubleshooting.md).
- Browse every setting: [configuration](configuration.md).
