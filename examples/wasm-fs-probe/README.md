# filesystem-probe reference plugin

A Phase 1 Wasm plugin that tries four filesystem operations through WASI:
open, truncate, rename and hard link. It returns `Pass` only when all four fail;
any success becomes a reject using the embedded `fs-capability-leak` template.
The test loads the committed component through the real host and checks for
`Pass`, proving no filesystem capability is available to the guest.

## Build

No `cargo-component` is needed. Install the `wasm32-wasip1` Rust target, put
`wasm-tools` on `PATH`, and use `perl` to inject the manifest section:

```sh
./rebuild.sh
```

The preview1 adapter in `rebuild.sh` deliberately returns
`errno_notcapable` for `path_open`, `path_link` and `path_rename`, and reports
no preopens. This matches the host's minimal WASI p2 context, which has no
preopened directories. Pass an output path to write elsewhere:
`./rebuild.sh /tmp/fs-probe.wasm`.
