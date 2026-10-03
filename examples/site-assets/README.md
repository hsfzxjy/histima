# Website asset workflow

This example exercises Histima as a small website-asset build rather than as a
language fixture. Two imported PPM sources are decoded into generic
`[height, width, 4]` Buffers, changed by the user-defined `darken` transform,
and encoded independently as PNG and WebP.

From this directory, create a disposable workspace and run the pipeline:

```text
cargo run --manifest-path ../../Cargo.toml -p histima -- init ../../build/site-assets-work
cargo run --manifest-path ../../Cargo.toml -p histima -- import --workspace ../../build/site-assets-work inputs --recursive
cargo run --manifest-path ../../Cargo.toml -p histima -- run ../../build/site-assets-work pipeline.tima --stock-intermediates --record hero_buffer --record hero_png --record hero_webp --record thumbnail_png --record thumbnail_webp --json
```

The last command prints all five explicitly selected Recipe and Content IDs in
one `records` array and eagerly stocks every reachable `Bytes`/`Buffer`
invocation after the successful run. Its `stocked_results` array includes the
decodes, user Buffer transforms, and encodes; imported source reads already
refer to durable source content rather than a transform result. Run it again
to observe reuse across the full transform graph, or select the optional
hybrid engine to exercise its interpreter fallback for the Buffer transform:

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

## Product pressure observed

- **CLI/product problem (fixed after the first run):** `run --record` initially
  stocked only one binding, forcing repeated executions. Repeating
  `--record <binding>` now stocks all selected outputs in one execution and
  reports them through the JSON `records` array.
- **Runtime problem (addressed for this workload):** selected immutable Buffers
  can be stored, validated, replayed, and reused across processes. The explicit
  `--stock-intermediates` policy now stocks recordable invocation results
  reachable from successful program outputs without retaining failed or
  discarded work.
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

The selected immediate follow-ups were the portable-locator fix and repeated
`--record` for multi-output stocking. The other items remain evidence for later
prioritization rather than invitations to broaden Tima now.
