# Histima / Tima

This repository currently contains the first vertical slice of **Tima**, the
embedded language for Histima asset pipelines.

The Rust workspace has one `tima` crate with:

- one lexer, parser, expression arena, source-span model, and diagnostic model
  shared by outer code and inner `transform` declarations;
- an immutable outer value model and small interpreter;
- static checking and backend-neutral typed control-flow IR for transforms;
- an inspectable generated-C backend, LLVM/Clang artifact compilation, and
  dynamic loading behind IR/backend boundaries;
- an explicit native ABI distinction between owned `Image` and read-only,
  aliasable `ImageView`;
- immutable semantic lineage DAGs kept entirely outside native payloads;
- separate native-artifact and transform-result caches keyed by semantic IDs;
- host-mediated environment observations shared by the reference interpreter
  and generated-C runtime ABI.

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
exactly one loop-invariant `u8` assignment; semantic lowering canonicalizes it
to the same backend-neutral `ImageFill` operation as `image_fill`, so equivalent
source forms share Transform identity. Reading the byte, nested/general loop
bodies, and arbitrary indexed mutation are the next language steps.

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

`replay(value)` now resolves the recorded transform by semantic identity,
validates recorded external observations, restores exact scalar arguments and
CAS-backed materialized arguments, and either reuses the recorded Recipe ID or
re-executes it. Re-execution must reproduce both the Recipe ID and expected
Content ID; arbitrary ancestor substitution remains intentionally unsupported.

Try the vertical slice:

```text
cargo run -p tima -- check examples/first.tima
cargo run -p tima -- run examples/first.tima
cargo run -p tima -- emit-c examples/first.tima
```

The language and runtime contract is maintained in the local design documents
described by `AGENTS.md`.
