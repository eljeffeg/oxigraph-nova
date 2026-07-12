Oxigraph Nova for JavaScript
============================

JavaScript/WebAssembly bindings for [Oxigraph Nova](https://github.com/jgentes/oxigraph-nova), API-compatible with
[oxigraph-js](https://www.npmjs.com/package/oxigraph), backed by `oxigraph-nova-store` instead of upstream `oxigraph::store::Store`.

This package provides a simple in-memory RDF store with [SPARQL 1.1 Query](https://www.w3.org/TR/sparql11-query/) and
[SPARQL 1.1 Update](https://www.w3.org/TR/sparql11-update/) capabilities, compiled to WebAssembly via `wasm-bindgen`.

The store can load RDF serialized in [Turtle](https://www.w3.org/TR/turtle/), [TriG](https://www.w3.org/TR/trig/),
[N-Triples](https://www.w3.org/TR/n-triples/), [N-Quads](https://www.w3.org/TR/n-quads/) and
[RDF/XML](https://www.w3.org/TR/rdf-syntax-grammar/).

## Example

```js
import { Store } from "oxigraph-nova-js";
import dataModel from "@rdfjs/data-model";

const store = new Store();
const ex = dataModel.namedNode("http://example/");
const schemaName = dataModel.namedNode("http://schema.org/name");
store.add(dataModel.triple(ex, schemaName, dataModel.literal("example")));
for (const binding of store.query("SELECT ?name WHERE { <http://example/> <http://schema.org/name> ?name }")) {
    console.log(binding.get("name").value);
}
```

## Building

The wasm32 build is produced by a hand-rolled `wasm-bindgen` invocation
(`python3 build_package.py`), **not** `wasm-pack` — this crate builds a
`cdylib` that needs a `#[cfg]`/feature setup `wasm-pack` doesn't drive
(matching the split workspace targets used elsewhere in Nova). It requires
a nightly Rust toolchain with the `wasm32-unknown-unknown` target, plus an
LLVM-backed `clang`/`llvm-ar` pair for the C shims some transitive
dependencies compile for that target:

```sh
# One-time (per machine) setup, e.g. via Homebrew's `llvm` on macOS:
export CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang
export AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar
export PATH="$HOME/.rustup/toolchains/nightly-aarch64-apple-darwin/bin:$PATH"

# Build (debug, faster iteration):
python3 build_package.py --debug

# Build (release):
python3 build_package.py
```

**Known macOS/nightly toolchain gotcha:** the nightly toolchain's own
`rust-lld` linker can SIGABRT while linking the wasm32 artifact if it can't
resolve its own `libLLVM.dylib`. If you hit that, symlink the toolchain's
`libLLVM.dylib` into `rust-lld`'s expected location instead of setting
`DYLD_LIBRARY_PATH` — the latter causes a *worse* failure (rustc itself
SIGSEGVs):

```sh
ln -sf "$HOME/.rustup/toolchains/nightly-aarch64-apple-darwin/lib/libLLVM.dylib" \
       "$HOME/.rustup/toolchains/nightly-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/lib/libLLVM.dylib"
```

Adjust the toolchain triple/paths above if your host isn't
`aarch64-apple-darwin`. Once `pkg/` is built, `npm test` (`npm run
build-debug && vitest run`) runs the vendored test suite against it.

## Known differences from upstream `oxigraph-js`

`Store.prototype.query`'s `default_graph`, `named_graphs` and `use_default_graph_as_union` dataset-selection
options are not supported by this Nova-backed build — see `src/store.rs`'s `query()` method, which raises an
explicit error if any of the three are passed. All other options (`base_iri`, `results_format`) behave the same
as upstream.

## License

This project is licensed under the MIT license ([LICENSE](../../LICENSE) or http://opensource.org/licenses/MIT).
