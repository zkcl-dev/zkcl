# zkcl

A Common Lisp implementation in Rust with a command-line toolchain and an
embeddable runtime. zkcl targets ANSI Common Lisp and is still under development.
Conformance testing uses [ANSI-TEST](https://gitlab.common-lisp.net/ansi-test/ansi-test).

## How it started

I got `zkcl` by rolling my fingers across the keyboard. It happened to end in
`cl`, and I was writing Lisp in SBCL at the time, so I started building my own
implementation for fun.

## Install

Build the CLI from a checkout.

```sh
cargo build --release -p zkcl-cli
export PATH="$PWD/target/release:$PATH"
```

Published toolchains include `install-toolchain.sh` for macOS and Linux on arm64
and x86-64. Run it with `sh` to install `zkcl` and `zkclx`. Use `--toolchain nightly`
for nightly or `--toolchain VERSION` for an exact release. Linux binaries
require glibc 2.35 or newer. [GitHub Releases](https://github.com/zkcl-dev/zkcl/releases)

For a managed installation, `zkcl toolchain upgrade` updates your default channel.
`zkcl toolchain pin nightly` writes an exact release to `zkcl-toolchain`. Commit
that file so `zkcl toolchain install` can install the same release in another
checkout. `zkcl +nightly test` selects an installed toolchain for one command.

## Run and edit

```sh
zkcl new demo --name example/demo
zkcl -C demo run
```

Use `zkcl repl` for an interactive session or `zkcl run script.lisp` for a
standalone file. See the [examples](examples/README.md) for programs to try.

`zkcl check` and `zkcl lint` analyze source without evaluating it. Use `zkcl fmt`
to format Lisp files and `zkcl lsp` to start the language server. From Lisp,
`(require :zk-syntax)` exposes source snapshots, syntax nodes, diagnostics, and
edits. Structural editing commands are in [sexp-edit.zkcl](https://github.com/zkcl-dev/sexp-edit.zkcl).

For source debugging in native code, compile the function with
`(declare (optimize (debug 3)))` to retain source locations and maps of live
lexical bindings.

## Projects

`zkcl.sexp` declares source files, targets, and dependencies. `zkcl lock` records
the resolved dependencies in `zkcl.lock`, and `zkcl fetch` caches their sources.
Use `zkcl work init core app` to group projects under one lockfile.
`zkcl x namespace/tool@1.2.3` runs a binary from a registry release.

A `(build "tools/generate.lisp")` declaration runs the script before compiling
project targets. Declare its generated files with `(output "generated/config.lisp")`
inside `build`. The script writes them under `ZKCL_OUT_DIR`.
[Unit converter example](examples/README.md#unit-converter-project)

## Build targets

```sh
zkcl build app.lisp -o app
zkcl build app.lisp --target js -o dist/app.mjs
```

Native linking uses a system C compiler. If `zkcl toolchain target list` lists
`wasm32-unknown-unknown`, install it with `zkcl toolchain target add wasm32-unknown-unknown`
and pass `--target wasm32-unknown-unknown` to `zkcl build`.

Load the generated ES module in Node or a browser.

```js
import { instantiate } from './dist/app.mjs';

const { values } = instantiate().evalString('(+ 40 2)');
```

Use `instantiateAsync(options)` when a top-level form calls a Promise-returning
host function. Project binaries also export `run()` and `runAsync(options)` to
call their entry points. With `--split-runtime`, deploy the generated runtime
module alongside the app modules.

Pass `{ executionStrategy: 'bytecode' }` for interactive `COMPILE` under a policy
that blocks string code generation. Supply a `pathnameHost` for filesystem or
module-loading operations.

For building LLVM IR from Lisp, see [llvm.zkcl](https://github.com/zkcl-dev/llvm.zkcl).

## Embedding in Rust

```rust
use zkcl::Runtime;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::start()?;
    runtime.load_str("(defun add (a b) (+ a b))")?;
    let add = runtime.lookup_symbol("CL-USER", "ADD")?.unwrap();
    let a = runtime.integer_from(19_i32)?;
    let b = runtime.integer_from(23_i32)?;
    let values = runtime.call(&add, &[&a, &b])?;
    assert_eq!(values[0].as_fixnum(), Some(42));
    Ok(())
}
```

Enable `project` to load a source project with its dependencies and build scripts.
Enable `delivery` to load a prepared `ProjectBundle`. Keep the project session or
bundle files and the writable ASDF cache available while the loaded code uses them.

```sh
cargo run -p zkcl --features project --example project -- PROJECT_PATH BUNDLE_DIRECTORY
cargo run -p zkcl --features delivery --example bundle -- BUNDLE_DIRECTORY CACHE_DIRECTORY
```

## Embedding in C

```sh
cargo build --release -p zkcl-capi
```

The C API uses the same `project` and `delivery` features for source and bundle
loading. [Header](crates/zkcl-capi/include/zkcl.h) ·
[Embedding example](crates/zkcl-capi/examples/embed.c) ·
[Project example](crates/zkcl-capi/tests/project.c)

## Workspace

Start in `crates/zkcl` for the embedding API, `crates/zkcl-runtime` for the compiler
and VM, and `crates/zkcl-syntax` for source analysis shared by the CLI and language
server. The full workspace is listed in [Cargo.toml](Cargo.toml).

## License

zkcl is licensed under the [Apache License, Version 2.0](LICENSE). Third-party
notices are included with the [bundled ASDF distribution](crates/zkcl-runtime/lisp/bundled/asdf/LICENSE)
and the [registry test fixtures](crates/zkcl-registry/src/signed/test_keys/README.md).
