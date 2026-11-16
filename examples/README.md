# zkcl examples

Build the CLI and run an example from the repository root.

```sh
cargo build --release -p zkcl-cli
./target/release/zkcl run examples/persistent-tree.lisp
```

Use the same `run` command for the other standalone files. Most run a few
checks and then print the results of the example.

## Persistent tree

zkcl's persistent sequence tree is based on Sek by Arthur Charguéraud and
François Pottier (ICFP 2026). It supports indexed edits, push and pop at both
ends, and splits and joins that share untouched subtrees. Thawing and taking a
snapshot share the tree in constant time. Mutable handles can update exclusively
owned nodes in place and copy shared nodes on the edited path. The release-notes
example shows how this preserves versions for undo, redo, and branching.
[persistent-tree.lisp](persistent-tree.lisp) · [Sek paper](https://cambium.inria.fr/~fpottier/publis/chargueraud-pottier-sek.pdf)

## Language and runtime

- `swapf` uses `get-setf-expansion` to evaluate each place's subforms once.
  Passing `&environment` lets it handle places from `symbol-macrolet`.
  [macros.lisp](macros.lisp)
- A CLOS generic function dispatches on the event's class and the requested
  output format. An `:around` method logs the call and preserves both values.
  The record renderer uses `zk-mop` to inspect inherited slots. [clos.lisp](clos.lisp)
- A lexical declaration tells the import macro how to handle an invalid
  percentage. Restarts can supply a replacement or omit the entry.
  [configuration.lisp](configuration.lisp)
- An AST simplifier matches a closed set of typed expression variants. The
  script also shows the diagnostic for a missing case. [pattern-matching.lisp](pattern-matching.lisp)
- A lexer scans across chunk boundaries inside `λ` and `==`. Its trace shows
  both character ranges and byte ranges in the original UTF-8.
  [unicode-lexer.lisp](unicode-lexer.lisp)
- A frame reader scans a decimal length prefix and unpacks the binary body.
  Bit patterns read signed and little-endian fields. The UTF-8 payload keeps
  its original byte range. [binary-frames.lisp](binary-frames.lisp)
- A bounded subscription can drain the channel or disconnect early. The suite
  also exercises child failures, cancellation, deadlines, racing tasks, and
  asynchronous file reads. [async.lisp](async.lisp)
- Saturating SIMD addition brightens RGBA pixels while preserving alpha. A mask
  bounds accesses to the final partial pack. [simd.lisp](simd.lisp)
- A Gray stream prefixes each line of a job log written with ordinary Common
  Lisp stream operations. [gray-streams.lisp](gray-streams.lisp)
- Source edits insert a missing closing parenthesis and wrap `雪`, preserving the
  comment. Each edit is tied to the snapshot it was made against.
  [source-editing.lisp](source-editing.lisp)
- A weak-key cache stores grapheme counts. After collection it retains the
  live label's count and drops the temporary label's entry. [weak-cache.lisp](weak-cache.lisp)
- Property tests check round trips, stateful models, and linearizability. The
  script also prints shrunk counterexamples. [property-testing.lisp](property-testing.lisp)

## Unit converter project

The build script reads the unit table and writes a Lisp source file under
`ZKCL_OUT_DIR`. Converting 90 seconds to minutes prints `3/2 min`.
[Manifest](units/zkcl.sexp) · [Build script](units/generate.lisp) · [Converter](units/convert.lisp)

```sh
./target/release/zkcl -C examples/units check
./target/release/zkcl -C examples/units run -- 90 s min
./target/release/zkcl -C examples/units test
./target/release/zkcl -C examples/units test --doc
```

Build the same project as a native executable or as an ES module for Node.

```sh
./target/release/zkcl -C examples/units build -o "$PWD/target/units"
./target/units 90 s min
./target/release/zkcl -C examples/units build --target js -o "$PWD/target/units.mjs"
node --input-type=module <<'JS'
import { instantiate } from './target/units.mjs';
const { values } = instantiate().evalString('(examples.units:convert 90 "s" "min")');
console.log(values[0]); // { numerator: 3, denominator: 2 }
JS
```

For Core WebAssembly, build the runtime for `wasm32-unknown-unknown` and pass
it to `build`. The module takes the same command-line arguments in `zkcl run`.

```sh
rustup target add wasm32-unknown-unknown
cargo build --release -p zkcl-runtime --example wasm_runtime --target wasm32-unknown-unknown
./target/release/zkcl -C examples/units build --target wasm32-unknown-unknown \
  --wasm-runtime "$PWD/target/wasm32-unknown-unknown/release/examples/wasm_runtime.wasm" \
  -o "$PWD/target/units.wasm"
./target/release/zkcl run target/units.wasm -- 90 s min
```

## Lisp-defined bit-count VOP

`zk-sys:define-vop` attaches a register-only population-count template to an
ordinary Lisp function for subsequent calls compiled in the same file. In the
bitmap loop the compiler loads each array element, checks the live function
binding, and unboxes an unsigned 64-bit integer. LLVM assigns the temporary
SIMD register on Apple arm64 and inlines `FMOV`, `CNT`, `UADDLV`, and `FMOV`
into the loop. A different LLVM target can use the portable `:popcnt` form.

Run the same suite with the VM and the linked executable. The property compares
the bitmap loop with a `NOTINLINE` call to the Lisp definition on four boundary
words and 300 generated words. The other checks cover redefinition and fallback
with already-evaluated arguments. [vop.lisp](vop.lisp)

With LLVM 21 configured for the CLI build, compile and run the arm64 executable
on macOS. The `.ll` file shows `asm` in the bitmap function and the `.o` file
can be inspected with `xcrun llvm-objdump`.

```sh
cargo build --release -p zkcl-cli --features llvm-aot
./target/release/zkcl run examples/vop.lisp
./target/release/zkcl build examples/vop.lisp --backend llvm --release \
  --list-entries --emit-dir target/vop -o target/vop/bitmap
./target/vop/bitmap
xcrun llvm-objdump -d target/vop/vop.o
```

## Foreign calls and embedding

The SQLite example copies borrowed UTF-8 strings inside a row callback. It
prints null pointers as `NULL` and empty strings as `""`. It uses the default
CLI build with FFI and a system SQLite shared library. Set `ZKCL_SQLITE_LIBRARY`
to select a library file. [sqlite.lisp](sqlite.lisp)

```sh
cargo run --release -p zkcl --example embed
```

The Rust host registers a UTF-8 byte counter. Lisp calls it and returns two
values to the host. For `"λ😀"` those values are 2 characters and 6 bytes.
[Rust example](../crates/zkcl/examples/embed.rs) · [C example](../crates/zkcl-capi/examples/embed.c)

## Check and compile

`check` analyzes source without executing it. `compile` writes a `.zkclfasl` bytecode file that `run` can load in a fresh process.

```sh
./target/release/zkcl check examples
./target/release/zkcl compile examples/persistent-tree.lisp target/persistent-tree.zkclfasl
./target/release/zkcl run target/persistent-tree.zkclfasl
```
