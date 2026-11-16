import test from "node:test";
import assert from "node:assert/strict";
import {
  installBuiltins,
  installBuiltinNative,
  BUILTIN_NATIVE_NAMES,
  writeToString,
  LispHashTable,
  StringStream,
} from "../runtime/builtins.mjs";
import {
  NIL,
  T,
  UNBOUND,
  LispSymbol,
  Cons,
  LispCharacter,
  LispString,
  LispArray,
  list,
  toArray,
} from "../runtime/objects.mjs";
import {
  NativeFunction,
  MultipleValues,
  LispCondition,
} from "../runtime/execution.mjs";
import { SingleFloat, DoubleFloat, Ratio } from "../runtime/numbers.mjs";
import { createRuntime } from "../runtime/runtime.mjs";

// Tests the host-facing builtin protocol independently of bytecode lowering.
function runtime() {
  const rt = {
    builtins: new Map(),
    packages: new Map(),
    installers: new Map(),
    specials: new Set(),
    output: new StringStream(),
    makePackage(name, { use = [], nicknames = [] } = {}) {
      const p = {
        name,
        symbols: new Map(),
        exports: new Set(),
        uses: use,
        nicknames: new Set(nicknames),
      };
      for (const n of [name, ...nicknames]) this.packages.set(n, p);
      return p;
    },
    findPackage(value) {
      return typeof value === "object" && value?.symbols instanceof Map
        ? value
        : this.packages.get(value);
    },
    exportSymbol(symbol, pkg = symbol.package) {
      const target = this.findPackage(pkg);
      target.symbols.set(symbol.name, symbol);
      target.exports.add(symbol.name);
      return symbol;
    },
    findSymbol(name, pkg = this.currentPackage) {
      const p = this.findPackage(pkg);
      if (p.symbols.has(name)) return p.symbols.get(name);
      for (const q of p.uses)
        if (q.exports.has(name)) return q.symbols.get(name);
      return null;
    },
    intern(name, pkg = "COMMON-LISP") {
      const p = this.findPackage(pkg);
      let s = this.findSymbol(name, p);
      if (!s) {
        s = new LispSymbol(name, p);
        if (p.name === "KEYWORD") {
          s.value = s;
          s.constant = true;
          p.exports.add(name);
        }
        p.symbols.set(name, s);
      }
      return s;
    },
    defineBuiltin(name, fn, metadata) {
      const f = new NativeFunction(name, fn, metadata);
      this.builtins.set(name, f);
      const s = this.intern(name);
      s.function = f;
      this.findPackage("COMMON-LISP").exports.add(name);
      return f;
    },
    multipleValues(...v) {
      return new MultipleValues(v);
    },
    requestCall(fn, args) {
      return this.call(fn, args);
    },
    call(fn, args = []) {
      if (typeof fn === "string") fn = this.builtins.get(fn);
      if (fn instanceof LispSymbol) fn = this.readFunction(fn);
      const result = fn.invoke(args, this, null);
      if (result && typeof result.next === "function") {
        let state = result.next();
        while (!state.done) state = result.next(state.value);
        return state.value;
      }
      return result;
    },
    fn(name, ...args) {
      return this.call(name, args);
    },
    readFunction(x) {
      const f = x instanceof LispSymbol ? x.function : this.builtins.get(x);
      if (f === UNBOUND || !f)
        throw new LispCondition("UNDEFINED-FUNCTION", "Undefined function");
      return f;
    },
    defineFunction(x, f) {
      if (x instanceof LispSymbol) x.function = f;
      else this.builtins.set(x, f);
      return f;
    },
    checkMutableSymbol(s) {
      if (s.constant)
        throw new LispCondition("PROGRAM-ERROR", "Cannot modify a constant");
    },
    getSymbolValue(s, task) {
      if (task?.dynamic?.has(s)) return task.dynamic.get(s);
      if (s.value === UNBOUND)
        throw new LispCondition("UNBOUND-VARIABLE", "Unbound variable");
      return s.value;
    },
    setSymbolValue(s, v, task) {
      this.checkMutableSymbol(s);
      if (task?.dynamic?.has(s)) task.dynamic.set(s, v);
      else s.value = v;
      return v;
    },
  };
  const cl = rt.makePackage("COMMON-LISP", { nicknames: ["CL"] }),
    keyword = rt.makePackage("KEYWORD");
  cl.symbols.set("NIL", NIL);
  cl.symbols.set("T", T);
  cl.exports.add("NIL");
  cl.exports.add("T");
  rt.currentPackage = rt.makePackage("CL-USER", { use: [cl] });
  return installBuiltins(rt);
}
const s = (text) => new LispString(text),
  ch = (text) => new LispCharacter(text),
  vals = (result) =>
    result instanceof MultipleValues ? result.values : [result];

test("exact arithmetic, numeric type contagion, comparisons, and rounding preserve Lisp values", () => {
  const rt = runtime();
  assert.equal(rt.fn("+", 268435455, 1), 268435456n);
  const ratio = rt.fn("/", 1, 3);
  assert.ok(ratio instanceof Ratio);
  assert.equal(writeToString(rt.fn("+", ratio, ratio, ratio)), "1");
  assert.equal(rt.fn("=", 1, new DoubleFloat(1)), T);
  assert.equal(rt.fn("EQL", 1, new DoubleFloat(1)), NIL);
  assert.deepEqual(vals(rt.fn("FLOOR", -7, 3)), [-3, 2]);
  assert.deepEqual(vals(rt.fn("ROUND", 5, 2)), [2, 1]);
  assert.deepEqual(vals(rt.fn("ROUND", 7, 2)), [4, -1]);
  assert.equal(rt.fn("MOD", -7, 3), 2);
  assert.equal(rt.fn("REM", -7, 3), -1);
  assert.equal(rt.fn("ASH", 1, 100), 1n << 100n);
  assert.equal(rt.fn("ISQRT", (1n << 100n) + 1n), 1n << 50n);
  assert.throws(
    () => rt.fn("+", s("1"), 2),
    (e) => e.className === "TYPE-ERROR",
  );
  assert.throws(
    () => rt.fn("/", 1, 0),
    (e) => e.className === "DIVISION-BY-ZERO",
  );
  assert.equal(rt.fn("NUMBERP", true), NIL);
});

test("list operations preserve explicit tails and destructive cell identity", () => {
  const rt = runtime(),
    tail = list(3, 4),
    head = list(1, 2),
    appended = rt.fn("APPEND", head, tail);
  assert.deepEqual(toArray(appended), [1, 2, 3, 4]);
  assert.equal(rt.fn("NTHCDR", 2, appended), tail);
  assert.notEqual(appended, head);
  assert.equal(rt.fn("CAR", NIL), NIL);
  assert.equal(rt.fn("CDR", NIL), NIL);
  assert.equal(rt.fn("CADDR", appended), 3);
  assert.equal(writeToString(rt.fn("LIST*", 1, 2, 3)), "(1 2 . 3)");
  const first = head,
    second = head.cdr;
  const reversed = rt.fn("NREVERSE", head);
  assert.equal(reversed, second);
  assert.equal(second.cdr, first);
  assert.equal(first.cdr, NIL);
  const circular = new Cons(1);
  circular.cdr = circular;
  assert.equal(rt.fn("LIST-LENGTH", circular), NIL);
  assert.equal(rt.fn("NTHCDR", 1n << 60n, circular), circular);
  assert.equal(rt.fn("NTH", 1n << 60n, appended), NIL);
  assert.equal(writeToString(rt.fn("BUTLAST", new Cons(5, new Cons(6, 7)))), "(5)");
  assert.throws(() => rt.fn("LENGTH", circular), /proper list/);
  assert.throws(
    () => rt.fn("CAR", 12),
    (e) => e.className === "TYPE-ERROR",
  );
  assert.throws(
    () => rt.fn("CONS", 1),
    (e) => e.className === "PROGRAM-ERROR",
  );
});

test("multiple values, APPLY, mapping and REDUCE call actual Lisp functions", () => {
  const rt = runtime(),
    fn = new NativeFunction("PAIR", (a) => rt.multipleValues(a[0], a[0] + 1));
  assert.deepEqual(vals(rt.fn("VALUES")), []);
  assert.deepEqual(vals(rt.fn("FUNCALL", fn, 41)), [41, 42]);
  assert.deepEqual(vals(rt.fn("APPLY", fn, list(41))), [41, 42]);
  assert.deepEqual(toArray(rt.fn("MAPCAR", fn, list(1, 2, 3))), [1, 2, 3]);
  const minus = rt.intern("-");
  assert.equal(rt.fn("REDUCE", minus, list(1, 2, 3)), -4);
  assert.equal(
    rt.fn("REDUCE", minus, list(1, 2, 3), rt.intern("FROM-END", "KEYWORD"), T),
    2,
  );
  assert.throws(() => rt.fn("APPLY", fn, 3), /proper list/);
});

test("strings use Unicode scalar indexing and comparisons return mismatch indices", () => {
  const rt = runtime(),
    text = s("a🦀z");
  assert.equal(rt.fn("LENGTH", text), 3);
  assert.equal(rt.fn("CHAR-CODE", rt.fn("CHAR", text, 1)), 0x1f980);
  rt.fn("(SETF CHAR)", ch("λ"), text, 1);
  assert.equal(text.toString(), "aλz");
  assert.equal(rt.fn("STRING<", s("ab"), s("ac")), 1);
  assert.equal(rt.fn("STRING-EQUAL", s("λ"), s("Λ")), T);
  assert.equal(rt.fn("SUBSEQ", text, 1, 3).toString(), "λz");
  assert.equal(rt.fn("NSTRING-UPCASE", text), text);
  assert.equal(text.toString(), "AΛZ");
  assert.equal(rt.fn("CODE-CHAR", 0xd800), NIL);
  assert.equal(rt.fn("DIGIT-CHAR-P", ch("f"), 16), 15);
  assert.deepEqual(
    vals(
      rt.fn(
        "PARSE-INTEGER",
        s("  -123x"),
        rt.intern("JUNK-ALLOWED", "KEYWORD"),
        T,
      ),
    ),
    [-123, 6],
  );
});

test("row-major arrays, fill pointers and displaced storage share ownership correctly", () => {
  const rt = runtime(),
    a = rt.fn(
      "MAKE-ARRAY",
      list(2, 3),
      rt.intern("INITIAL-ELEMENT", "KEYWORD"),
      0,
    );
  assert.equal(rt.fn("ARRAY-RANK", a), 2);
  assert.deepEqual(toArray(rt.fn("ARRAY-DIMENSIONS", a)), [2, 3]);
  rt.fn("(SETF AREF)", 17, a, 1, 2);
  assert.equal(rt.fn("ROW-MAJOR-AREF", a, 5), 17);
  assert.equal(rt.fn("ARRAY-ROW-MAJOR-INDEX", a, 1, 2), 5);
  assert.equal(rt.fn("ARRAY-IN-BOUNDS-P", a, -1, 0), NIL);
  const vector = rt.fn(
    "MAKE-ARRAY",
    4,
    rt.intern("FILL-POINTER", "KEYWORD"),
    0,
  );
  assert.equal(rt.fn("LENGTH", vector), 0);
  assert.equal(rt.fn("VECTOR-PUSH", 41, vector), 0);
  assert.equal(rt.fn("VECTOR-POP", vector), 41);
  const source = rt.fn("VECTOR", 1, 2, 3),
    view = rt.fn(
      "MAKE-ARRAY",
      2,
      rt.intern("DISPLACED-TO", "KEYWORD"),
      source,
      rt.intern("DISPLACED-INDEX-OFFSET", "KEYWORD"),
      1,
    );
  rt.fn("(SETF AREF)", 99, view, 0);
  assert.equal(rt.fn("AREF", source, 1), 99);
});

test("sequence copying handles overlap, keyword keys, and actual predicate results", () => {
  const rt = runtime(),
    v = rt.fn("VECTOR", 0, 1, 2, 3, 4);
  rt.fn("REPLACE", v, v, rt.intern("START1", "KEYWORD"), 1);
  assert.deepEqual(
    Array.from({ length: 5 }, (_, i) => v.get(i)),
    [0, 0, 1, 2, 3],
  );
  const odd = rt.intern("ODDP"),
    values = list(1, 2, 3, 4);
  assert.equal(rt.fn("POSITION-IF", odd, values), 0);
  assert.equal(rt.fn("COUNT-IF", odd, values), 2);
  assert.deepEqual(toArray(rt.fn("REMOVE-IF", odd, values)), [2, 4]);
  assert.equal(rt.fn("SOME", odd, values), T);
  assert.deepEqual(toArray(rt.fn("SUBSEQ", values, 1, 3)), [2, 3]);
  assert.deepEqual(toArray(values), [1, 2, 3, 4]);
});

test("duplicate removal preserves retained order, types, and destructive list cells", () => {
  const rt = runtime(),
    keyword = (name) => rt.intern(name, "KEYWORD"),
    input = list(9, 1, 2, 1, 9),
    later = input.cdr.cdr.cdr;
  const options = [keyword("START"), 1, keyword("END"), 4];
  const removed = rt.fn("REMOVE-DUPLICATES", input, ...options);
  assert.deepEqual(toArray(removed), [9, 2, 1, 9]);
  assert.deepEqual(toArray(input), [9, 1, 2, 1, 9]);
  assert.equal(rt.fn("DELETE-DUPLICATES", input, ...options), input);
  assert.equal(input.cdr.cdr, later);
  assert.deepEqual(toArray(input), [9, 2, 1, 9]);

  const same = list(list(1, 2), list(2, 3), list(1, 4));
  const kept = rt.fn("REMOVE-DUPLICATES", same, keyword("FROM-END"), T,
    keyword("KEY"), rt.intern("CAR"), keyword("TEST-NOT"), rt.intern("/="));
  assert.deepEqual(toArray(kept), [same.car, same.cdr.car]);
  assert.equal(rt.fn("REMOVE-DUPLICATES", s("🦀a🦀")).toString(), "a🦀");
  const bits = new LispArray([4], { elementType: "BIT", initialContents: [0, 1, 0, 1] });
  assert.equal(writeToString(rt.fn("DELETE-DUPLICATES", bits)), "#*01");
});

test("sequence edits respect visit limits and keep specialized output", () => {
  const rt = runtime(),
    keyword = (name) => rt.intern(name, "KEYWORD"),
    input = list(1, 2, 3, 4);
  const calls = [];
  const testOdd = new NativeFunction("TRACK-ODD", ([value]) => {
    calls.push(value);
    return value % 2 ? T : NIL;
  });
  assert.equal(rt.fn("REMOVE-IF", testOdd, input, keyword("COUNT"), 0), input);
  assert.deepEqual(calls, []);
  assert.equal(rt.fn("NSUBSTITUTE-IF", 7, testOdd, input, keyword("FROM-END"), T,
    keyword("COUNT"), 1), input);
  assert.deepEqual(calls, [4, 3]);
  assert.deepEqual(toArray(input), [1, 2, 7, 4]);
  const bits = rt.fn("MAKE-SEQUENCE", rt.intern("BIT-VECTOR"), 3);
  assert.equal(writeToString(rt.fn("SUBSTITUTE", 1, 0, bits, keyword("COUNT"), 2)), "#*110");
  assert.equal(writeToString(bits), "#*000");
  assert.throws(() => rt.fn("REMOVE-IF", testOdd, input, keyword("TEST"), NIL),
    (e) => e.className === "PROGRAM-ERROR");
});

test("sequence comparison and construction respect bounds and array capacity", () => {
  const rt = runtime(), keyword = (name) => rt.intern(name, "KEYWORD");
  assert.equal(rt.fn("SEARCH", s("λa"), s("xλaλaz"), keyword("START2"), 1,
    keyword("END2"), 5, keyword("FROM-END"), T), 3);
  assert.equal(rt.fn("SEARCH", NIL, list(1, 2, 3), keyword("FROM-END"), T), 3);
  assert.equal(rt.fn("MISMATCH", list(1, 2, 3, 4), list("bad", 3, 4),
    keyword("START1"), 1, keyword("FROM-END"), T), 2);
  assert.equal(writeToString(rt.fn("MERGE", list(rt.intern("BIT-VECTOR"), 4),
    list(0, 1), list(0, 1), rt.intern("<"))), "#*0011");

  const target = new LispArray([4], { fillPointer: 1 });
  assert.equal(rt.fn("MAP-INTO", target, rt.intern("1+"), list(1, 2, 3)), target);
  assert.equal(target.fillPointer, 3);
  assert.deepEqual(target.storage, [2, 3, 4, NIL]);
  const cycle = list(7);
  cycle.cdr = cycle;
  assert.deepEqual(toArray(rt.fn("MAP", rt.intern("LIST"), rt.intern("+"),
    cycle, list(1, 2, 3))), [8, 9, 10]);
});

test("tree substitution can replace an entire subtree or retain its shared tail", () => {
  const rt = runtime(),
    tail = list(2, 3),
    nested = new Cons(list(1), tail),
    replaced = rt.fn("SUBST", 9, list(1), nested, rt.intern("TEST", "KEYWORD"), rt.intern("EQUAL"));
  assert.equal(replaced.car, 9);
  assert.equal(replaced.cdr, tail);
  assert.deepEqual(toArray(nested.car), [1]);
  assert.equal(rt.fn("NSUBLIS", list(new Cons(2, 7)), nested), nested);
  assert.equal(tail.car, 7);
  assert.equal(rt.fn("TREE-EQUAL", nested, list(list(1), 7, 3)), T);
});

test("packages preserve intern status, symbol identities, and exported accessibility", () => {
  const rt = runtime(),
    p = rt.fn("MAKE-PACKAGE", s("TOOLS"));
  const [first, status] = vals(rt.fn("INTERN", s("FOO"), p));
  assert.equal(status, NIL);
  const [again, internal] = vals(rt.fn("INTERN", s("FOO"), p));
  assert.equal(again, first);
  assert.equal(internal.name, "INTERNAL");
  rt.fn("EXPORT", first, p);
  assert.equal(vals(rt.fn("FIND-SYMBOL", s("FOO"), p))[1].name, "EXTERNAL");
  rt.fn("USE-PACKAGE", p, rt.currentPackage);
  assert.equal(rt.findSymbol("FOO", rt.currentPackage), first);
  assert.equal(
    vals(rt.fn("FIND-SYMBOL", s("FOO"), rt.currentPackage))[1].name,
    "INHERITED",
  );
  const uninterned = rt.fn("MAKE-SYMBOL", s("FOO"));
  assert.notEqual(uninterned, first);
  assert.equal(rt.fn("SYMBOL-PACKAGE", uninterned), NIL);
  rt.fn("SET", first, 123);
  assert.equal(rt.fn("SYMBOL-VALUE", first), 123);
  assert.throws(() => rt.fn("SET", T, 1), /constant/);
});

test("package shadow replacement removes the old membership and home", () => {
  const rt = runtime();
  const pkg = rt.fn("MAKE-PACKAGE", s("SHADOW-REPLACEMENT"));
  const spelling = s("ENTRY");
  rt.fn("SHADOW", spelling, pkg);
  const old = vals(rt.fn("FIND-SYMBOL", spelling, pkg))[0];
  rt.fn("EXPORT", old, pkg);
  rt.fn("SHADOWING-IMPORT", old, pkg);
  assert.equal(vals(rt.fn("FIND-SYMBOL", spelling, pkg))[1].name, "EXTERNAL");

  const replacement = rt.fn("MAKE-SYMBOL", spelling);
  assert.equal(rt.fn("SHADOWING-IMPORT", replacement, pkg), T);
  const [found, status] = vals(rt.fn("FIND-SYMBOL", spelling, pkg));
  assert.equal(found, replacement);
  assert.equal(status.name, "INTERNAL");
  assert.equal(rt.fn("SYMBOL-PACKAGE", old), NIL);
  assert.equal(rt.fn("SYMBOL-PACKAGE", replacement), NIL);
  assert.deepEqual(toArray(rt.fn("PACKAGE-SHADOWING-SYMBOLS", pkg)), [replacement]);
});

test("uninterning a shadow preserves it if it hides distinct inherited symbols", () => {
  const rt = runtime();
  const target = rt.fn("MAKE-PACKAGE", s("SHADOW-TARGET"));
  const first = rt.fn("MAKE-PACKAGE", s("SHADOW-FIRST"));
  const second = rt.fn("MAKE-PACKAGE", s("SHADOW-SECOND"));
  const spelling = s("ENTRY");
  const inherited = vals(rt.fn("INTERN", spelling, first))[0];
  const conflicting = vals(rt.fn("INTERN", spelling, second))[0];
  rt.fn("EXPORT", inherited, first);
  rt.fn("EXPORT", conflicting, second);
  rt.fn("SHADOW", spelling, target);
  rt.fn("USE-PACKAGE", list(first, second), target);

  const local = vals(rt.fn("FIND-SYMBOL", spelling, target))[0];
  assert.throws(() => rt.fn("UNINTERN", local, target), error =>
    error.className === "PACKAGE-ERROR" && error.package === target);
  assert.equal(vals(rt.fn("FIND-SYMBOL", spelling, target))[0], local);
  assert.equal(rt.fn("SYMBOL-PACKAGE", local), target);
  assert.deepEqual(toArray(rt.fn("PACKAGE-SHADOWING-SYMBOLS", target)), [local]);

  rt.fn("SHADOWING-IMPORT", inherited, second);
  rt.fn("EXPORT", inherited, second);
  assert.equal(rt.fn("UNINTERN", local, target), T);
  assert.equal(rt.fn("SYMBOL-PACKAGE", local), NIL);
  const [found, status] = vals(rt.fn("FIND-SYMBOL", spelling, target));
  assert.equal(found, inherited);
  assert.equal(status.name, "INHERITED");
});

test("hash tables use requested Lisp equality and preserve presence multiple values", () => {
  const rt = runtime(),
    table = rt.fn(
      "MAKE-HASH-TABLE",
      rt.intern("TEST", "KEYWORD"),
      rt.intern("EQUAL"),
    );
  assert.ok(table instanceof LispHashTable);
  rt.fn("(SETF GETHASH)", NIL, s("same"), table);
  assert.deepEqual(vals(rt.fn("GETHASH", s("same"), table)), [NIL, T]);
  assert.deepEqual(vals(rt.fn("GETHASH", s("absent"), table, 42)), [42, NIL]);
  assert.equal(rt.fn("REMHASH", s("same"), table), T);
  assert.equal(rt.fn("HASH-TABLE-COUNT", table), 0);
});

test("printer, FORMAT and string streams return characters and indices without coercion", () => {
  const rt = createRuntime({ output: new StringStream() });
  rt.fn = (name, ...args) => {
    const result = rt.call(name, args);
    return result.values.length === 1 ? result.values[0] : result;
  };
  const stream = rt.fn("MAKE-STRING-OUTPUT-STREAM");
  assert.equal(
    rt
      .fn("FORMAT", NIL, s("~A ~S ~D ~X~%"), s("hello"), s("hello"), 123, 255)
      .toString(),
    'hello "hello" 123 FF\n',
  );
  rt.fn("FORMAT", stream, s("~A/~D"), s("value"), 42);
  assert.equal(
    rt.fn("GET-OUTPUT-STREAM-STRING", stream).toString(),
    "value/42",
  );
  assert.equal(rt.fn("GET-OUTPUT-STREAM-STRING", stream).toString(), "");
  assert.equal(writeToString(new SingleFloat(1)), "1.0");
  assert.equal(writeToString(new DoubleFloat(1)), "1.0d0");
  assert.equal(writeToString(rt.fn("/", 2, 3)), "2/3");
  const input = rt.fn("MAKE-STRING-INPUT-STREAM", s("🦀\nabc"));
  assert.equal(rt.fn("READ-CHAR", input).codePoint, 0x1f980);
  assert.deepEqual(vals(rt.fn("READ-LINE", input)), [s(""), NIL]);
  assert.equal(vals(rt.fn("READ-LINE", input))[0].toString(), "abc");
  assert.equal(rt.fn("READ-CHAR", input, NIL, 42), 42);
  assert.throws(
    () => rt.fn("FORMAT", NIL, s("~Q"), 1),
    (e) => e.className === "FORMAT-ERROR",
  );
});

test("serialized builtin native identifiers come from installer table and unknown IDs fail", () => {
  const rt = runtime();
  assert.equal(BUILTIN_NATIVE_NAMES[21], "DEFINE-CONSTANT");
  assert.equal(BUILTIN_NATIVE_NAMES[12], undefined);
  const x = rt.intern("ANSWER");
  assert.equal(installBuiltinNative(21, [x, 42], rt), x);
  assert.equal(x.value, 42);
  assert.equal(x.constant, true);
  rt.installers.set(14, () => 123);
  assert.equal(installBuiltinNative(14, [], rt), 123);
  assert.throws(
    () => installBuiltinNative(12, [], rt),
    (e) => e.className === "PROGRAM-ERROR",
  );
  assert.throws(
    () => installBuiltinNative(3, [], rt),
    (e) => e.className === "UNDEFINED-FUNCTION",
  );
});

test("target constants and condition accessors expose Lisp values in a resident runtime", async () => {
  const { createRuntime } = await import("../runtime/runtime.mjs"),
    rt = createRuntime();
  assert.deepEqual(
    rt.evalString(
      "(values most-positive-fixnum most-negative-fixnum char-code-limit array-rank-limit)",
    ).values,
    [268435455, -268435456, 0x110000, 128],
  );
  assert.deepEqual(
    rt.evalString(
      "(handler-case (endp 42) (type-error (c) (values (eq (type-error-expected-type c) 'list) (typep 42 (type-error-expected-type c)))))",
    ).values,
    [T, NIL],
  );
  assert.deepEqual(
    rt
      .evalString(
        "(handler-case (error 'type-error :datum 'foo :expected-type nil) (type-error (c) (values (type-error-datum c) (type-error-expected-type c) (typep c 'error))))",
      )
      .values.map((x) => x.name ?? x),
    ["FOO", "NIL", "T"],
  );
  assert.deepEqual(
    rt.evalString("(let ((x (gensym))) (progv (list x) nil (boundp x)))")
      .values,
    [NIL],
  );
});

test("two-way streams route Lisp input and output without closing the underlying streams", async () => {
  const { createRuntime } = await import("../runtime/runtime.mjs"),
    rt = createRuntime();
  const result = rt.evalString(`
    (let* ((input (make-string-input-stream "a 41"))
           (output (make-string-output-stream))
           (stream (make-two-way-stream input output)))
      (let ((*terminal-io* stream))
        (write-char (read-char t) t)
        (format stream "~D" (1+ (read t))))
      (close stream)
      (values (type-of stream) (get-output-stream-string output)
              (open-stream-p stream) (open-stream-p input) (open-stream-p output)
              (eq input (two-way-stream-input-stream stream))
              (eq output (two-way-stream-output-stream stream))))`).values;
  assert.deepEqual(result.map(value => value.name ?? value.toString()),
    ["TWO-WAY-STREAM", "a42", "NIL", "T", "T", "T", "T"]);
});

test("NIL and T property lists belong to the runtime while symbol identities stay canonical", async () => {
  const { createRuntime } = await import("../runtime/runtime.mjs"),
    left = createRuntime(),
    right = createRuntime();
  assert.deepEqual(
    left.evalString("(setf (get nil :private) 17 (get t :private) 42)").values,
    [42],
  );
  assert.deepEqual(
    left.evalString("(values (get nil :private) (get t :private))").values,
    [17, 42],
  );
  assert.deepEqual(
    right.evalString("(values (get nil :private) (get t :private))").values,
    [NIL, NIL],
  );
  assert.equal(
    left.intern("NIL", "COMMON-LISP"),
    right.intern("NIL", "COMMON-LISP"),
  );
  assert.deepEqual(
    left.evalString("(values (remprop nil :private) (get nil :private))")
      .values,
    [T, NIL],
  );
});

test("printer reads task bindings and preserves circular/shared graphs on reread", async () => {
  const { createRuntime } = await import("../runtime/runtime.mjs"),
    rt = createRuntime();
  const result = rt.evalString(
    "(let ((*print-base* 16) (*print-case* :downcase)) (write-to-string (list 255 'hello)))",
  );
  assert.equal(result.values[0].toString(), "(FF hello)");
  const printed = rt.evalString(
    "(let ((x (list 1))) (setf (cdr x) x) (write-to-string x :circle t))",
  ).values[0];
  assert.equal(printed.toString(), "#1=(1 . #1#)");
  const reread = rt.readFromString(printed.toString())[0];
  assert.equal(reread.cdr, reread);
  assert.equal(
    rt
      .evalString("(write-to-string '(1 (2 3) 4) :level 1 :length 2)")
      .values[0].toString(),
    "(1 # ...)",
  );
  const shared = list(2, 3),
    graph = list(shared, shared);
  assert.match(writeToString(graph, { circle: true }), /#1=\(2 3\).*#1#/u);
  assert.equal(
    rt
      .evalString("(with-standard-io-syntax (write-to-string 255))")
      .values[0].toString(),
    "255",
  );
});

test("callback builtins resume after asynchronous Lisp calls with their native continuation intact", async () => {
  const { createRuntime } = await import("../runtime/runtime.mjs"),
    rt = createRuntime();
  const increment = rt.registerFunction("CL-USER::ASYNC-INCREMENT", ([x]) =>
    Promise.resolve(x + 1),
  );
  const add = rt.registerFunction("CL-USER::ASYNC-ADD", ([x, y]) =>
    Promise.resolve(x + y),
  );
  const less = rt.registerFunction("CL-USER::ASYNC-LESS", ([x, y]) =>
    Promise.resolve(x < y ? T : NIL),
  );
  const odd = rt.registerFunction("CL-USER::ASYNC-ODD", ([x]) =>
    Promise.resolve(x % 2 ? T : NIL),
  );
  const mapped = await rt.callAsync("MAPCAR", [increment, list(1, 2, 3)]);
  assert.deepEqual(toArray(mapped.values[0]), [2, 3, 4]);
  assert.deepEqual(
    (await rt.callAsync("REDUCE", [add, list(1, 2, 3, 4)])).values,
    [10],
  );
  assert.deepEqual(
    toArray(
      (await rt.callAsync("STABLE-SORT", [list(3, 1, 2), less])).values[0],
    ),
    [1, 2, 3],
  );
  const unique = await rt.callAsync("REMOVE-DUPLICATES", [
    list(1, 2, 3, 4), rt.intern("KEY", "KEYWORD"), odd,
  ]);
  assert.deepEqual(toArray(unique.values[0]), [3, 4]);
  assert.deepEqual(
    (
      await rt.callAsync("TYPEP", [
        3,
        list(rt.intern("SATISFIES", "COMMON-LISP"), odd),
      ])
    ).values,
    [T],
  );
});
