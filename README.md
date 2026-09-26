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
transform calls, and pipelines. `tima run` compiles checked scalar transforms
to a temporary DLL with LLVM/Clang and invokes them through generated C ABI
adapters; the IR interpreter remains available as a reference execution path.
Transform bodies may contain inferred immutable local bindings followed by one
typed `return` expression; shadowing, rebinding, branches, and local mutation
remain intentionally unsupported.
Host-provided immutable images can cross the native boundary: `Image` acquires
unique mutable storage by transfer or detach, multiple owned arguments cannot
alias, `ImageView` shares storage zero-copy, and returned descriptors are frozen
only when they reference storage retained by the invocation. Inner-to-inner
calls with owned images remain disabled until the IR has explicit move/detach
lowering. Native `i64` arithmetic is also held back until its overflow and
division-error semantics are specified.

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
