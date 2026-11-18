# Hub

The hub is a planned design for coordinating multiple Reiki instances. In this release, its tables are parsed and validated when the binary includes the `hub` cargo feature (not in the default set), but Reiki does not open a hub listener, build a hub client, or contact a hub.

A binary built without the feature refuses a config that contains `[hubs]`, `[hub]`, or `[accounts]`. With the feature on, `reiki check` accepts valid hub tables, while runtime behavior is unchanged. Without these tables nothing changes.

## Tables

```toml
[hubs.main]                              # a remote hub this instance reaches as an edge
url = "https://hub.example.net:4443"             # https, or http on a loopback address
token_file = "/etc/reiki/hub/edge.token" # raw token the hub issued; mode 0400
ca_file = "/etc/reiki/hub/ca.pem"        # optional PEM CA certificates for this hub
accounts = ["acct-a"]                   # account labels routed through this hub
timeout_secs = 300                       # default 300, 1 to 3600

[hub]                                    # this instance serves edges
listen = "127.0.0.1:4443"                # loopback only: no TLS yet

[hub.edges.peer-a]
token_sha256 = "<64 hex>"                # SHA-256 of the token this edge presents
allow_from = ["acct-a"]                 # account labels this edge may send for
max_concurrent = 8                       # 0 means unlimited
max_tokens_per_day = 2000000             # 0 means unlimited

[accounts.acct-a]                       # one account: its key(s) on one upstream
upstream = "zen"
keys = ["zen-a"]                         # key names from [keys.*], tried in order
shared = false

[accounts.acct-a.limits]                # absent means no shaping
preset = "zen-free"                      # one of zen-free, openrouter-free
rpm = 20                                 # 0 means unlimited, for every field below
tpm = 60000
concurrent = 2
daily_requests = 200
daily_tokens = 0
```

## Rules

- Hub, edge and account names: lowercase letters, digits and `-`, at most 63 characters.
- A key belongs to one account only, and an account needs at least one key.
- An edge's `allow_from` and a hub's `accounts` name existing accounts.
- A hub token must differ from the admin token and every stream token; the same secret in two places would let a weaker holder act with a stronger one's rights.
- A hub may not list its own listener, which would loop back to itself.
