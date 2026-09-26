# Histima / Tima

This repository currently contains the first vertical slice of **Tima**, the
embedded language for Histima asset pipelines.

The Rust workspace has one `tima` crate with:

- one lexer, parser, expression arena, source-span model, and diagnostic model
  shared by outer code and inner `transform` declarations;
- an immutable outer value model and small interpreter;
- static checking and backend-neutral typed control-flow IR for transforms;
- an inspectable generated-C backend behind an IR-to-artifact interface;
- an explicit native ABI distinction between owned `Image` and read-only,
  aliasable `ImageView`.

The intentionally small executable subset supports outer bindings, scalar and
string literals, immutable lists/records, `asset(...)`, arithmetic, scalar
transform calls, and pipelines. Transform bodies currently contain exactly one
typed `return` expression. `Image`/`ImageView` are represented and emitted in
the ABI, but the runtime rejects executing them until detach/view acquisition
and freeze-on-return are implemented.

Try the vertical slice:

```text
cargo run -p tima -- check examples/first.tima
cargo run -p tima -- run examples/first.tima
cargo run -p tima -- emit-c examples/first.tima
```

The language and runtime contract is maintained in the local design documents
described by `AGENTS.md`.

