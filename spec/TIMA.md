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
- **inner code**, introduced by `transform`, is statically checked and is
  executed from typed IR or by the initial AOT runtime.

Both strata use the same lexer, parser, expression syntax tree, source spans,
and diagnostic model. Semantic context determines which syntax and values are
legal in each stratum.

The implemented compilation path is:

```text
Tima source
    -> shared lexer/parser/AST
    -> inner semantic checking
    -> backend-neutral typed Tima IR
    -> typed-IR interpreter
       or AOT Cranelift artifact
```

The interpreter is the default and reference Histima execution engine. Normal
Histima script execution may explicitly select a hybrid AOT engine, which can
emit, cache, link, load, and execute host artifacts for a subset of the same
typed IR while interpreting unsupported transforms.
The native subset includes scalar, String, and Bytes code. Generic Buffer
operations currently remain interpreted. This remains an additive
implementation of `TypedIR -> NativeArtifact`; Cranelift IR is not Tima's
semantic IR and JIT execution is not planned. WebAssembly is reserved for
separately registered plugin transforms through the ABI in section 11.

## 2. Core invariants

These rules take precedence over optimization choices.

1. Outer composite values are immutable. An outer value visible through one
   binding must not be changed by invoking a transform through another binding.
2. An inner `Buffer` is uniquely owned and mutable for the duration of a
   transform invocation.
3. An inner `BufferView` is read-only and may alias other views.
4. Two owned inner parameters must not alias mutable storage, even if the
   caller supplies the same outer buffer twice.
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
NativeValue
    -> execute transform
NativeValue
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
transform  uses  return  if  else  for  in  true  false  null
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
1/3
"cat.png"
[1, 2, 3]
{name: "cat", sizes: [1, 2]}
```

Integer tokens are unsigned decimal text parsed into `i64`; negative literals
are not currently available because unary negation is unsupported. A float
token requires digits on both sides of one decimal point. Exponents and type
suffixes are unsupported.

Two unsigned integer components joined by `/` with no intervening whitespace
form one exact fraction literal: `1/3`. Both components must fit `i64`, and the
denominator must be positive. Whitespace is semantically significant only for
this lexical distinction: `1 / 3` is the existing integer division expression,
while `1/3` is a fraction value. If either side is not an integer token, `/`
remains division; for example, `1/3.0` divides an integer by a float and is
rejected as mixed arithmetic at runtime.

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

### 3.5 Semantic identity assertions

Any expression may be followed by `#` and 1 through 64 lowercase hexadecimal
characters:

```tima
darken#4f26a3(img, 0.8)
bytes | png.decode#0123456789abcdef
source = read(asset("cat.png"))#89abcdef
result = process(input, 0.8)#fedcba98
```

The hexadecimal text is a full identity or a prefix. `value#hash` evaluates
`value`, verifies its selected semantic identity, and returns the same
immutable value unchanged. A mismatch is an error at the assertion. This is a
reproducibility assertion, not global lookup by hash, so the expression to the
left still selects or computes the value.

An abbreviated identity must be unique among distinct identities in the same
identity domain that are known locally. A prefix matching two local Transform
IDs, Source IDs, Recipe IDs, or Content IDs is an error even when the value on
the left has one of those identities. Duplicate references to the same full ID
do not create ambiguity. The diagnostic reports two colliding IDs and requires
a longer prefix or the full ID. Different domains do not compete because the
left-hand value deterministically selects one domain.

The local set includes source-defined, built-in, and configured plugin
transforms; outer values already available in the current execution; and, when
running under Histima, identities in the current workspace catalog. Standalone
Tima has no hidden global identity index. Callable transform prefixes are
checked against the compile-time registry and checked again against a host
catalog at execution when one is available.

The selected identity domain is deterministic:

- a transform value or a directly named transform call uses its Transform ID;
- a value with observed source lineage uses its Source ID;
- a value with invocation lineage uses its Recipe ID;
- any other materialized outer value uses its Content ID.

Lineage identity takes precedence over materialized content identity. Thus a
derived result is asserted by Recipe ID even though its bytes also have a
Content ID. An unresolved asset locator and a lineage-inspection value do not
have one assertable semantic identity and produce a runtime error.

Assertions on directly named user-defined and registered transform calls are
checked during compilation. Other outer value assertions are checked during
execution. Assertions do not contribute to typed IR and do not change any
Transform, Recipe, Source, Content, or Artifact ID. Arbitrary value assertions
are outer-only; inner code may use assertions only to pin directly named Tima
transform calls. Outer builtins and reserved inner runtime operations do not
have a Transform ID and cannot be asserted as callables.

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
expression#identity-prefix
left binary-op right
input | stage
```

From highest to lowest precedence:

1. calls, member access, and semantic identity assertion;
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
webp.encode(buffer, quality=85)
darken#4f26a3(img, 0.8)
```

Arguments are evaluated in source order, then associated with parameters.
Positional arguments fill the next unfilled parameter. Unknown, duplicate,
missing, and excess arguments are errors.

An outer callee must be either a direct name or a one-level callable namespace
such as `webp.encode`, optionally followed by a semantic identity assertion.
Member access is not otherwise executable in outer code.

Inner calls must use a directly named transform, optionally identity-asserted,
and positional arguments only.

### 5.2 Pipeline expressions

A pipeline inserts its input as the first positional argument of the next
stage:

```tima
x | f              // f(x)
x | f(a, flag=true) // f(x, a, flag=true)
```

A stage must be a transform/builtin name, a one-level namespaced registered
transform, an identity-asserted transform name, or a call to one of those.
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
- exact outer `fraction`;
- immutable UTF-8 string;
- immutable bytes;
- immutable list;
- immutable string-keyed record;
- logical asset locator;
- immutable shaped byte buffer;
- transform callable;
- immutable lineage value.

Bytes, buffers, transform callables, assets, and lineage values are created by
the runtime or host operations; they have no general literal syntax.

All outer composites use immutable value semantics. Bindings may not currently
be rebound, and a duplicate top-level binding is an error.

### 6.2 Arithmetic and comparisons

Outer arithmetic requires two operands of the same numeric kind:

- integer arithmetic is checked `i64` arithmetic; overflow and division by
  zero are errors;
- float arithmetic is `f32` arithmetic;
- fraction arithmetic is exact, checked rational arithmetic. Fractions are
  always reduced to a signed numerator and positive denominator; zero is
  canonicalized to `fraction(0, 1)`.

There are no implicit numeric conversions. Integer, float, and fraction
operands cannot be mixed.

Equality is defined only for two values of the same scalar kind: `null`,
`bool`, integer, float, fraction, or string. Ordering is defined only for two
integers, two floats, or two fractions. Equality or ordering of lists, records,
assets, buffers, transforms, bytes, or lineage values is unsupported. Fraction
equality and ordering are mathematical because values are canonical and exact.

Binary expression results do not automatically inherit operand lineage.
Lineage is recorded at source and transform boundaries.

### 6.3 Outer builtins

#### `fraction(numerator, denominator)`

`fraction` accepts two `i64` integers. The denominator must be positive. It
returns an immutable exact rational value reduced to lowest terms. The current
surface constructor and arithmetic require the reduced numerator and positive
denominator to fit the constructor's `i64` range; overflow, a non-positive
denominator, and division by zero are errors.

`fraction` and fraction literals are outer-only. Fractions are deliberately
absent from the inner type system, typed IR, native ABI, and registered-Wasm ABI
until a concrete transform needs exact rational arithmetic. The constructor
remains useful for computed or negative numerators, which cannot be written as
one unsigned fraction token.

#### `f32.from_fraction(value)`

`f32.from_fraction` explicitly converts one fraction to `f32`. Conversion
rounds the exact rational to the nearest IEEE-754 single-precision value, with
ties resolved toward an even significand. There is no implicit fraction-to-
float conversion.

#### `f32.from_bits(bits)`

`f32.from_bits` accepts one integer in `0..=4294967295` and returns the `f32`
with that exact IEEE-754 bit pattern. It is an outer-only representation
primitive used when a value such as negative zero, infinity, or a particular
NaN payload cannot be written with Tima's decimal float-literal grammar. It
performs no numeric conversion.

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
transform name(parameter: Type, ...) -> Type uses capability, ... {
    statements
}
```

The `uses` clause is optional. Its currently recognized capabilities are
`env.read`, `file.read`, and `http.get`. Duplicate and unknown capabilities are
errors. Declarations are broad authority requirements and are part of
Transform identity; successful runtime reads record precise resource keys and
observed content identities.

Every parameter and result has an explicit native-safe type. Transform names
and parameter names must be unique in their respective scopes. The names
`environment_i64`, `buffer_zero`, and `buffer_fill`, plus the namespaced call
`u8.scale`, are reserved inner runtime operations.

Source-defined Tima transforms, standard transforms, and workspace
registered-Wasm transforms are one callable concept at the outer-language
boundary. They use the same call and pipeline syntax, argument association,
semantic identity assertions, invocation lineage, Recipe cache, and replay
path. Their implementation kind affects execution and Artifact identity, not
the meaning of a call.

Rust hosts may invoke any of those transform kinds positionally through
`tima::runtime::invoke_transform`. The
`invoke_transform_with_capabilities` variant supplies an explicit World for
source-defined transforms that declare capabilities. This direct interface
applies standard default arguments and attaches the same invocation lineage as
an outer Tima call; it does not implicitly provide a result cache.

Rust inspection uses one `TransformInfo` contract for every implementation.
It reports the transform's origin (`source`, `standard`, or `workspace`), its
implementation family (`tima` or `registered-wasm`), ordered parameters and
defaults, exact Tima boundary types, result, capabilities, Transform ID, and
optional version, ABI, Artifact ID, and module Content ID. Thus a Wasm
manifest's `buffer` input is inspected as `BufferView`, while its `buffer`
result is inspected as owned `Buffer`. `CompiledProgram::transform_infos`
includes source and registered definitions; `registered_transform_infos`
provides the process-wide standard and workspace-configured subset.

### 7.1 Types

The implemented inner types are:

| Type | Meaning |
| --- | --- |
| `bool` | Boolean scalar |
| `u8` | Unsigned 8-bit scalar |
| `i64` | Signed 64-bit scalar |
| `f32` | IEEE-754 single-precision scalar |
| `String` | Uniquely owned UTF-8 storage |
| `StringView` | Read-only, aliasable UTF-8 storage |
| `Bytes` | Uniquely owned byte storage |
| `BytesView` | Read-only, aliasable byte storage |
| `Buffer` | Uniquely owned, mutable shaped `u8` storage |
| `BufferView` | Read-only, aliasable shaped `u8` storage |

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
for byte in buffer.bytes { ... }
for byte, offset in buffer.bytes { ... }
```

Inner local bindings are inferred, immutable, and cannot shadow parameters or
earlier locals. Branch-local bindings do not escape their branch. There are no
merged branch values.

An `if` condition must be `bool` and an `else` arm is mandatory. A transform
must return a value of its declared type on every path. A statement after a
`return` or fully returning `if` is an error.

### 7.3 Inner expressions

Inner expressions support `bool`, integer, float, and string literals; names;
scalar binary operators; direct transform calls; and reserved runtime
operations. An inner string literal has type `StringView`.

Inner arithmetic requires operands of the same type. `f32` uses IEEE-754
arithmetic. `i64` arithmetic is checked; overflow and division by zero abort
the invocation with a diagnostic. `u8` arithmetic is unsupported.

`u8.scale(value, factor)` is the one implemented explicit mixed scalar
operation. It multiplies a `u8` by an `f32` using binary32 arithmetic, truncates
the result toward zero, and saturates it to `0..=255`; NaN produces zero. It is
intended for general byte-valued buffers and assigns no channel or image
meaning to either argument.

Equality supports same-typed `bool`, `u8`, `i64`, and `f32`. Ordering supports
same-typed `u8`, `i64`, and `f32`. Buffer equality is unsupported.

Outer-only syntax and values—including `null`, lists, records, general member
access outside World calls or the constrained Buffer byte loop, and pipelines—are
rejected inside transforms.

### 7.4 Calls and ownership

Calling another transform requires an exact argument count and exact types.
Passing an owned `String`, `Bytes`, or `Buffer` consumes that inner value. Using
the consumed value again is an error, including after any branch on which it
may have been consumed. Passing the same owned value to two owned parameters
is therefore rejected.

Owned values transfer directly between inner calls; they are not boxed as
outer values between calls. `StringView`, `BytesView`, and `BufferView`
arguments do not transfer ownership and may alias.

## 8. Implemented Buffer operations

### 8.1 Buffer layout

A `Buffer` is shaped `u8` storage with rank one through three. The dimensions
after the first are dense. `outer_stride` is the number of bytes between
successive slices of the first dimension and may include padding:

- rank 1 requires `outer_stride == shape[0]` and
  `byte_length == shape[0]`;
- rank 2 or 3 requires `outer_stride >= product(shape[1..])` and
  `byte_length == shape[0] * outer_stride`.

Layout multiplication must not overflow. Shape, outer stride, and every
storage byte—including padding—participate in Content identity. Results are
revalidated before freezing. Tima assigns no pixel format, channel meaning,
color space, or other image semantics to a Buffer. Such interpretation belongs
to an ordinary transform contract.

### 8.2 `buffer_zero(buffer)`

This inner-only operation consumes an owned `Buffer`, sets every storage byte
to zero in place, and returns ownership of the same storage.

### 8.3 `buffer_fill(buffer, value)`

This inner-only operation consumes an owned `Buffer`, fills every storage byte
with a `u8`, and returns ownership of the same storage.

### 8.4 Byte loop

The implemented byte loop is:

```tima
for byte in buffer.bytes {
    byte = expression
}
```

It may optionally bind the zero-based storage-byte offset:

```tima
for byte, offset in buffer.bytes {
    byte = expression
}
```

`buffer` must be a directly named owned `Buffer`. The body must contain exactly
one assignment to the byte binding and its expression must produce `u8`. The
byte binding is `u8`; the optional offset binding is `i64`. The offset counts
all storage bytes in order, including outer-stride padding. Both bindings are
immutable inputs to the assignment expression and cannot shadow another value.

If the expression uses neither binding, the loop has the same typed-IR meaning
as `buffer_fill`; this canonicalization makes equivalent source forms share
Transform identity. An unused optional offset also disappears during semantic
normalization. Otherwise the expression is evaluated once per storage byte and
the result replaces that byte. A byte-loop expression may not consume another
owned value.

General loop bodies, nested loops, indexing, `break`, and `continue` are
unsupported.

## 9. Outer/inner boundary, memory, and ABI

Scalars are range- and type-checked at the boundary. The interpreter lowers
outer strings, bytes, and buffers into distinct inner representations. Owned
parameters receive detached mutable storage; views retain immutable shared
storage and may alias. Multiple owned arguments and simultaneous owned/view
arguments therefore cannot expose a mutable alias.

Returned owned storage is frozen into an immutable outer value. String, byte,
and Buffer outer storage retain owned allocations behind immutable reference
counting, so a uniquely held value can move into an owned inner parameter and
back out without reallocating. Shared values detach once before mutation.
Lineage is attached beside the outer payload and never enters the inner
representation.

The implemented AOT entry ABI is C-compatible. ABI version 2 uses one fixed
descriptor per statically typed value:

```c
typedef struct {
    uint64_t words[8];
} TimaAbiValue;

int32_t tima_transform_N(
    void *runtime_context,
    const TimaAbiValue *arguments,
    TimaAbiValue *result
);
```

The statically checked transform signature determines how each descriptor is
interpreted. `bool`, `u8`, `i64`, and `f32` use word 0 with the scalar encoding
defined by their type. Status zero means success. Status 2 reports a host
callback failure; the host retains the source-spanned diagnostic rather than
placing diagnostic objects in the ABI. Other nonzero statuses are reserved.

`String`, `StringView`, `Bytes`, and `BytesView` use words 0 through 2 for data
pointer, byte length, and capacity. Views have zero capacity. String bytes are
UTF-8; the host validates an owned String again when it freezes a native
result. Remaining words are reserved and zero in the current ABI.

Inner string literals are emitted as immutable local object data and lowered to
zero-capacity `StringView` descriptors. Passing a literal to native code does
not allocate at runtime. If a literal-derived view is returned to the outer
layer, the host copies it into immutable outer storage before the native module
can be unloaded; artifact memory never becomes outer value storage.

Before a native call, an owned String or Bytes value is uniquely
detached from immutable outer storage. Native code may mutate owned allocations
in place subject to the statically known value type. A returned owned
descriptor must identify exactly one compatible owned argument of the same
type; the host then adopts its allocation while freezing it into a new outer
value. A view points directly into retained immutable outer storage and a
returned view must identify a compatible view argument. These restrictions
make the current result path zero-copy without trusting or freeing a foreign
allocation.

The runtime context points to a C-compatible callback table containing opaque
host user data, a checked String/Bytes allocator, and a World-call trampoline.
Allocations remain in host call state and can be frozen only when the returned
descriptor exactly identifies a registered allocation of the expected static
type. World calls may directly register an already-owned host buffer, avoiding
a copy solely for ABI transfer.

`Buffer` and `BufferView` do not cross this AOT ABI in v0. A transform whose
transitive call graph accepts, returns, or operates on Buffer values remains on
the typed-IR interpreter. This backend limitation does not change Buffer
language semantics or identity.

Generated code receives only the callback table, function pointers, and opaque
user data. No Rust layout, dynamic outer tag, lineage, or cache metadata is
part of the ABI. The ABI supports little-endian x86-64 and AArch64 hosts. ABI
and backend versions are part of Artifact identity, not Transform identity.

## 10. Runtime-mediated capabilities

Tima does not implicitly read ambient filesystem, environment, network, clock,
or randomness state. A host explicitly supplies capabilities. Semantically
relevant observations are recorded precisely and included in lineage and
Recipe identity.

The inner World operations are:

```tima
env.read(name: StringView) -> String
file.read(path: StringView) -> Bytes
http.get(url: StringView) -> Bytes
environment_i64("NAME")
```

Each operation requires its corresponding `uses` declaration. `env.read`
requires UTF-8. `file.read` and `http.get` preserve response bytes exactly.
`environment_i64` remains as a compatibility operation for reading a non-empty
literal environment name as UTF-8 decimal `i64`, and requires `uses env.read`.
Missing grants, invalid UTF-8, invalid integer text, host I/O failures, and
oversized HTTP responses abort the invocation with a source-spanned
diagnostic. Structured `Result` values are deferred.

Reading the same capability/key more than once during one outer transform
invocation must observe one stable Content ID. If it changes mid-invocation,
execution fails rather than recording an ambiguous recipe or snapshot.

The Histima host grants workspace files automatically. Additional file roots,
environment names, and exact URL prefixes are configured in `.histima.toml`:

```toml
[world]
environment = ["MODE"]
retain_environment = ["MODE"]
file_roots = ["../shared-assets"]
http_prefixes = ["https://assets.example.test/v1/"]
```

Relative file paths resolve from the workspace. Existing paths are
canonicalized before the root check, so `..` and filesystem links cannot
escape the granted roots. URL prefixes are matched byte-for-byte. Redirects are
rejected because their target has not been independently granted. HTTP bodies
are limited to 64 MiB and a request has a 30-second global timeout.

The outer `read(asset)` builtin observes an asset locator through the explicit
asset capability. Tima code has no ambient OS access outside the World.

## 11. Registered standard transforms

Registered transforms use normal outer call and pipeline syntax. They have
versioned semantic identities and use the same normalized arguments, lineage,
result cache, and replay machinery as user transforms. The PPM, PNG, and WebP
implementations are registered-Wasm plugins behind the same registry. The
registry is not general native-library FFI.

| Transform | Parameters | Result | Contract |
| --- | --- | --- | --- |
| `ppm.decode` | `bytes` | `Buffer` | ASCII P3 only; returns shape `[height, width, 4]`; max value 255 |
| `ppm.encode` | `buffer` | bytes | Deterministic ASCII P3; requires shape `[height, width, 4]` interpreted as RGBA8 |
| `png.decode` | `bytes` | `Buffer` | Still PNG; returns RGBA8 bytes with shape `[height, width, 4]`; APNG rejected |
| `png.encode` | `buffer`, `compression=6` | bytes | Requires shape `[height, width, 4]` interpreted as RGBA8; compression 1 through 9 |
| `webp.encode` | `buffer`, `quality=85` | bytes | Requires shape `[height, width, 4]` interpreted as RGBA8; quality 0 through 100 |

Omitting a default and spelling its canonical value produce identical lineage
arguments and Recipe IDs. A registered-transform implementation change that
can alter output must bump that transform's semantic version. WebP decoding is
not implemented.

`histima transforms [workspace]` lists the callable standard transforms and
workspace-configured plugins in stable name order. Each entry reports its
origin, implementation family, semantic version, exact boundary signature,
and Transform ID. Workspace plugins additionally report their ABI, Artifact,
and module Content IDs. Transforms declared inside a Tima source file belong
to that compilation and are intentionally not a workspace registry.

### 11.1 Registered-Wasm ABI v4

The current ABI is intentionally the exact slice required by the registered
PPM, PNG, and WebP codecs: immutable Bytes and Buffer inputs, signed integer
configuration, and owned Bytes and Buffer results. The ABI attaches only
generic shape and stride metadata to Buffer values; codec-specific RGBA8
interpretation is not a Tima type or descriptor tag. A module:

- is Wasm32 and imports nothing (in particular, it has no WASI);
- exports `memory`;
- exports `tima_abi_version() -> i32`, which returns `4`;
- exports `tima_reset()` and `tima_alloc(length: i32) -> i32` for one
  invocation's guest arena;
- exports `tima_transform(arguments: i32, argument_count: i32,
  result: i32) -> i32`.

All pointers are unsigned offsets into exported linear memory. `arguments`
points to `argument_count` consecutive descriptors, each containing eight
little-endian `u32` words. The current descriptor kinds are:

| Kind | Meaning | Words |
| --- | --- | --- |
| `1` | immutable `BytesView` argument | `[kind, data, length, 0, 0, 0, 0, 0]` |
| `2` | immutable `BufferView` argument | `[kind, data, length, rank, dim0, dim1, dim2, outer_stride]` |
| `3` | signed `i64` argument | `[kind, low-32, high-32, 0, 0, 0, 0, 0]` |
| `4` | owned `Bytes` result | `[kind, data, length, 0, 0, 0, 0, 0]` |
| `5` | owned `Buffer` result | `[kind, data, length, rank, dim0, dim1, dim2, outer_stride]` |
| `255` | UTF-8 diagnostic | `[kind, data, length, 0, 0, 0, 0, 0]` |

A Buffer rank is 1 through 3 and unused dimension words must be zero. Its shape,
outer stride, and byte length must satisfy section 8.1. A zero
`tima_transform` status means its result descriptor is initialized. Other
statuses, kinds, non-zero reserved words, invalid ranges, and invalid Buffer
layouts fail the invocation. The `i64` words encode the canonical little-endian
two's-complement bit pattern.

The host runs plugins in a fuel-metered interpreter, with no JIT, and limits
linear memory to 64 MiB. ABI v4 limits each argument payload to 32 MiB, result
bytes to 64 MiB, and diagnostic text to 4096 bytes; the combined argument,
descriptor, scratch, and result storage must also fit the linear-memory limit.
An immutable input crosses the sandbox boundary once into guest memory. The
result crosses once into the final host-owned buffer, which is validated and
frozen directly as an outer value; freezing does not make a second copy.
Direct zero-copy sharing with guest linear memory is intentionally not part of
the isolation contract.

Built-in Transform ID remains the registry's semantic name/version identity.
For workspace plugins it includes the namespaced name, semantic version, ABI
version, ordered parameter names and types, result type, **and the verified
module Content ID**. This conservative default prevents result-cache reuse if
module behavior changes while an author forgets to increment the semantic
version. The current manifest format has no opt-out or manual
implementation-equivalence assertion.

A registered-Wasm Artifact ID remains a separate concept. It includes the
Transform ID, ABI/backend configuration, and exact module content identity.
Thus identical module and contract data produce the same Transform and
Artifact IDs; changing module bytes changes both under the default policy.
Including content in semantic identity is a safety policy, not a conflation of
the two domains: future execution packaging or backend configuration may
change Artifact ID without changing Transform ID.

### 11.2 Workspace-local plugin registration

Histima workspaces may explicitly opt into external ABI-v4 modules through
`.histima.toml`:

```toml
[plugins]
manifests = ["plugins/example-encode.toml"]
```

Each listed TOML manifest contains exactly these fields:

```toml
name = "example.encode"
semantic_version = 1
abi_version = 4
module = "example_encode.wasm"
module_content = "<module byte Content ID>"
result = "bytes"

[[parameters]]
name = "buffer"
type = "buffer"
```

The transform name is exactly two Tima identifiers separated by one dot and
may not collide with a built-in or another configured plugin. Parameters are
ordered, uniquely named, and currently have no defaults. Parameter types are
`bytes`, `buffer`, or `i64`; result type is `bytes` or `buffer`.
These spellings map directly to ABI-v4 immutable views, scalar arguments, and
owned results.

Manifest paths are relative to the workspace. Module paths are relative to
their manifest. Both are canonicalized and must remain files inside the
workspace, including after resolving `..` and filesystem links. Manifests are
limited to 1 MiB and modules to 64 MiB. `module_content` is the Tima Content ID
of the exact module bytes; a mismatch rejects workspace opening before Wasm
compilation.

Loading also verifies the ABI version, no-import rule, memory and function
exports, and function signatures. A configured plugin receives no World
capabilities and has no WASI. There is no ambient discovery, download, update,
package manager, or native-library fallback. Once loaded, its calls use normal
pipeline and `name#hash` syntax and the standard Recipe cache, lineage, trace,
and replay rules. Replay requires the same semantic plugin definition to be
available when the workspace is reopened.

`histima plugins [workspace]` is the read-only inspection surface for the
configured registry. It reports transforms in stable name order with the
manifest semantic and ABI versions, ordered parameter/result signature,
Transform ID, module-sensitive Artifact ID, and exact module Content ID.
`--json` additionally exposes the parameters and result as structured fields.
This command lists workspace-configured plugins, not the built-in transform
registry. Use `histima transforms` for the unified callable view.

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
  canonical Hash IR and referenced Transform IDs. Formatting, comments, local
  names, source transform names, declaration order, backend, and target do not
  normally affect it.
- **Recipe ID** identifies one semantic invocation. It includes Transform ID,
  ordered semantic argument identities, and sorted/deduplicated observed
  Dependency IDs.
- **Content ID** identifies a materialized typed value. Type and Buffer shape,
  outer stride, and storage bytes are part of identity. Raw imported bytes use
  a separate untyped-byte content domain.
- **Source ID** identifies a locator together with its observed source content.
- **Dependency ID** identifies capability, precise key, and observed content.
- **Artifact ID** identifies compiled native code for one Transform ID and also
  includes backend and backend version, compiler version, target, normalized
  CPU features, optimization configuration, and ABI version.
- **Artifact Bundle ID** identifies the ordered artifacts in one compilation
  unit.

Source text itself is not a semantic identity. Recipe ID and Content ID are
not interchangeable: distinct recipes may produce identical content.

#### Transform identity boundary

Transform identity is established at an explicit semantic boundary:

```text
Tima source
    -> parse / typecheck
    -> language-defined semantic normalization
    -> Hash IR                         [Transform identity boundary]
    -> execution / optimization IR
    -> interpreter / Cranelift / future backend
```

Two transforms have the same Transform ID when they lower to the same
canonical operational semantics under Tima's explicitly defined normalization
rules. This is deliberately narrower than arbitrary extensional program
equivalence. Hash IR is a semantic identity representation, not an optimizer
IR and not a proof that every pair of programs producing the same mathematical
result has one identity.

Language-defined normalization removes source-only distinctions such as
formatting, comments, transform/parameter/local names, declaration order, and
capability declaration order. It may also equate multiple surface forms when
Tima explicitly specifies one semantic operation. In particular, the initial
whole-buffer byte-assignment loop whose expression uses neither the byte nor
optional offset binding is defined to normalize to `buffer.fill`, so it has
the same identity as a direct `buffer_fill` call.

No other optimizer behavior is implied. Hash IR does not automatically apply
algebraic identities such as replacing `x + 0` with `x`, reassociate
expressions, simplify floating-point operations, canonicalize NaNs or signed
zero, perform CSE, inline calls, eliminate semantic computations, or otherwise
optimize code. Such a normalization may be added only as an explicit Tima
language rule. Optimizations performed for interpretation or native code
generation occur after or independently of the identity boundary and cannot
change the Hash IR of an unchanged transform.

The current compiler's backend-neutral typed structures may help implement
both sides of this boundary, but their Rust layout is not the identity
contract. A replacement parser, semantic analyzer, execution IR, interpreter,
Cranelift lowering, optimizer, or backend must reproduce the same
language-normalized Hash IR for the same transform semantics.

#### Hash IR v1

Hash IR v1 is a frozen, language-neutral semantic graph grammar. Transform
identity does not serialize compiler typed-IR structs or assign wire tags to
Tima's current types and operations. Instead, every semantic construct is a
schema-qualified node with named fields. A schema has a UTF-8 namespace, UTF-8
name, and independent `u32` semantic version. New Tima constructs and other
finite language IRs add schemas or schema versions without changing Hash IR
v1's wire grammar.

Tima reserves the `tima` schema namespace and Histima reserves `histima`.
Other producers must use a stable namespace they control. Changing a schema's
meaning requires a new schema version. Unknown schemas can still be validated,
canonically encoded, and hashed; interpreting or executing them requires an
implementation of that schema.

The two versioning layers have separate purposes:

```text
Hash IR format version
    controls the structural graph encoding

(namespace, schema-name, schema-version)
    controls the semantic meaning of one node schema
```

Adding a Tima construct should normally add a schema under the existing v1
graph grammar. Changing an existing schema's meaning, including adding a field
that changes its semantic contract, requires a new schema version. Existing
`tima:*@1` and `histima:*@1` meanings must never silently change. A new Hash IR
format version is reserved for a structural requirement that truly cannot be
represented by v1's nodes, fields, data atoms, references, and sequences; a
new operation alone is not such a requirement. Execution backends may evolve
without changing either Hash IR or semantic schema versions.

A definition is one finite rooted directed graph. Graphs may share nodes and
contain cycles, which admits ordinary expression graphs, SSA/CFG forms with
block arguments, nested-region encodings, recursive type descriptions, and
future language-specific semantic nodes. Source spans, source-facing names,
backend choices, artifact configuration, and other non-semantic data remain
excluded.

Node sharing is an intentional semantic property. Tima lowering creates one
Hash IR node for each semantic computation or value and reuses that node for
every use of the same value. Distinct semantic computations remain distinct
nodes even when their schemas and fields happen to be structurally identical.
Therefore a root referencing one computation twice differs from a root
referencing two separately recomputed but structurally identical computations.
Likewise, absent a future explicit normalization rule, these transforms need
not have the same Transform ID:

```tima
a = x * 2
return a + a
```

```tima
return (x * 2) + (x * 2)
```

Hash IR v1 performs neither graph isomorphism nor structural hash-consing.
Producer arena order is irrelevant, but the producer must construct sharing
deterministically from pre-optimization Tima semantics. CSE or another
execution optimization must never change identity-layer sharing.

Hash IR excludes source spans, diagnostics, compiler-generated names, backend
configuration, target architecture and CPU features, optimization levels,
backend-only allocation/lifetime metadata, cache state, profiling data,
Artifact IDs, and all other execution-only details. A later execution IR may
introduce SSA values, phi nodes, inlining, CSE, vector operations, or other
backend forms without changing Hash IR for the original Tima semantics.

The canonical byte primitives are:

- `u8`, `u32`, `u64`, and `i64` are fixed-width little-endian integers;
- `bytes` is `u32 byte_length` followed by those bytes;
- `text` is `bytes` whose payload is UTF-8;
- `sequence<T>` is `u32 element_count` followed by each encoded element;
- a digest is exactly 32 uninterpreted bytes whose meaning comes from its
  containing schema field.

Every definition encodes as the 13 bytes `TIMA-HASH-IR\0`, `u32(1)`,
`sequence<node>`, and a `u32` canonical root ID. The root ID is always zero.
A node encodes as `text namespace`, `text schema_name`, `u32 schema_version`,
and `sequence<field>`. A field encodes as `text field_name` followed by its
data value. Schema namespace, schema name, and field names must be non-empty;
field names must be unique within one node.

Fields are encoded in ascending bytewise UTF-8 name order. Canonical node IDs
are independent of producer arena allocation: assign the root ID zero, then
traverse fields in canonical order and sequence elements in semantic order,
assigning each newly encountered node the next ID before traversing that node.
Encode nodes in that discovery order and replace node references with their
canonical IDs. Repeated references and cycles reuse the first assigned ID.
Unreachable arena nodes are not part of the definition and are omitted.
This last rule removes producer arena garbage; it is not permission for the
encoder to perform semantic dead-code elimination. Language lowering remains
responsible for connecting every semantically ordered computation to the
rooted graph.

The fixed structural data tags are:

| Tag | Data payload |
| ---: | --- |
| 0 | unit: no payload |
| 1 | boolean: canonical `u8` 0 or 1 |
| 2 | unsigned integer: `u64` |
| 3 | signed integer: `i64` |
| 4 | raw IEEE-754 binary32 bits: `u32` |
| 5 | raw IEEE-754 binary64 bits: `u64` |
| 6 | bytes |
| 7 | text |
| 8 | node reference: `u32` canonical node ID |
| 9 | digest: 32 bytes |
| 10 | sequence: `sequence<data>` |

These atoms are structural rather than a closed language type system. Records
are nodes; maps are sequences of entry nodes whose schema defines ordering;
optional fields use unit or schema-defined omission; arbitrary-width numbers
and other future literals use canonical bytes interpreted by their schema.
Consequently new language types, operations, control-flow forms, ownership
rules, and effect descriptions do not consume new Hash IR wire tags.

Current typed Tima transforms use root schema
`tima:definition.transform@1` with fields `capabilities` (canonical sorted
sequence), `parameters` (ordered sequence of type nodes), `result` (type node),
and `entry` (CFG block node). Current leaf type schemas are `type.bool`,
`type.u8`, `type.i64`, `type.f32`, `type.string`, `type.string-view`,
`type.bytes`, `type.bytes-view`, `type.buffer`, and `type.buffer-view`, all in
namespace `tima` at schema version 1 with no fields. Capability schemas are
`capability.env.read`, `capability.file.read`, and `capability.http.get` under
the same namespace and version.

Declared capabilities are transform semantics and therefore appear in Hash IR
and Transform ID. Precise runtime observations do not: for example, declaring
`uses file.read` affects Transform ID, while observing `font.ttf` with a
particular Content ID belongs to dependency lineage and Recipe ID.

Current Tima value and operation schemas are:

| `tima` schema at version 1 | Fields |
| --- | --- |
| `value.parameter` | `index` unsigned, `type` node |
| `constant.bool` | `value` boolean, `type` node |
| `constant.i64` | `value` signed, `type` node |
| `constant.f32` | `bits` binary32 bits, `type` node |
| `constant.string` | `value` text, `type` node |
| `binary.add`, `binary.subtract`, `binary.multiply`, `binary.divide`, `binary.equal`, `binary.not-equal`, `binary.less`, `binary.less-equal`, `binary.greater`, `binary.greater-equal` | `left` node, `right` node, `type` node |
| `numeric.u8-scale` | `value` node, `factor` node, `type` node |
| `operation.call` | `arguments` ordered node sequence, `transform` digest, `type` node |
| `world.environment-i64` | `name` text, `type` node |
| `world.environment-read` | `name` node, `type` node |
| `world.file-read` | `path` node, `type` node |
| `world.http-get` | `url` node, `type` node |
| `buffer.zero` | `buffer` node, `type` node |
| `buffer.fill` | `buffer` node, `value` node, `type` node |
| `buffer.byte-element` | `type` node |
| `buffer.byte-map` | `buffer` node, `element` node, `instructions` ordered node sequence, `result` node, `type` node |
| `buffer.byte-index` | `type` node |
| `buffer.byte-map-indexed` | `buffer` node, `element` node, `index` node, `instructions` ordered node sequence, `result` node, `type` node |

`constant.f32.bits` preserves the exact IEEE-754 bit pattern. Positive and
negative zero, distinct NaN payloads, and otherwise algebraically equivalent
floating-point expressions are not canonicalized together.

The `transform` digest in `operation.call` is the callee's semantic Transform
ID. A call never refers to a source name, source declaration order, local
`TransformId`, compiler index, or backend artifact. This keeps callers stable
across callee renaming and source reordering while still changing caller
identity when the referenced callee semantics change.

`tima:cfg.block@1` has `arguments` (an ordered node sequence, empty for the
current typed IR), `instructions` (evaluation-order node sequence), and
`terminator` (node). Terminator schemas are `terminator.return` with `value`,
`terminator.branch` with `condition`, `then`, and `else`, and
`terminator.jump` with `target`.

Standard and contract-only non-Tima transforms use root schema
`histima:definition.external-transform@1` with text `scheme` and `name`,
unsigned `semantic_version`, unit-or-unsigned `interface_version`, ordered
`parameters`, and unit-or-unsigned `result_type`. Each parameter uses
`histima:external.parameter@1` with text `name` and unsigned `type_code`.
Type codes are interpreted by the external scheme. Its meaning and fields are
unchanged.

Workspace-provided Wasm transforms use the additive root schema
`histima:definition.workspace-wasm@1` with text `name`; unsigned
`semantic_version`, `abi_version`, and `result_type`; ordered `parameters`
using `histima:external.parameter@1`; and digest `module_content`. The digest
is the verified module Content ID and is part of semantic identity by the
workspace safety policy. Artifact IDs remain outside Hash IR. This new schema
does not change the frozen v1 graph grammar or the existing external-transform
schema.

Transform ID is:

```text
SHA-256("tima.transform-id.hash-ir-v1\0" || canonical_hash_ir_v1_bytes)
```

The following golden vectors are normative:

> **Archival compatibility warning:** Do not update these expected IDs after
> an implementation refactor merely to make tests pass. Rust representation,
> arena allocation, parser/sema implementation, execution IR, optimization,
> Cranelift changes, new backends, source spans, and diagnostics must not alter
> them. An intentional semantic-schema or structural compatibility break must
> use the appropriate schema or format version before publishing new vectors.

| Definition | Transform ID |
| --- | --- |
| `transform keep(value: i64) -> i64 { return value }` | `ee7691ec5931fd0275c2bab44a630077922ccbc60215caf1d75f8f07a6475c35` |
| the preceding `keep` called by `transform apply(value: i64) -> i64 { return keep(value) }` | `2aed765597ed27f4890088f3467637376071c78815a017efdb4b6419636c2551` |
| standard operation `ppm.decode`, semantic version 3 | `05b5808b2657b94dce94a07cc7f476f4cc91934da878f3371365968f926154c1` |
| contract-only registered-Wasm `fixture.encode`, semantic version 1, ABI 4, parameters `buffer:2, quality:3`, result 1 | `f365c363fa4e2ea1902a839ada1679deae2bc9a9a0148c2fd1b275a9c8279404` |

For an encoding-level vector, a one-node graph whose root schema is namespace
`a`, name `bc`, schema version 1, with no fields, encodes as hexadecimal:

```text
54494d412d484153482d49520001000000010000000100000061020000006263010000000000000000000000
```

### 12.3 Result and artifact caches

The interpreter does not produce an Artifact ID or artifact cache entry. The
AOT Cranelift backend uses a separate filesystem artifact cache; Artifact IDs
include backend, compiler version, target, CPU features, optimization
configuration, and ABI version independently from Transform ID. Histima's
SQLite artifact records remain available for later product integration and
for migration/inspection of older workspaces.

Transform results are cached by Recipe ID and validated against immutable
content.

Histima's durable result store supports immutable `Bytes` and `Buffer` values.
A stored Buffer retains its rank, shape, outer stride, and every storage byte,
including padding; loading reconstructs and validates the Buffer layout before
cache reuse or replay. Its storage encoding is a host persistence detail and
does not replace or alter the semantic Buffer Content ID.

Durable stocking is Histima host policy, not Tima semantics. By default,
`histima run` stocks only bindings explicitly selected with `--record`. With
the explicit `--stock-intermediates` policy, Histima waits for the complete
program to succeed and then stocks newly produced immutable `Bytes` and
`Buffer` invocation results reachable through the final outer bindings or
final expression. Results reachable only from discarded expressions, scalar
results, and all results from a failed program are not stocked. The policy
does not affect Transform, Recipe, or Content IDs, lineage, or execution-engine
selection.

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
non-materialized values are deferred. Strict replay is the default: external
dependencies are read again and must have the recorded Content ID.

Histima also implements snapshot replay, selected by `histima replay ...
--snapshot`. File and HTTP observations are retained as raw immutable CAS
content by default. Environment observations remain hash-only unless their
name appears in `[world].retain_environment` as well as `[world].environment`.
Snapshot replay resolves every recorded dependency from retained bytes and
does not access that external resource. A missing snapshot is an error. Source
assets retain their separate source-validation behavior. Retention metadata
and policy do not enter Dependency, Recipe, or Content identity.

### 12.5 Recipe expressions

Histima can render a durably recorded recipe as one outer-Tima expression:

```text
histima expression [workspace] <recipe-id>
```

The generated expression reconstructs the first-argument pipeline, renders
other recorded arguments positionally, pins each transform with its full
Transform ID, reconstructs observed sources as `read(asset(locator))#source`,
and asserts the final Recipe ID. Executing it therefore either produces the
queried derivation or fails an identity assertion. A recorded materialized
argument without reconstructable lineage cannot be emitted as source text.
Arguments remain positional because parameter names are not part of semantic
Transform identity and may change without changing a Transform ID.
Finite non-negative floats use an exactly round-tripping decimal literal.
Every other `f32` bit pattern uses `f32.from_bits`, so recipe-expression
generation preserves negative values, signed zero, infinities, subnormals, and
NaN payloads bit for bit.
Recorded non-negative fractions use canonical `numerator/denominator` source.
Negative numerators use `fraction(numerator, denominator)` and are reconstructed
with checked integer subtraction because unary negation is not yet part of Tima
syntax.

`--input <tima-expression>` replaces the deepest value on the primary
first-argument chain. The supplied text must itself be exactly one one-line
outer expression. This form retains Transform-ID pins but omits the recorded
Source- and final Recipe-ID assertions affected by the replacement. Executing
the result creates a new derivation; it is not replay and does not weaken the
strict replay contract. Non-primary argument branches retain their recorded
lineage and identity assertions.

## 13. Diagnostics

Lexer, parser, semantic checker, boundary validation, World operations,
caching, and replay report source-spanned diagnostics.
Diagnostics may contain a primary label, related labels, and explanatory
notes. Implementations should identify the violated stratum or boundary rule,
not merely report a backend failure.

Examples include:

- an outer-only value used inside a transform;
- a dynamic outer value that cannot cross a typed inner parameter;
- use of an owned Buffer after it has moved;
- a read-only view passed to a mutating operation;
- an unavailable runtime capability;
- changed source or dependency content during replay;
- an invalid Buffer layout at the outer/inner or plugin boundary.

## 14. Execution and backend contract

The typed-IR interpreter is the reference executor for inner transforms. It
defines current evaluation, ownership, World-observation, lineage, and error
behavior together with the typed IR contract. Normal Histima script execution
uses it by default and may explicitly select the hybrid AOT engine with
`histima run ... --engine hybrid-aot`; `--engine interpreter` selects the
reference path explicitly.

The backend boundary is conceptually:

```text
TypedIR -> NativeArtifact
```

The AOT Cranelift backend emits a host relocatable object through `tima
emit-object`. `tima run-native` additionally caches that object, links a host
load image with Clang, and executes supported transforms through the native ABI.
`TIMA_CLANG` may select the Clang executable; otherwise the runtime uses the
standard LLVM installation on Windows or `clang` from `PATH`.

The implemented subset admits scalar parameters and results of `bool`, `u8`,
`i64`, or `f32`, plus owned and view `String` and `Bytes` boundaries;
scalar constants, comparisons, control flow, and `f32` arithmetic; and direct
calls whose complete callee closure is native-compatible. Calls marshal the
same fixed descriptors through native stack storage, forward the runtime
context, and propagate failure status to the outermost invocation. String and
byte descriptors currently support identity returns and passthrough call
chains; they have no native mutation operations yet.

Checked `i64` arithmetic, `u8.scale`, general string operations, newly
allocated native Buffers, and `environment_i64` are not compiled. String literals are emitted as
read-only object data. `env.read`, `file.read`, and `http.get` are therefore
compiled for both literal keys and keys supplied by native-compatible
`StringView` values. They call the host through the runtime context, retain
precise observations, propagate source-spanned errors, and return registered
host allocations for zero-copy freeze. A transform remains interpreted when
any transitive callee uses unsupported behavior. The hybrid engine makes that
decision per outer invocation without changing Transform, Recipe, or Content
identity, lineage, replay semantics, or registered standard/Wasm dispatch.
Backend and artifact-cache details are execution metadata only. The standalone
`tima run-native` command and Histima's explicit `hybrid-aot` choice exercise
the same boundary; neither makes AOT the mandatory default.

Native artifacts are cached independently using Artifact IDs derived from the
Transform ID plus the Cranelift/compiler version, target, inferred CPU feature
configuration, optimization setting, and ABI version. A compilation-unit
bundle ID includes the ordered Artifact IDs. The linked shared-library load
image is a rebuildable execution cache derived from the validated object, not a
second semantic artifact. Artifact metadata reports the total source byte size
of emitted string data. None of these machine details affect semantic
lineage or Recipe IDs. Extending the backend must preserve typed evaluation
order, ownership transfer, World observations, boundary validation, and error
behavior specified here. JIT compilation is out of scope.

Tima does not generate or execute WebAssembly for inner-language transforms.
WebAssembly is reserved for the separately registered plugin boundary described
in section 11.

## 15. Deliberately unsupported in v0

The current language does not include:

- rebinding or mutable outer composites;
- unary operators, implicit conversions, or general member evaluation;
- general loops, arbitrary indexing, or arbitrary inner mutation (the
  constrained Buffer byte loop may expose its current storage offset);
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
- clock/random access and any filesystem/network access outside granted World
  operations;
- tracing of scalar temporaries;
- ancestor substitution during replay;
- multi-output transforms;
- user-defined language strata.

Future support for any item above requires an explicit specification update.

## Appendix A: current end-to-end example

```tima
source = asset("cat.png")

transform darken_byte(value: u8, offset: i64, factor: f32) -> u8 {
    pixel = offset / 4
    alpha = pixel * 4 + 3
    if offset == alpha {
        return value
    } else {
        return u8.scale(value, factor)
    }
}

transform darken(buffer: Buffer, factor: f32) -> Buffer {
    for byte, offset in buffer.bytes {
        byte = darken_byte(byte, offset, factor)
    }
    return buffer
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

The source asset remains immutable. `darken` receives unique mutable Buffer
storage, treats the codec's `[height, width, 4]` contract as ordinary byte
layout rather than a core Tima type, and preserves every fourth alpha byte.
The returned Buffer is frozen before `webp.encode` sees it, each
transform result carries semantic lineage, and valid Recipe results may be
reused without changing that lineage. Future compiled artifacts will remain
execution details outside semantic lineage.
