# PNG encoder plugin

This registered-Wasm plugin implements `png.encode(BufferView, i64) -> Bytes`
with the same pinned `png` crate and deterministic settings as the original
host implementation. It has no WASI or other imports. Its input and output
allocations are reclaimed when the per-invocation Wasm instance is dropped.

The checked-in artifact was built with Rust 1.98.1. Rebuild it from this
directory, using the committed guest lockfile:

```powershell
cargo build --target wasm32-unknown-unknown --release `
  --target-dir ../../build/png-encode-target --locked
Copy-Item `
  ../../build/png-encode-target/wasm32-unknown-unknown/release/histima_png_encode_plugin.wasm `
  png_encode.wasm
```

`.cargo/config.toml` exports linear memory and declares the same 64 MiB maximum
that the host independently enforces.
