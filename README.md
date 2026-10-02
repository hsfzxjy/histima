# Histima / Tima

Histima is a local asset manager and transformation runner. Tima is its
embedded language: a convenient immutable outer scripting layer surrounds a
small statically checked inner language introduced by `transform`.

The repository currently provides:

- one Rust lexer/parser/AST, source-span model, and diagnostic system shared by
  both language strata;
- immutable outer values, calls, pipelines, registered codecs, lineage, replay,
  and Recipe-ID result caching;
- statically checked transforms lowered to backend-neutral typed Tima IR;
- a production typed-IR interpreter with owned mutable inner values,
  read-only aliasable views, and freeze-on-return;
- a capability-controlled `World` boundary for observable external state;
- a SQLite catalog at `.histima.sql3` plus a filesystem content-addressed
  store for durable assets, lineage, recipes, and results;
- a CLI for import, execution, recording, query, trace, replay, and
  materialization.

The typed IR is the semantic compiler boundary. Native compilation uses
ahead-of-time Cranelift, not JIT. WebAssembly is reserved for separately
registered plugin transforms; Tima does not generate WebAssembly for
inner-language transforms.

The AOT Cranelift slice can emit a host relocatable object with `cargo run -p
tima -- emit-object program.tima`, or link and execute supported scalar,
String, and Bytes transforms with `cargo run -p tima -- run-native
program.tima`. The latter is a hybrid path: Buffer operations and other
unsupported transforms remain interpreted. Supported inner transform calls
are linked in the same artifact; a caller falls back to the interpreter when
any transitive callee is unsupported. Histima still uses the interpreter while
the remaining Buffer operations and World callbacks are implemented.

## Build and try it

```text
cargo test --workspace
cargo run -p histima -- init build/my-workspace
cargo run -p histima -- import build/my-workspace examples/tiny.ppm
cargo run -p histima -- import build/my-workspace examples/textures --recursive
cargo run -p histima -- transforms build/my-workspace
cargo run -p histima -- summary build/my-workspace
cargo run -p histima -- verify build/my-workspace
cargo run -p histima -- search build/my-workspace png
cargo run -p histima -- pipeline build/my-workspace 'asset("examples/tiny.ppm") | read | ppm.decode | webp.encode(quality=85)'
cargo run -p histima -- run build/my-workspace examples/image_pipeline.tima --record out
cargo run -p histima -- trace build/my-workspace <recipe-id>
cargo run -p histima -- expression build/my-workspace <recipe-id>
cargo run -p histima -- replay build/my-workspace examples/image_pipeline.tima <recipe-id>
```

Every `[workspace]` CLI argument is optional. When omitted, Histima walks from
the current directory to the nearest ancestor containing `.histima.sql3`.
Opening an older workspace migrates `catalog.sqlite3` to that hidden name.
Add `--json` anywhere for structured output.

`histima summary [workspace]` combines catalog counts with bounded first pages
of assets and recipes and the callable transform registry. `--limit <1-100>`
applies independently to each section; asset and recipe cursors can be passed
to their dedicated listing commands. The catalog has no timestamps, so the
summary uses stable semantic ordering and does not pretend those pages are
execution-history recency.

`histima import [workspace] <source-path>...` imports one or more explicit
files. Add `--recursive` to traverse directory arguments in deterministic
lexical order; recursive traversal rejects symbolic links instead of following
them. A single non-recursive file retains the original output shape, while a
batch reports every imported asset in order. When positional workspace/source
arguments would be ambiguous, `--workspace <path>` selects the workspace
explicitly.

Catalog listings use bounded SQLite keyset pagination. `histima assets` and
`histima recipes` accept `--limit <1-100>` and return `next_cursor` when more
rows exist. Pass that value back with `--after <cursor>` to fetch the next
page. Asset cursors are locators; recipe cursors are Recipe IDs. Calls without
these options retain the existing 100-row maximum. `assets --prefix <text>`
filters by a case-sensitive locator prefix, while `recipes --transform <name>`
matches an exact recorded transform name; both filters compose with cursors.

`histima search [workspace] <query>` performs a bounded, case-sensitive
substring search across current asset locators and recorded transform names.
Results are ordered by locator and Recipe ID. `--limit <1-100>` applies
independently to each category and reports when either category is truncated;
this initial search surface does not build a separate full-text index.

`histima verify [workspace]` is an explicit read-only integrity pass. It runs
SQLite integrity and foreign-key checks, then validates every cataloged CAS
object's canonical path, recorded kind, byte length, and Content ID. Invalid
workspaces produce a structured report and non-zero exit status; verification
never repairs or removes data.

`histima pipeline [workspace] <expression>` accepts exactly one outer Tima
expression. Invocation-derived byte results are automatically recorded in
workspace stock, so a later process can reuse them. Custom `transform`
declarations remain file-based through `histima run`.

`histima expression [workspace] <recipe-id>` prints one outer-Tima expression
for a recorded output. It pins transforms by full Transform ID, reconstructs
recorded sources, and asserts the queried Recipe ID, so the output can be fed
back to `histima pipeline` when all transforms are registered. Add `--input
<tima-expression>` to replace the primary starting input; the generated source
then omits the old Source/Recipe assertions and represents a new derivation.
Source-defined transforms still need their declarations when the expression is
embedded in a Tima file. Recorded `f32` arguments round-trip bit exactly;
ordinary values remain decimal while exceptional representations use the
outer-only `f32.from_bits(...)` constructor.

Outer orchestration also has exact canonical fractions. Construct them with
the compact `1/3` literal or `fraction(numerator, denominator)`, use checked
arithmetic only with other fractions, and convert explicitly with
`f32.from_fraction(...)`. The compact form requires no whitespace: `1 / 3`
remains integer division. Fractions are currently outer-only.

## A small Tima program

```tima
source = asset("cat.png")

transform copy(buffer: Buffer) -> Buffer {
    return buffer
}

out =
    source
    | read
    | png.decode
    | copy
    | webp.encode(quality=85)

derivation = trace(out)
replayed = replay(out)
```

Outer composites are immutable. Inner owned values such as `Buffer`, `Bytes`,
and `String` are unique and consumable; `BufferView`, `BytesView`, and
`StringView` are read-only and may alias. Returning to outer code freezes the
value. Dynamic outer tags, lineage, and cache metadata do not enter inner
representations.

`Buffer` is general rank-1-through-rank-3 shaped `u8` storage with dense inner
dimensions and an explicit outer stride. It has no image format in the Tima
type system. Registered deterministic codecs currently include PPM and PNG
decode/encode and WebP encode; their ordinary transform contracts interpret
`[height, width, 4]` buffers as RGBA8. They use the same
Transform/Recipe/Content identity, lineage, cache, and replay model as user
transforms and run as fuel- and memory-bounded registered-Wasm plugins.

Any outer value with one selected semantic identity can be pinned as
`value#hash`, where `hash` is a full lowercase identity or prefix. Transform
values/calls use Transform ID, observed sources use Source ID, derived values
use Recipe ID, and otherwise materialized values use Content ID. The assertion
returns the value unchanged and is not lookup by hash. A prefix must uniquely
match its identity domain among identities known in the current program,
execution, and Histima workspace catalog; local collisions require a longer or
full hash.

## The World boundary

Inner transforms declare broad external authority explicitly:

```tima
transform load_config() -> Bytes uses file.read {
    return file.read("config.bin")
}

transform mode() -> String uses env.read {
    return env.read("MODE")
}

transform fetch() -> Bytes uses http.get {
    return http.get("https://assets.example.test/v1/palette.bin")
}
```

Recognized declarations and calls are:

- `uses env.read` / `env.read(StringView) -> String`;
- `uses file.read` / `file.read(StringView) -> Bytes`;
- `uses http.get` / `http.get(StringView) -> Bytes`.

`environment_i64("NAME")` remains as a compatibility helper and also requires
`uses env.read`.

Histima grants workspace files automatically. Extra authorities are explicit
in `<workspace>/.histima.toml`:

```toml
[world]
environment = ["MODE"]
retain_environment = ["MODE"]
file_roots = ["../shared-assets"]
http_prefixes = ["https://assets.example.test/v1/"]
```

File paths are canonicalized before checking the workspace and configured
roots. Environment names must be listed exactly. HTTP URLs must match a listed
prefix byte-for-byte, redirects are rejected rather than escaping that grant,
and response bodies are capped at 64 MiB. Tima itself
never falls back to ambient filesystem, environment, network, clock, or
randomness access.

Every successful read records its precise key and observed Content ID in
lineage and Recipe identity. File and HTTP response bytes are retained in the
workspace CAS by default. Environment values are hash-only unless the name is
also listed in `retain_environment`.

Replay uses strict re-observation by default: it reads each external dependency
again and rejects changed content before cached descendants are accepted.
`histima replay ... --snapshot` instead uses retained observations without
touching those external resources. Snapshot availability is execution/storage
policy; it does not change Dependency or Recipe identity. Source assets still
follow their separately recorded source-validation rules.

## Workspace Wasm plugins

A workspace may opt into local registered-Wasm transforms explicitly:

```toml
# <workspace>/.histima.toml
[plugins]
manifests = ["plugins/example-encode.toml"]
```

```toml
# <workspace>/plugins/example-encode.toml
name = "example.encode"
semantic_version = 1
abi_version = 4
module = "example_encode.wasm"
module_content = "<64-character Tima Content ID of the module bytes>"
result = "bytes"

[[parameters]]
name = "buffer"
type = "buffer"
```

The current manifest types are `bytes`, `buffer`, and `i64`; results are
limited to `bytes` or `buffer`. Manifest and module paths are resolved and
required to remain inside the canonical workspace. Opening the workspace
verifies the declared module Content ID, ABI version, signature, exports,
absence of imports/WASI, and transform-name uniqueness before compiling the
module in the existing sandbox. `histima import <module>` prints the raw byte
Content ID accepted by `module_content`.

Plugin transforms use ordinary call, pipeline, and `name#hash` syntax and
participate in the same lineage, Recipe cache, trace, and replay behavior as
built-ins. Histima does not search for, download, update, or grant ambient
capabilities to plugins. `histima plugins [workspace]` lists the configured
contracts in stable name order, including each signature and its Transform,
Artifact, and module Content IDs; add `--json` for machine-readable output.
`histima transforms [workspace]` is the unified discovery view: it merges the
standard registry with configured workspace plugins and reports implementation
kind, semantic version, signature, and Transform ID.

## Storage, lineage, and identity

The filesystem CAS contains immutable payloads; SQLite contains queryable
source versions, current locator heads, normalized lineage, recipes, result
references, and legacy/future artifact metadata. CAS publication and output
materialization are atomic, and materialization refuses to replace an existing
file.

Tima keeps distinct hash domains for Transform ID, Recipe ID, Content ID,
Source ID, Dependency ID, and Artifact ID. Backend choice, cache hits, and
execution timestamps never alter semantic derivation lineage. The Cranelift
artifact cache remains independent from the existing Recipe-ID result cache.

The normative implemented contract is [`spec/TIMA.md`](spec/TIMA.md). Local
design rationale and work plans live under `agents/` as required by
`AGENTS.md` and are intentionally not committed.
