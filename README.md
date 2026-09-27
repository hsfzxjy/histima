# Histima / Tima

This repository contains the first vertical slice of **Tima**, the embedded
language for Histima asset pipelines, and the initial durable **Histima** host
foundation.

The `tima` crate provides:

- one lexer, parser, expression arena, source-span model, and diagnostic model
  shared by outer code and inner `transform` declarations;
- an immutable outer value model, including encoded byte values, and a small
  interpreter;
- static checking and backend-neutral typed control-flow IR for transforms;
- an inspectable generated-C backend, LLVM/Clang artifact compilation, and
  dynamic loading behind IR/backend boundaries;
- an explicit native ABI distinction between owned `Image` and read-only,
  aliasable `ImageView`;
- explicit image layout metadata for opaque byte rows and validated interleaved
  RGBA8 pixels;
- immutable semantic lineage DAGs kept entirely outside native payloads;
- separate native-artifact and transform-result caches keyed by semantic IDs;
- host-mediated asset and environment observations shared by the outer runtime,
  reference interpreter, and generated-C runtime ABI.

The separate `histima` crate now owns the beginning of the product boundary. A
configurable workspace combines a migration-managed SQLite catalog with a
filesystem content-addressed store. SQLite runs with foreign keys and WAL mode,
stores queryable content/source metadata, preserves immutable source versions,
and tracks the current observation for each locator. Imported payloads are
atomically published outside SQLite under their Tima Content IDs; repeat imports
deduplicate bytes, and every read revalidates stored content before returning
it. The workspace implements Tima's explicit asset-read capability and never
falls back to an ambient path that has not been imported.

The initial `histima` CLI exposes that storage boundary across processes:

```text
cargo run -p histima -- init build/my-workspace
cargo run -p histima -- import build/my-workspace examples/tiny.ppm
cargo run -p histima -- stats build/my-workspace
cargo run -p histima -- materialize build/my-workspace <content-id> output.ppm
```

Materialization verifies the stored Content ID, publishes through a temporary
file, and refuses to replace an existing destination. Tima identities use one
canonical durable text form: 64 lowercase hexadecimal characters.

The intentionally small executable subset supports outer bindings, scalar and
string literals, immutable lists/records, `asset(...)`, arithmetic, scalar
comparisons, transform calls, and pipelines. `tima run` compiles checked scalar
transforms to a temporary DLL with LLVM/Clang and invokes them through generated
C ABI adapters; the IR interpreter remains available as a reference execution
path.
Transform bodies may contain inferred immutable local bindings, typed returns,
and `if` statements with required `else` arms. Either branch may return early
or fall through to a continuation; branch-local bindings do not escape that
join. The typed IR represents control flow as explicit basic-block branches and
jumps consumed by both the reference interpreter and C backend. Merged branch
values, rebinding, and general loop bodies remain intentionally unsupported.
Host-provided immutable images can cross the native boundary: `Image` acquires
unique mutable storage by transfer or detach, multiple owned arguments cannot
alias, `ImageView` shares storage zero-copy, and returned descriptors are frozen
only when they reference storage retained by the invocation. Inner-to-inner
calls transfer owned `Image` arguments without returning through the outer
representation; passing the same owned value twice or using it after the call
is rejected. Native `i64` arithmetic is still held back until its overflow and
division-error semantics are specified.

The first concrete owned-image operation is the inner-only
`image_zero(image)`. It consumes an owned `Image`, zeros its byte storage in
place, and returns the same ownership under a new value; aliases to the consumed
value are rejected, including unsafe uses after branch joins. Both the reference
interpreter and generated-C backend implement the same typed IR operation, while
the outer input remains immutable because shared storage is detached at the
boundary. This is intentionally narrower than general field or buffer mutation.

The same ownership path now supports `image_fill(image, value)`, where `value`
is a native-safe `u8`. Outer integers cross a `u8` parameter only after a
`0..=255` range check; inner integer literals remain `i64`, and implicit numeric
conversions or `u8` arithmetic are intentionally deferred. `u8` equality and
ordering are statically checked and execute consistently in the reference and C
paths.

The first constrained loop surface is
`for byte in img.bytes { byte = value }` over an owned `Image`. The body must be
exactly one assignment producing `u8`. A loop-invariant assignment canonicalizes
to the same backend-neutral `ImageFill` operation as `image_fill`, so equivalent
source forms share Transform identity. A byte-dependent assignment lowers to a
structured `ImageByteMap` IR operation containing its typed scalar instruction
sequence; both the reference interpreter and generated C execute it once per
byte. General/nested loop statements, arbitrary indexing, and consuming other
owned values inside the loop remain deferred.

Images now carry a semantic format through the outer value, ownership boundary,
reference interpreter, and generated-C ABI. Existing `ImageValue::new` values
remain opaque byte rows; `ImageValue::new_rgba8` validates four interleaved
8-bit channels per pixel and row stride. Format participates in content identity
and is revalidated when a native result is frozen. Byte loops work with either
layout.

The first RGBA8 pixel surface supports the target-shaped loop
`for p in img.pixels { p.r *= factor }` over an owned `Image`. A loop may scale
each of `r`, `g`, `b`, and `a` at most once by an `f32` expression evaluated
before iteration. Scaling operates on stored channel bytes, truncates fractional
results, saturates above 255, and maps non-positive or NaN results to zero.
Unmentioned channels and row padding remain unchanged. Both execution engines
reject opaque-byte images with a source-spanned format diagnostic. General
channel expressions, replacement assignment, and a general pixel value type
remain intentionally deferred.

The first Histima-facing codec path is deliberately small but complete:
`asset(...) | decode.ppm | darken(0.5) | encode.ppm`. `decode.ppm` accepts
ASCII P3 data supplied through an explicit host asset capability and produces
an RGBA8 image; `encode.ppm` produces deterministic immutable P3 bytes. These
versioned host transforms participate in semantic lineage, result caching, and
replay, but remain outside typed inner IR and the generated-C artifact cache.
The CLI explicitly supplies local-file access; the library has no ambient
filesystem fallback.

Encoded bytes can be persisted with the outer sink
`bytes | save("output.ppm")`. Saving requires an explicit host asset-output
capability and returns the same immutable value with the same derivation
lineage. It is an execution effect rather than a transform: it is never
result-cached, and replay reconstructs the derived bytes without unexpectedly
repeating the write. The CLI maps this capability to a local-file write.

Every checked transform also receives a stable semantic identity derived from
canonical typed IR and referenced transform identities. Source formatting,
comments, local names, declaration order, backend, and target do not affect it.
Content, invocation recipe, observed dependency, and native artifact identities
use separate hash domains; artifact identity additionally includes the actual
backend, Clang version, target, optimization mode, and native ABI version.
Lazy `asset(...)` values now begin with source lineage, and every outer-to-inner
transform call records a stable invocation recipe, semantic argument snapshots,
and ancestor edges without retaining owned native storage. `trace(value)`
returns the derivation as an inspectable outer value.

The first tracked capability is the literal-key inner call
`environment_i64("NAME")`. A Histima host must explicitly implement
`RuntimeCapabilities`; Tima never falls back to ambient process state. The
reference interpreter and generated C both parse the supplied bytes as an
`i64`, record the raw bytes as an external observation, and include that
dependency in Recipe identity. Different observed bytes therefore produce
different recipes, while replay validates the recorded observation before
cache reuse or re-execution.

Native C bundles are cached persistently under `build/cache` using the ordered
Artifact IDs of their transforms and are validated against the generated source
and compiled-library Content ID before reuse. Transform results use a separate
Recipe-ID index over an immutable content-addressed store. Cache hits reconstruct
lineage from the current semantic invocation rather than recording cache
execution history; conflicting content for one recipe is rejected as a
reproducibility failure.

`replay(value)` now resolves recorded inner and host transforms by semantic
identity, recursively validates source assets and recorded external
observations before consulting descendant result caches, restores exact scalar
arguments and CAS-backed materialized arguments, and either reuses the recorded
Recipe ID or re-executes it. Re-execution must reproduce both the Recipe ID and
expected Content ID; arbitrary ancestor substitution remains intentionally
unsupported.

Try the vertical slice:

```text
cargo run -p tima -- check examples/first.tima
cargo run -p tima -- check examples/darken.tima
cargo run -p tima -- run examples/first.tima
cargo run -p tima -- run examples/image_pipeline.tima
cargo run -p tima -- emit-c examples/first.tima
```

The language and runtime contract is maintained in the local design documents
described by `AGENTS.md`.
