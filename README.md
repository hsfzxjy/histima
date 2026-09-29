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
registered plugin transforms whose ABI is still deferred. Tima does not
generate or execute WebAssembly for inner-language transforms.

The AOT Cranelift slice can emit a host relocatable object with `cargo run -p
tima -- emit-object program.tima`, or link and execute supported scalar and
image transforms with `cargo run -p tima -- run-native program.tima`. The
latter is a hybrid path: unsupported transforms remain interpreted. Its native
ABI supports zero-copy immutable image views and ownership transfer for image
identity, zero, fill, and RGBA8 channel-scaling operations. Histima still uses
the interpreter while the remaining buffer operations and World callbacks are
implemented.

## Build and try it

```text
cargo test --workspace
cargo run -p histima -- init build/my-workspace
cargo run -p histima -- import build/my-workspace examples/tiny.ppm
cargo run -p histima -- pipeline build/my-workspace 'asset("examples/tiny.ppm") | read | ppm.decode | webp.encode(quality=85)'
cargo run -p histima -- run build/my-workspace examples/image_pipeline.tima --record out
cargo run -p histima -- trace build/my-workspace <recipe-id>
cargo run -p histima -- replay build/my-workspace examples/image_pipeline.tima <recipe-id>
```

Every `[workspace]` CLI argument is optional. When omitted, Histima walks from
the current directory to the nearest ancestor containing `.histima.sql3`.
Opening an older workspace migrates `catalog.sqlite3` to that hidden name.
Add `--json` anywhere for structured output.

`histima pipeline [workspace] <expression>` accepts exactly one outer Tima
expression. Invocation-derived byte results are automatically recorded in
workspace stock, so a later process can reuse them. Custom `transform`
declarations remain file-based through `histima run`.

## A small Tima program

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

Outer composites are immutable. Inner owned values such as `Image`, `Bytes`,
and `String` are unique and consumable; `ImageView`, `BytesView`, and
`StringView` are read-only and may alias. Returning to outer code freezes the
value. Dynamic outer tags, lineage, and cache metadata do not enter inner
representations.

Image support currently includes opaque byte rows and validated RGBA8 storage,
owned byte loops, `image_zero`, `image_fill`, and constrained RGBA8 channel
scaling. Registered deterministic codecs currently include PPM and PNG
decode/encode and WebP encode. They use the same Transform/Recipe/Content
identity, lineage, cache, and replay model as user transforms, but their host
implementations are provisional pending a registered-Wasm-plugin ABI.

Transform references can assert semantic identity as `name#hash`, where `hash`
is a full Transform ID or lowercase prefix. The assertion does not itself
change semantic identity.

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
