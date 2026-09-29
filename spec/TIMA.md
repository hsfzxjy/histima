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

The interpreter is the Histima product execution engine in this version. The
standalone Tima runtime can emit, cache, link, load, and execute host artifacts
for a subset of the same typed IR, while interpreting unsupported transforms.
The native subset includes scalar code and initial owned/view image operations.
This remains an additive implementation of `TypedIR -> NativeArtifact`;
Cranelift IR is not Tima's semantic IR and JIT execution is not planned.
WebAssembly is reserved for separately registered plugin transforms, whose ABI
is not yet part of this contract.

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
| `String` | Uniquely owned UTF-8 storage |
| `StringView` | Read-only, aliasable UTF-8 storage |
| `Bytes` | Uniquely owned byte storage |
| `BytesView` | Read-only, aliasable byte storage |
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

Inner expressions support `bool`, integer, float, and string literals; names;
scalar binary operators; direct transform calls; and reserved runtime
operations. An inner string literal has type `StringView`.

Inner arithmetic requires operands of the same type. `f32` uses IEEE-754
arithmetic. `i64` arithmetic is checked; overflow and division by zero abort
the invocation with a diagnostic. `u8` arithmetic is unsupported.

Equality supports same-typed `bool`, `u8`, `i64`, and `f32`. Ordering supports
same-typed `u8`, `i64`, and `f32`. Image equality is unsupported.

Outer-only syntax and values—including `null`, lists, records, general member
access outside World calls or a constrained image loop, and pipelines—are
rejected inside transforms.

### 7.4 Calls and ownership

Calling another transform requires an exact argument count and exact types.
Passing an owned `String`, `Bytes`, or `Image` consumes that inner value. Using
the consumed value again is an error, including after any branch on which it
may have been consumed. Passing the same owned value to two owned parameters
is therefore rejected.

Owned values transfer directly between inner calls; they are not boxed as
outer values between calls. `StringView`, `BytesView`, and `ImageView`
arguments do not transfer ownership and may alias.

## 8. Implemented image operations

### 8.1 Image layouts

An image carries `width`, `height`, byte `stride`, storage bytes, and a semantic
format:

- **opaque-bytes**: byte-oriented storage with `len == stride * height`;
- **RGBA8**: four interleaved 8-bit channels in red, green, blue, alpha order,
  with `stride >= width * 4` and `len == stride * height`.

Layout multiplication must not overflow. Format and layout participate in
content identity. Results are revalidated before freezing.

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

Scalars are range- and type-checked at the boundary. The interpreter lowers
outer strings, bytes, and images into distinct inner representations. Owned
parameters receive detached mutable storage; views retain immutable shared
storage and may alias. Multiple owned arguments and simultaneous owned/view
arguments therefore cannot expose a mutable alias.

Returned owned storage is frozen into an immutable outer value. String and byte
outer storage retains an owned allocation behind immutable reference counting,
so a uniquely held value can move into an owned inner parameter and back out
without reallocating. Shared values detach once before mutation. Lineage is
attached beside the outer payload and never enters the inner representation.

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
defined by their type. An `Image` or `ImageView` uses words 0 through 6 for its
data pointer, byte length, capacity, format, width, height, and row stride;
views have zero capacity. Format tag 0 denotes opaque bytes and tag 1 denotes
RGBA8. Word 7 is reserved. Status zero means success; status 1 reports that an
RGBA8 operation received another image format. Status 2 reports a host callback
failure; the host retains the source-spanned diagnostic rather than placing
diagnostic objects in the ABI. Other nonzero statuses are reserved.

`String`, `StringView`, `Bytes`, and `BytesView` use words 0 through 2 for data
pointer, byte length, and capacity. Views have zero capacity. String bytes are
UTF-8; the host validates an owned String again when it freezes a native
result. Remaining words are reserved and zero in the current ABI.

Inner string literals are emitted as immutable local object data and lowered to
zero-capacity `StringView` descriptors. Passing a literal to native code does
not allocate at runtime. If a literal-derived view is returned to the outer
layer, the host copies it into immutable outer storage before the native module
can be unloaded; artifact memory never becomes outer value storage.

Before a native call, an owned String, Bytes, or Image value is uniquely
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
a copy solely for ABI transfer. Image allocation is reserved until its layout
metadata contract is defined.

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
| `ppm.decode` | `bytes` | RGBA8 image | ASCII P3 only; non-zero dimensions; max value 255 |
| `ppm.encode` | `image` | bytes | Deterministic ASCII P3; RGBA8 input |
| `png.decode` | `bytes` | RGBA8 image | Still PNG; supported grayscale/RGB/palette/alpha forms; APNG rejected |
| `png.encode` | `image`, `compression=6` | bytes | RGBA8; compression integer 1 through 9; fixed deterministic settings |
| `webp.encode` | `image`, `quality=85` | bytes | Lossy still WebP; RGBA8; quality integer 0 through 100; alpha preserved losslessly |

Omitting a default and spelling its canonical value produce identical lineage
arguments and Recipe IDs. A registered-transform implementation change that
can alter output must bump that transform's semantic version. WebP decoding is
not implemented.

### 11.1 Registered-Wasm ABI v3

The current ABI is intentionally the exact slice required by the registered
PPM, PNG, and WebP codecs: immutable bytes and image inputs, signed integer
configuration, and owned bytes and image results. PNG decoding normalizes
supported still-image color forms to RGBA8 inside the sandbox and rejects
APNG. A module:

- is Wasm32 and imports nothing (in particular, it has no WASI);
- exports `memory`;
- exports `tima_abi_version() -> i32`, which returns `3`;
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
| `2` | immutable `ImageView` argument | `[kind, data, length, format, width, height, stride, 0]` |
| `3` | signed `i64` argument | `[kind, low-32, high-32, 0, 0, 0, 0, 0]` |
| `4` | owned `Bytes` result | `[kind, data, length, 0, 0, 0, 0, 0]` |
| `5` | owned `Image` result | `[kind, data, length, format, width, height, stride, 0]` |
| `255` | UTF-8 diagnostic | `[kind, data, length, 0, 0, 0, 0, 0]` |

Image format `1` is RGBA8. A zero `tima_transform` status means its result
descriptor is initialized. Other statuses, kinds, formats, non-zero reserved
words, invalid ranges, and invalid image layouts fail the invocation. The
`i64` words encode the canonical little-endian two's-complement bit pattern.

The host runs plugins in a fuel-metered interpreter, with no JIT, and limits
linear memory to 64 MiB. ABI v3 limits each argument payload to 32 MiB, result
bytes to 64 MiB, and diagnostic text to 4096 bytes; the combined argument,
descriptor, scratch, and result storage must also fit the linear-memory limit.
An immutable input crosses the sandbox boundary once into guest memory. The
result crosses once into the final host-owned buffer, which is validated and
frozen directly as an outer value; freezing does not make a second copy.
Direct zero-copy sharing with guest linear memory is intentionally not part of
the isolation contract.

Built-in Transform ID remains the registry's semantic name/version identity.
For workspace plugins it includes the namespaced name, semantic version, ABI
version, ordered parameter names and types, and result type. A registered-Wasm
Artifact ID is separate and includes Transform ID, ABI/backend configuration,
and the exact module content identity. Replacing a module while claiming the
same semantic contract changes Artifact ID but not Transform ID; changing
observable behavior requires a semantic version bump.

### 11.2 Workspace-local plugin registration

Histima workspaces may explicitly opt into external ABI-v3 modules through
`.histima.toml`:

```toml
[plugins]
manifests = ["plugins/example-encode.toml"]
```

Each listed TOML manifest contains exactly these fields:

```toml
name = "example.encode"
semantic_version = 1
abi_version = 3
module = "example_encode.wasm"
module_content = "<module byte Content ID>"
result = "bytes"

[[parameters]]
name = "image"
type = "rgba8-image"
```

The transform name is exactly two Tima identifiers separated by one dot and
may not collide with a built-in or another configured plugin. Parameters are
ordered, uniquely named, and currently have no defaults. Parameter types are
`bytes`, `rgba8-image`, or `i64`; result type is `bytes` or `rgba8-image`.
These spellings map directly to ABI-v3 immutable views, scalar arguments, and
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
registry.

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
- **Artifact ID** identifies compiled native code for one Transform ID and also
  includes backend and backend version, compiler version, target, normalized
  CPU features, optimization configuration, and ABI version.
- **Artifact Bundle ID** identifies the ordered artifacts in one compilation
  unit.

Source text itself is not a semantic identity. Recipe ID and Content ID are
not interchangeable: distinct recipes may produce identical content.

### 12.3 Result and artifact caches

The interpreter does not produce an Artifact ID or artifact cache entry. The
AOT Cranelift backend uses a separate filesystem artifact cache; Artifact IDs
include backend, compiler version, target, CPU features, optimization
configuration, and ABI version independently from Transform ID. Histima's
SQLite artifact records remain available for later product integration and
for migration/inspection of older workspaces.

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

## 13. Diagnostics

Lexer, parser, semantic checker, boundary validation, World operations,
caching, and replay report source-spanned diagnostics.
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
- an invalid native value or image layout at the outer/inner boundary.

## 14. Execution and backend contract

The typed-IR interpreter is the production executor for inner transforms. It
defines current evaluation, ownership, World-observation, lineage, and error
behavior together with the typed IR contract.

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
`i64`, or `f32`, plus owned and view `String`, `Bytes`, and `Image` boundaries;
scalar constants, comparisons, control flow, and `f32` arithmetic; and direct
calls whose complete callee closure is native-compatible. Calls marshal the
same fixed descriptors through native stack storage, forward the runtime
context, and propagate failure status to the outermost invocation. String and
byte descriptors currently support identity returns and passthrough call
chains; they have no native mutation operations yet.

`Image` and `ImageView` may cross the descriptor boundary. Identity returns,
owned `image_zero` and `image_fill`, byte-map loops, and RGBA8 channel scaling
are compiled. Native byte maps evaluate their typed scalar instruction sequence
once per storage byte, including calls. Native RGBA8 operations validate the
runtime format before addressing pixels and preserve the scalar conversion and
row-padding semantics in section 8.5.

Checked `i64` arithmetic, general string operations, newly allocated native
images, and `environment_i64` are not compiled. String literals are emitted as
read-only object data. `env.read`, `file.read`, and `http.get` are therefore
compiled for both literal keys and keys supplied by native-compatible
`StringView` values. They call the host through the runtime context, retain
precise observations, propagate source-spanned errors, and return registered
host allocations for zero-copy freeze. A transform remains interpreted when
any transitive callee uses unsupported behavior. The hybrid `run-native` path
makes that decision per outer invocation without changing lineage or Recipe
identity. Histima product execution remains interpreted while the native path
matures.

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
transform result carries semantic lineage, and valid Recipe results may be
reused without changing that lineage. Future compiled artifacts will remain
execution details outside semantic lineage.
