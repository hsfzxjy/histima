# PNG decoder plugin

This registered-Wasm plugin implements `png.decode(BytesView) -> Buffer` with
the same pinned `png` crate and RGBA8 normalization rules as the original host
implementation. Animated PNG is rejected explicitly. The module has no WASI
or other imports, and its allocations are reclaimed with the per-invocation
Wasm instance.

The checked-in artifact was built with Rust 1.98.1. Rebuild it from this
directory, using the committed guest lockfile:

```powershell
cargo build --target wasm32-unknown-unknown --release `
  --target-dir ../../build/png-decode-target --locked
Copy-Item `
  ../../build/png-decode-target/wasm32-unknown-unknown/release/histima_png_decode_plugin.wasm `
  png_decode.wasm
```

`.cargo/config.toml` exports linear memory and declares the same 64 MiB maximum
that the host independently enforces.
