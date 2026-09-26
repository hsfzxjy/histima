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
  aliasable `ImageView`.
- immutable semantic lineage DAGs kept entirely outside native payloads.

The intentionally small executable subset supports outer bindings, scalar and
string literals, immutable lists/records, `asset(...)`, arithmetic, scalar
transform calls, and pipelines. `tima run` compiles checked scalar transforms
to a temporary DLL with LLVM/Clang and invokes them through generated C ABI
adapters; the IR interpreter remains available as a reference execution path.
Transform bodies currently contain exactly one typed `return` expression.
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
returns the derivation as an inspectable outer value. External-observation nodes
are represented for future runtime-mediated capabilities, but no ambient access
is introduced by this milestone.

Try the vertical slice:

```text
cargo run -p tima -- check examples/first.tima
cargo run -p tima -- run examples/first.tima
cargo run -p tima -- emit-c examples/first.tima
```

The language and runtime contract is maintained in the local design documents
described by `AGENTS.md`.
