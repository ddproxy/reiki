# Running Reiki as a service

Reiki runs in the foreground and logs to stderr, so a process manager such as systemd supervises it. This page covers how it stops, reloads and reports readiness, and an example unit.

## Signals

| Signal | Effect |
| --- | --- |
| `SIGTERM`, `SIGINT` | Clean stop: every listener stops accepting at once, idle keep-alive connections close, requests in flight get `[service] shutdown_timeout_secs` (default 25) to finish, then the request log and the state file are written and Reiki exits 0. |
| a second `SIGTERM` or `SIGINT` | Exits at once with status `128 + signal` (143 or 130). |
| `SIGHUP` | Reloads the config, keys and log file, like `POST /reload`. See [troubleshooting](troubleshooting.md#reload). |

A streamed response that is still running when the drain time ends is cut. Open tunnel connections get the same drain time. Keep the unit's `TimeoutStopSec` above `shutdown_timeout_secs`.

Exit status: 0 after a clean stop, 1 for a config or startup failure (a bad config, a port that cannot be bound), 143 or 130 after a second stop signal.

## systemd readiness and watchdog

When started with `Type=notify`, Reiki sends `READY=1` once every listener is bound and `STOPPING=1` when a stop begins. With `WatchdogSec=` set it sends `WATCHDOG=1` at half the period while the async runtime is still running tasks; a wedged runtime stops the pings and systemd restarts the service. Without `NOTIFY_SOCKET` all of this does nothing, so the same binary runs under any manager, or none.

## Open files

Each client connection and each upstream connection uses a file descriptor, and the client listener alone admits 1024 connections, the same as systemd's default soft limit. Set `LimitNOFILE=65536`. Reiki logs a warning at start when the soft limit is below 4096.

## Example unit

```ini
[Unit]
Description=Reiki API key failover proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
User=reiki
Group=reiki
ExecStart=/usr/local/bin/reiki --config /etc/reiki/reiki.toml
ExecReload=/bin/kill -HUP $MAINPID
Restart=always
RestartSec=3
TimeoutStopSec=40
WatchdogSec=60
LimitNOFILE=65536
StateDirectory=reiki
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictNamespaces=true
RestrictRealtime=true
LockPersonality=true
UMask=0077

[Install]
WantedBy=multi-user.target
```

Add `ReadWritePaths=` for a log or request-log directory outside `StateDirectory`. A forwarder command (see [providers](providers.md)) inherits these limits; check `systemd-analyze security reiki` after adding `SystemCallFilter=` or `MemoryDenyWriteExecute=`, which a JavaScript runtime used as a forwarder may not tolerate.
