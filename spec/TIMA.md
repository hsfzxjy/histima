# Tima Language and Runtime Specification

Status: **implemented v0 contract**

This document specifies the Tima behavior implemented by this repository. It
is the public language/runtime contract for the current vertical slice, not a
roadmap for a general-purpose language.

The words **must**, **must not**, **should**, and **may** are normative. A
feature described as deferred or unsupported is not part of the contract even
if a frontend stage happens to accept some form of it.

Language changes must update this document in the same change. Tests and the
implementation are the conformance evidence. `agents/LANGPLAN.md` records
design rationale and future directions; its unimplemented examples are not
part of this specification.

## 1. Purpose and architecture

Tima is Histima's embedded language for asset-transformation pipelines. It has
one source language with two execution strata:

- **outer code** is dynamic, immutable, and orchestration-oriented;
- **inner code**, introduced by `transform`, is statically checked and runs as
  WebAssembly through the host runtime.

Both strata use the same lexer, parser, expression syntax tree, source spans,
and diagnostic model. Semantic context determines which syntax and values are
legal in each stratum.

The implemented compilation path is:

```text
Tima source
    -> shared lexer/parser/AST
    -> inner semantic checking
    -> backend-neutral typed Tima IR
    -> WebAssembly memory64 module
    -> Wasmtime
```

WebAssembly is the sole inner-language backend. It is a backend output, not
Tima's IR. The typed IR remains backend-neutral so language semantics are not
defined by Wasm instructions or by Wasmtime's internal Cranelift compiler.
Portable `.wasm` modules are the durable compiled artifacts; host machine-code
caches are non-semantic and may be discarded at any time.

## 2. Core invariants

These rules take precedence over optimization choices.

1. Outer composite values are immutable. An outer value visible through one
   binding must not be changed by invoking a transform through another binding.
2. An inner `Image` is uniquely owned and mutable for the duration of a
   transform invocation.
3. An inner `ImageView` is read-only and may alias other views.
4. Two owned inner parameters must not alias mutable storage, even if the
   caller supplies the same outer image twice.
5. An inner value returned to outer code is frozen before it becomes visible.
   Freezing should transfer ownership when it is safe to do so.
6. Outer and inner values do not share a universal object representation.
   Dynamic tags, lineage, and cache metadata never enter hot inner payloads.
7. Derivation lineage is semantic data. Backend selection, cache hits,
   timestamps, and other execution history must not change it.

The conceptual boundary is:

```text
OuterValue
    -> validate / lower / acquire or detach
WasmValue / linear-memory offset
    -> execute transform
WasmValue / linear-memory offset
    -> validate / freeze / box / attach lineage
OuterValue
```

## 3. Source text and lexical grammar

Source is UTF-8. Spans are half-open byte ranges and diagnostics map them to
one-based line and column locations.

### 3.1 Identifiers and keywords

An identifier starts with an ASCII letter or `_` and continues with ASCII
letters, digits, or `_`.

The reserved keywords are:

```text
transform  return  if  else  for  in  true  false  null
```

Identifiers are case-sensitive. Type names are also case-sensitive.

### 3.2 Comments and whitespace

Spaces, tabs, and carriage returns separate tokens. A `//` comment extends to
the end of its line. Newlines are significant as item and statement
separators, except where the grammar explicitly accepts inline newlines.

### 3.3 Literals

The implemented literal forms are:

```tima
null
true
false
42
3.5
"cat.png"
[1, 2, 3]
{name: "cat", sizes: [1, 2]}
```

Integer tokens are unsigned decimal text parsed into `i64`; negative literals
are not currently available because unary negation is unsupported. A float
token requires digits on both sides of one decimal point. Exponents and type
suffixes are unsupported.

Strings are double-quoted. The supported escapes are `\n`, `\r`, `\t`, `\"`,
and `\\`. A string cannot contain an unescaped newline.

List elements and record fields are comma-separated. Record field names are
identifiers. Duplicate record fields are rejected when outer code executes.
Trailing commas are not currently accepted.

### 3.4 Separators

Top-level items and inner statements are separated by one or more newlines or
`;`. A separator is optional immediately before `}` and at end of file.

Newlines are accepted:

- after `=` in a binding;
- before and after a pipeline `|`;
- inside parameter, argument, list, and record delimiters.

### 3.5 Semantic identity qualifiers

A callable transform name may be followed by `#` and 1 through 64
lowercase hexadecimal characters:

```tima
darken#4f26a3(img, 0.8)
bytes | png.decode#0123456789abcdef
```

The hexadecimal text is a full Transform ID or a prefix of one. It is checked
against the named transform during compilation. A mismatch is an error at the
qualifier, which makes this syntax a reproducibility assertion rather than a
second name-resolution mechanism. The qualifier does not contribute to typed
IR or change the Transform ID, Recipe ID, or runtime call semantics.

Qualifiers are supported on user-defined transforms and registered standard
transforms. They are not supported on ordinary values, outer builtins, or
reserved inner runtime operations. The name still selects the transform, so a
short prefix need not be globally unique.

## 4. Program structure

A source file is a sequence of:

```text
top-level binding
transform declaration
top-level expression
```

Examples:

```tima
img = asset("cat.png")

transform scale(x: f32, factor: f32) -> f32 {
    return x * factor
}

result = 4.0 | scale(0.5)
trace(result)
```

Transform declarations are collected and checked before outer execution, so a
transform may be called regardless of its source order. Recursive transform
definitions are rejected in v0.

## 5. Expressions

The shared expression forms are:

```text
literal
name
(expression)
list literal
record literal
callee(arguments)
receiver.member
transform-name#identity-prefix
left binary-op right
input | stage
```

From highest to lowest precedence:

1. calls, member access, and semantic identity qualification;
2. `*`, `/` (left-associative);
3. `+`, `-` (left-associative);
4. one of `==`, `!=`, `<`, `<=`, `>`, `>=`;
5. `|` (left-associative).

Comparison chaining is unsupported. Parentheses may override precedence.

### 5.1 Calls and arguments

Outer calls may use positional arguments, named arguments, or both:

```tima
darken(img, 0.8)
darken(img=img, factor=0.8)
webp.encode(image, quality=85)
darken#4f26a3(img, 0.8)
```

Arguments are evaluated in source order, then associated with parameters.
Positional arguments fill the next unfilled parameter. Unknown, duplicate,
missing, and excess arguments are errors.

An outer callee must be either a direct name or a one-level callable namespace
such as `webp.encode`, optionally followed by a semantic identity qualifier.
Member access is not otherwise executable in outer code.

Inner calls must use a directly named transform, optionally identity-qualified,
and positional arguments only.

### 5.2 Pipeline expressions

A pipeline inserts its input as the first positional argument of the next
stage:

```tima
x | f              // f(x)
x | f(a, flag=true) // f(x, a, flag=true)
```

A stage must be a transform/builtin name, a one-level namespaced registered
transform, an identity-qualified transform name, or a call to one of those.
Pipelines associate left-to-right, so:

```tima
x | f | g(2)
```

means `g(f(x), 2)`.

## 6. Outer language

### 6.1 Values

The outer runtime has these value kinds:

- `null`;
- `bool`;
- `i64` integer;
- `f32` float;
- immutable UTF-8 string;
- immutable bytes;
- immutable list;
- immutable string-keyed record;
- logical asset locator;
- immutable image;
- transform callable;
- immutable lineage value.

Bytes, images, transform callables, assets, and lineage values are created by
the runtime or host operations; they have no general literal syntax.

All outer composites use immutable value semantics. Bindings may not currently
be rebound, and a duplicate top-level binding is an error.

### 6.2 Arithmetic and comparisons

Outer arithmetic requires two operands of the same numeric kind:

- integer arithmetic is checked `i64` arithmetic; overflow and division by
  zero are errors;
- float arithmetic is `f32` arithmetic.

There are no implicit numeric conversions.

Equality is defined only for two values of the same scalar kind: `null`,
`bool`, integer, float, or string. Ordering is defined only for two integers or
two floats of the same kind. Equality or ordering of lists, records, assets,
images, transforms, bytes, or lineage values is unsupported.

Binary expression results do not automatically inherit operand lineage.
Lineage is recorded at source and transform boundaries.

### 6.3 Outer builtins

#### `asset(locator)`

`asset` accepts exactly one string, optionally named `path` or `locator`, and
returns a lazy logical asset reference. It does not read bytes immediately.
The value begins with unresolved source lineage.

Tima has no ambient filesystem fallback. A host must explicitly provide asset
reading.

#### `read(asset)`

`read` accepts exactly one asset value and observes its locator through the
host's asset-reading capability. It returns immutable `Bytes` with observed
source lineage, including the Content ID and Source ID. If replay supplied an
expected source content identity, changed bytes are rejected before downstream
cache reuse.

Separating source observation from decoding keeps codecs pure over values and
allows the same bytes to be inspected, hashed, cached, or passed to any
compatible registered decoder.

#### `trace(value)`

`trace` accepts one value with semantic lineage and returns an immutable,
first-class lineage value. It is an error if the value has no lineage.

#### `replay(value)`

`replay` accepts one invocation-derived value and reconstructs its recorded
recipe as specified in section 12.

#### `save(value, locator)`

`save` currently accepts immutable encoded bytes and a string locator. In
pipeline form it is normally written:

```tima
encoded | save("output.webp")
```

Saving requires an explicit host output capability and returns the original
byte value with unchanged lineage. It is an execution effect, not a semantic
transform: it is not result-cached and replay does not repeat the write.
Overwrite policy belongs to the host; the Histima CLI refuses to replace an
existing output.

## 7. Transform declarations

The syntax is:

```tima
transform name(parameter: Type, ...) -> Type {
    statements
}
```

Every parameter and result has an explicit native-safe type. Transform names
and parameter names must be unique in their respective scopes. The names
`environment_i64`, `image_zero`, and `image_fill` are reserved inner runtime
operations.

### 7.1 Types

The implemented inner types are:

| Type | Meaning |
| --- | --- |
| `bool` | Boolean scalar |
| `u8` | Unsigned 8-bit scalar |
| `i64` | Signed 64-bit scalar |
| `f32` | IEEE-754 single-precision scalar |
| `Image` | Uniquely owned, mutable image storage |
| `ImageView` | Read-only, aliasable image view |

`u8` has no literal suffix. Integer literals are `i64`; a `u8` normally enters
through a parameter or byte-loop element. An outer integer crosses a `u8`
parameter only if it is in `0..=255`, and a returned `u8` becomes an outer
integer.

There are no implicit conversions.

### 7.2 Statements and scope

Transform bodies support:

```tima
name = expression
return expression
if condition { statements } else { statements }
for element in image.bytes { ... }
for pixel in image.pixels { ... }
```

Inner local bindings are inferred, immutable, and cannot shadow parameters or
earlier locals. Branch-local bindings do not escape their branch. There are no
merged branch values.

An `if` condition must be `bool` and an `else` arm is mandatory. A transform
must return a value of its declared type on every path. A statement after a
`return` or fully returning `if` is an error.

### 7.3 Inner expressions

Inner expressions support `bool`, integer, and float literals; names; scalar
binary operators; direct transform calls; and the three reserved runtime
operations. Strings are allowed only as the literal key argument of
`environment_i64`.

Inner arithmetic requires operands of the same type and is defined for `f32`.
The checker and reference interpreter retain checked `i64` arithmetic, but the
Wasm executor rejects a transform whose reachable IR contains it: portable
overflow and division-error semantics have not been chosen. Inner `i64`
arithmetic is therefore not part of the executable v0 contract. `u8`
arithmetic is unsupported.

Equality supports same-typed `bool`, `u8`, `i64`, and `f32`. Ordering supports
same-typed `u8`, `i64`, and `f32`. Image equality is unsupported.

Outer-only syntax and values—including `null`, general strings, lists,
records, member access outside a constrained image loop, and pipelines—are
rejected inside transforms.

### 7.4 Calls and ownership

Calling another transform requires an exact argument count and exact types.
Passing an `Image` consumes that inner value. Using the consumed value again is
an error, including after any branch on which it may have been consumed.
Passing the same owned value to two owned parameters is therefore rejected.

Owned values transfer directly between inner calls; they are not boxed as
outer values between calls. `ImageView` arguments do not transfer ownership
and may alias.

## 8. Implemented image operations

### 8.1 Image layouts

An image carries `width`, `height`, byte `stride`, storage bytes, and a semantic
format:

- **opaque-bytes**: byte-oriented storage with `len == stride * height`;
- **RGBA8**: four interleaved 8-bit channels in red, green, blue, alpha order,
  with `stride >= width * 4` and `len == stride * height`.

Layout multiplication must not overflow. Format and layout participate in
content identity. Wasm results are revalidated before freezing.

Byte operations support both formats. Pixel operations require RGBA8.

### 8.2 `image_zero(image)`

This inner-only operation consumes an owned `Image`, sets every storage byte to
zero in place, and returns ownership of the same storage.

### 8.3 `image_fill(image, value)`

This inner-only operation consumes an owned `Image`, fills every storage byte
with a `u8`, and returns ownership of the same storage.

### 8.4 Byte loop

The implemented byte loop is:

```tima
for byte in img.bytes {
    byte = expression
}
```

`img` must be a directly named owned `Image`. The body must contain exactly one
assignment to the loop binding and its expression must produce `u8`.

If the expression does not use `byte`, the loop has the same typed-IR meaning
as `image_fill`; this canonicalization makes equivalent source forms share
Transform identity. If it uses `byte`, the expression is evaluated once per
storage byte and the result replaces that byte. A byte-loop expression may not
consume another owned value.

General loop bodies, nested loops, indexing, `break`, and `continue` are
unsupported.

### 8.5 RGBA8 pixel loop

The implemented pixel loop is:

```tima
for p in img.pixels {
    p.r *= red_factor
    p.g *= green_factor
    p.b *= blue_factor
    p.a *= alpha_factor
}
```

`img` must be a directly named owned `Image` whose runtime format is RGBA8.
The body must contain one or more `*=` statements. Each of `r`, `g`, `b`, and
`a` may appear at most once. Each factor must be `f32`, must not read `p`, and
is evaluated once before iteration. A factor may not consume an owned value.

For stored channel byte `c` and factor `f`, scaling computes `c * f`, truncates
the fractional part, saturates values above 255, and maps non-positive or NaN
results to zero. Positive infinity maps to 255. Unmentioned channels and row
padding remain unchanged.

General pixel expressions and `=` channel replacement are unsupported.

## 9. Outer/inner boundary, memory, and ABI

Scalars are range- and type-checked at the boundary. `bool` and `u8` lower to
Wasm `i32`, `i64` to Wasm `i64`, and `f32` to Wasm `f32`.

Each program run or replay creates one Wasm memory64 session and one arena in
its imported linear memory. Buffer descriptors use 64-bit byte offsets and
lengths, never host pointers. An image is flattened as offset, byte length,
width, height, stride, and format tag. Dynamic outer tags, Rust objects,
lineage, and cache metadata do not cross this ABI. WASI is not available.

Host-backed bytes must be copied once when first admitted to the Wasm arena.
The session retains a weak mirror keyed by host storage identity, so repeated
read-only views of that same outer storage reuse one allocation. Values
already backed by the current session enter without a host copy.

For an `Image` parameter, the runtime acquires unique mutable storage. Unique
current-session storage transfers directly. Shared or aliased current-session
storage is detached with one linear-memory-to-linear-memory copy. Host storage
is copied into a fresh arena allocation. Multiple owned arguments and
simultaneous owned/view arguments therefore cannot expose a mutable alias.

For an `ImageView` parameter, the runtime reuses current-session storage or the
session's host mirror. Wasm code receives no mutating operation for a view.

A returned owned image must reference an allocation acquired by that
invocation. Its descriptor and layout are validated, then the allocation is
wrapped as immutable outer storage without copying it out of linear memory.
The session remains alive while such outer values exist. Lineage is attached
beside the outer payload and never enters linear memory.

Arena allocations are 16-byte aligned and use a coalescing free list. The
runtime does not compact live buffers. This keeps offsets stable across a run
and makes zero-copy freeze possible.

The Wasm ABI is versioned independently from semantic identity. It does not
use Rust ABI or accept arbitrary dynamic outer objects.

## 10. Runtime-mediated capabilities

Tima does not implicitly read ambient filesystem, environment, network, clock,
or randomness state. A host explicitly supplies capabilities. Semantically
relevant observations are recorded precisely and included in lineage and
Recipe identity.

The first inner capability operation is:

```tima
environment_i64("NAME")
```

Its key must be a non-empty string literal. The host supplies raw bytes; Tima
requires valid UTF-8 decimal `i64` text. The raw bytes are content-hashed as an
external observation. Missing capabilities, invalid UTF-8, and invalid integer
text are errors.

The outer `read(asset)` builtin observes an asset locator through the explicit
asset capability. Arbitrary OS access from Wasm is not part of v0.

## 11. Registered standard transforms

Registered transforms use normal outer call and pipeline syntax. They have
versioned semantic identities and use the same normalized arguments, lineage,
result cache, and replay machinery as user transforms, but are not emitted into
the user-transform Wasm artifact. Decoders place their image result in the
active Wasm arena when one exists, so following inner transforms do not need a
host round trip. The registry is a deliberately small Tima runtime interface;
it is not general native-library FFI.

| Transform | Parameters | Result | Contract |
| --- | --- | --- | --- |
| `ppm.decode` | `bytes` | RGBA8 image | ASCII P3 only; non-zero dimensions; max value 255 |
| `ppm.encode` | `image` | bytes | Deterministic ASCII P3; RGBA8 input |
| `png.decode` | `bytes` | RGBA8 image | Still PNG; supported grayscale/RGB/palette/alpha forms; APNG rejected |
| `png.encode` | `image`, `compression=6` | bytes | RGBA8; compression integer 1 through 9; fixed deterministic settings |
| `webp.encode` | `image`, `quality=85` | bytes | Lossy still WebP; RGBA8; quality integer 0 through 100; alpha preserved losslessly |

Omitting a default and spelling its canonical value produce identical lineage
arguments and Recipe IDs. A registered-transform implementation change that
can alter output must bump that transform's semantic version. WebP decoding is
not implemented.

## 12. Lineage, identity, caching, and replay

### 12.1 Derivation lineage

Lineage is an immutable DAG with three node kinds:

- **source**: logical locator plus optional observed content and Source ID;
- **invocation**: transform name, Transform ID, Recipe ID, normalized argument
  snapshots, ancestor lineage, and external observations;
- **external observation**: capability, precise key, observed Content ID, and
  Dependency ID.

Invocation arguments record small scalar values directly. Materialized large
values are recorded by semantic/content identity so lineage never retains
mutable inner storage merely to support replay.

Derivation lineage excludes compiler artifacts, cache-hit status, timestamps,
profiling data, and other execution history.

### 12.2 Identity domains

All public identities have the canonical text form of exactly 64 lowercase
hexadecimal characters. Each identity kind has a distinct hash domain.

- **Transform ID** identifies semantic transform behavior. It is derived from
  canonical typed IR and referenced Transform IDs. Formatting, comments, local
  names, transform names, declaration order, backend, and target do not
  normally affect it.
- **Recipe ID** identifies one semantic invocation. It includes Transform ID,
  ordered semantic argument identities, and sorted/deduplicated observed
  Dependency IDs.
- **Content ID** identifies a materialized typed value. Type and image format/
  layout are part of identity. Raw imported bytes use a separate untyped-byte
  content domain.
- **Source ID** identifies a locator together with its observed source content.
- **Dependency ID** identifies capability, precise key, and observed content.
- **Artifact ID** identifies compiled Wasm for one Transform ID and also
  includes backend and backend version, compiler version, target, normalized
  CPU features, optimization configuration, and ABI version.
- **Artifact Bundle ID** identifies the ordered artifacts in one compilation
  unit.

Source text itself is not a semantic identity. Recipe ID and Content ID are
not interchangeable: distinct recipes may produce identical content.

### 12.3 Result and artifact caches

Portable Wasm artifacts are cached under
`cache/artifacts/wasm/<Artifact-Bundle-ID>/module.wasm`, independently from
semantic transform results. Their identity includes the Wasm emitter version,
memory64 target, optimization mode, and ABI version. Source spans, formatting,
and local or transform names do not enter the module bytes when they do not
enter Transform identity. Wasmtime's workspace-local machine cache lives under
`cache/wasmtime` and is not a durable Histima artifact.

Transform results are cached by Recipe ID and validated against immutable
content.

A result-cache hit receives the current semantic invocation lineage. The hit
does not add an execution-history node. Conflicting content for one Recipe ID
is a reproducibility error.

Observed source and external dependency state must be validated before a
descendant cached result is accepted. A compiler/backend change may invalidate
an Artifact ID without changing Transform or Recipe IDs.

### 12.4 Replay

In v0, replay means re-executing a recorded transform recipe with its recorded
semantic arguments and dependencies while reusing valid cached intermediates.

Replay must:

1. require invocation lineage;
2. resolve the recorded Transform ID against the current checked program or a
   versioned registered transform;
3. restore exact scalar arguments and materialized arguments by identity;
4. validate recorded sources and external observations before cache reuse;
5. reuse a valid recorded Recipe result or execute the transform;
6. verify that execution reproduces both the Recipe ID and expected Content
   ID.

Replay does not repeat `save` effects. Applying an ancestor derivation to a
different input, arbitrary ancestor substitution, and replay of unavailable
non-materialized values are deferred.

## 13. Diagnostics

Lexer, parser, semantic checker, boundary validation, runtime capabilities,
Wasm loading, caching, and replay report source-spanned diagnostics.
Diagnostics may contain a primary label, related labels, and explanatory
notes. Implementations should identify the violated stratum or boundary rule,
not merely report a backend failure.

Examples include:

- an outer-only value used inside a transform;
- a dynamic outer value that cannot cross a typed inner parameter;
- use of an owned image after it has moved;
- a read-only view passed to a mutating operation;
- an RGBA8 pixel operation applied to opaque-byte storage;
- an unavailable runtime capability;
- changed source or dependency content during replay;
- a returned Wasm descriptor with an invalid layout.

## 14. Execution and backend contract

The Wasm engine is the production executor for inner transforms. The typed-IR
interpreter is a test oracle and must produce the same semantic result and
lineage for supported programs; it is not an independently selectable product
backend. Wasmtime currently compiles Wasm with its Cranelift implementation,
but Cranelift IR and machine code are not Tima artifacts or language contracts.

The backend boundary is conceptually:

```text
TypedIR -> WasmArtifact
```

The backend must preserve typed evaluation order, ownership transfer,
capability observations, boundary validation, and error behavior specified
here.

The default maximum linear-memory size is 4 GiB and must be a positive multiple
of 64 KiB. Histima resolves an override in this order: CLI
`--wasm-memory-limit <size>`, `HISTIMA_WASM_MEMORY_LIMIT`, workspace
`.histima.toml` key `[wasm].memory_limit`, then the default. Sizes use exact
`B`, `KiB`, `MiB`, `GiB`, or `TiB` suffixes. The cap is an execution policy and
does not affect Transform or Recipe identity.

## 15. Deliberately unsupported in v0

The current language does not include:

- rebinding or mutable outer composites;
- unary operators, implicit conversions, or general member evaluation;
- general loops, indexing, or arbitrary inner mutation;
- user-defined composite native types, enums, options, or results;
- inner named arguments;
- recursive transforms;
- functions distinct from transforms or closures;
- classes, inheritance, traits, or advanced generics;
- macros or arbitrary compile-time/source evaluation;
- async/await;
- a borrow checker or general effect system;
- arbitrary native FFI or WASI;
- a Tima-level JIT backend, LLVM IR as the language IR, or LLVM as a required
  backend;
- general filesystem/network/clock/random access;
- tracing of scalar temporaries;
- ancestor substitution during replay;
- multi-output transforms;
- user-defined language strata.

Future support for any item above requires an explicit specification update.

## Appendix A: current end-to-end example

```tima
source = asset("cat.png")

transform darken(img: Image, factor: f32) -> Image {
    for p in img.pixels {
        p.r *= factor
        p.g *= factor
        p.b *= factor
    }
    return img
}

out =
    source
    | read
    | png.decode
    | darken(0.8)
    | webp.encode(quality=85)

derivation = trace(out)
replayed = replay(out)
```

The source asset remains immutable. `darken` receives unique mutable image
storage, the returned image is frozen before `webp.encode` sees it, each
transform result carries semantic lineage, and valid compiled artifacts and
Recipe results may be reused without changing that lineage.
