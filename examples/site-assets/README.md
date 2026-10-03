# Website asset workflow

This example exercises Histima as a small website-asset build rather than as a
language fixture. Two imported PPM sources are decoded into generic
`[height, width, 4]` Buffers, changed by the user-defined `darken` transform,
and encoded independently as PNG and WebP.

From this directory, create a disposable workspace and run the pipeline:

```text
cargo run --manifest-path ../../Cargo.toml -p histima -- init ../../build/site-assets-work
cargo run --manifest-path ../../Cargo.toml -p histima -- import --workspace ../../build/site-assets-work inputs --recursive
cargo run --manifest-path ../../Cargo.toml -p histima -- run ../../build/site-assets-work pipeline.tima --record hero_webp --json
```

The last command prints the recorded Recipe and Content IDs. Run it again to
observe result-cache reuse, or select the optional hybrid engine to exercise
its interpreter fallback for the Buffer transform:

```text
cargo run --manifest-path ../../Cargo.toml -p histima -- run ../../build/site-assets-work pipeline.tima --engine hybrid-aot --record hero_webp --json
```

Use the printed IDs to inspect, replay, render, and materialize the derivation:

```text
cargo run --manifest-path ../../Cargo.toml -p histima -- trace ../../build/site-assets-work <recipe-id>
cargo run --manifest-path ../../Cargo.toml -p histima -- expression ../../build/site-assets-work <recipe-id>
cargo run --manifest-path ../../Cargo.toml -p histima -- replay ../../build/site-assets-work pipeline.tima <recipe-id>
mkdir ../../build/site-assets-dist
cargo run --manifest-path ../../Cargo.toml -p histima -- materialize ../../build/site-assets-work <content-id> ../../build/site-assets-dist/hero.webp
```

Repeat `run --record` with `hero_png`, `thumbnail_png`, and `thumbnail_webp` to
stock and materialize the other outputs.

## Product pressure observed

- **CLI/product problem:** `run --record` stocks only one binding, so a
  multi-output asset build needs repeated executions and manual collection of
  IDs. A future batch-record/materialize surface would remove this friction.
- **Runtime problem:** across processes, only explicitly stocked outputs are
  durable result-cache hits; unrecorded sibling outputs and intermediates are
  recomputed. Eagerly stocking a successful build graph needs a deliberate
  storage-policy design.
- **Performance problem:** the indexed Buffer transform intentionally falls
  back to the interpreter under `hybrid-aot`; the current CLI reports the
  engine but does not explain the per-transform fallback decision.
- **Missing transform/library problem:** there is no general resize transform,
  so this workload uses separate hero and thumbnail sources rather than
  deriving multiple sizes from one Buffer.
- **Language problem:** the constrained byte loop is sufficient, but expressing
  "preserve every fourth alpha byte" through storage-offset arithmetic is
  low-level. This is not yet enough evidence for general indexing or image
  types.
- **CLI/product problem (fixed here):** recursive import previously leaked
  Windows separators into locators, making portable `/` locators in Tima fail
  to resolve. Imported filesystem locators are now slash-normalized on
  Windows.

The selected immediate follow-up is only the portable-locator fix. The other
items remain evidence for later prioritization rather than invitations to
broaden Tima now.
