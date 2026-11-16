import * as O from "./objects.mjs";
import {
  NIL, T, UNBOUND, Cons, LispSymbol, LispCharacter, LispArray,
  list, fromArray, toArray, eq, eql, equal,
} from "./objects.mjs";
import * as N from "./numbers.mjs";
import { LispHashTable, hashTests, equalp, sxhash } from "./hash_tables.mjs";
import { NativeFunction, LispCondition, LispRestart } from "./execution.mjs";
import {
  truth, fail, typeError, isString, string, name, integer, naturalInteger, index,
  booleOperations, requestedValue, arity, keys, car, cdr, cells, listSpine, nthTail,
  sequence, sequenceResult, dynamicValue,
} from "./builtins_helpers.mjs";
import {
  commonLispName, complexMask, requestedTypep, expandedTypep, atomicTypep, nameSafe,
  bridgeCondition, coerceCondition, nonClassTypeNames,
} from "./types.mjs";
import { installBuiltinNative } from "./installers.mjs";
import { installNumbers } from "./builtins_numbers.mjs";
import { installSequences, installTreeOperations } from "./builtins_sequences.mjs";
import { installArrays } from "./builtins_arrays.mjs";
import { installCharacters } from "./builtins_chars.mjs";
import { installSymbols } from "./builtins_symbols.mjs";
import { installPrinting } from "./builtins_printing.mjs";

export { StringStream } from "./file_streams.mjs";
export { LispHashTable, equalp };
export { keys, PprintDispatchTable } from "./builtins_helpers.mjs";
export { writeToString } from "./printer.mjs";
export { BUILTIN_NATIVE_NAMES, installBuiltinNative } from "./installers.mjs";

export function installBuiltins(rt) {
  const constants = {
    ...Object.fromEntries(booleOperations.map(([op], code) => [`BOOLE-${op}`, code])),
    "MOST-POSITIVE-FIXNUM": N.FIXNUM_MAX,
    "MOST-NEGATIVE-FIXNUM": N.FIXNUM_MIN,
    "ARRAY-RANK-LIMIT": O.ARRAY_RANK_LIMIT,
    "ARRAY-DIMENSION-LIMIT": N.integer(O.ARRAY_DIMENSION_LIMIT),
    "ARRAY-TOTAL-SIZE-LIMIT": N.integer(O.ARRAY_TOTAL_SIZE_LIMIT),
    "CHAR-CODE-LIMIT": 0x110000,
    "CALL-ARGUMENTS-LIMIT": 65536,
    "LAMBDA-PARAMETERS-LIMIT": 65536,
    "MULTIPLE-VALUES-LIMIT": 65536,
    "INTERNAL-TIME-UNITS-PER-SECOND": 1000,
    PI: new N.DoubleFloat(Math.PI),
  };
  for (const format of ["SHORT", "SINGLE", "DOUBLE", "LONG"]) {
    const single = format === "SHORT" || format === "SINGLE",
      Float = single ? N.SingleFloat : N.DoubleFloat,
      max = single ? 3.4028234663852886e38 : Number.MAX_VALUE,
      min = single ? 1.401298464324817e-45 : Number.MIN_VALUE,
      normal = single ? 1.1754943508222875e-38 : 2.2250738585072014e-308;
    constants[`MOST-POSITIVE-${format}-FLOAT`] = new Float(max);
    constants[`MOST-NEGATIVE-${format}-FLOAT`] = new Float(-max);
    constants[`LEAST-POSITIVE-${format}-FLOAT`] = new Float(min);
    constants[`LEAST-NEGATIVE-${format}-FLOAT`] = new Float(-min);
    constants[`LEAST-POSITIVE-NORMALIZED-${format}-FLOAT`] = new Float(normal);
    constants[`LEAST-NEGATIVE-NORMALIZED-${format}-FLOAT`] = new Float(-normal);
    constants[`${format}-FLOAT-EPSILON`] = new Float(2 ** (single ? -23 : -52));
    constants[`${format}-FLOAT-NEGATIVE-EPSILON`] = new Float(
      2 ** (single ? -24 : -53),
    );
  }
  for (const [spelling, value] of Object.entries(constants)) {
    const symbol = rt.intern(spelling, "COMMON-LISP");
    symbol.value = value;
    symbol.constant = true;
    rt.findPackage("COMMON-LISP").exports.add(spelling);
  }
  const def = (n, min, max, fn) =>
    rt.defineBuiltin(
      n,
      (a, r, t) => {
        arity(a, min, max);
        try {
          const result = fn(a, r, t);
          if (result && typeof result.next === "function")
            return (function* () {
              try {
                return yield* result;
              } catch (error) {
                throw bridgeCondition(error, r, n, a);
              }
            })();
          return result;
        } catch (e) {
          throw bridgeCondition(e, r, n, a);
        }
      },
      { primitive: n },
    );
  const unary = (n, fn) => def(n, 1, 1, (a) => fn(a[0]));
  const pred = (n, fn) => unary(n, (x) => truth(fn(x)));
  def("IDENTITY", 1, 1, (a) => a[0]);
  def("VALUES", 0, Infinity, (a) => rt.multipleValues(...a));
  def("VALUES-LIST", 1, 1, (a) => rt.multipleValues(...toArray(a[0])));
  for (const [n, fn] of [
    ["EQ", eq],
    ["EQL", eql],
    ["EQUAL", equal],
  ])
    def(n, 2, 2, (a) => truth(fn(...a)));
  def("EQUALP", 2, 2, (a) => truth(equalp(...a)));
  def("FUNCALL", 1, Infinity, (a, r) => r.requestCall(a[0], a.slice(1)));
  def("APPLY", 2, Infinity, (a, r) =>
    r.requestCall(a[0], [...a.slice(1, -1), ...toArray(a.at(-1))]),
  );
  def("CONSTANTLY", 1, 1, (a) => new NativeFunction("CONSTANTLY", () => a[0]));
  def(
    "COMPLEMENT",
    1,
    1,
    (a) =>
      new NativeFunction("COMPLEMENT", function* (xs, r) {
        return truth((yield* requestedValue(r, a[0], xs)) === NIL);
      }),
  );
  def("CONS", 2, 2, (a) => new Cons(...a));
  def("LIST", 0, Infinity, (a) => fromArray(a));
  def("LIST*", 1, Infinity, (a) => fromArray(a.slice(0, -1), a.at(-1)));
  for (let len = 1; len <= 4; len++)
    for (let bits = 0; bits < 1 << len; bits++) {
      const letters = Array.from({ length: len }, (_, i) =>
        (bits >> i) & 1 ? "D" : "A",
      ).join("");
      unary(`C${letters}R`, (x) => {
        for (const c of [...letters].reverse()) x = c === "A" ? car(x) : cdr(x);
        return x;
      });
    }
  [
    "FIRST",
    "SECOND",
    "THIRD",
    "FOURTH",
    "FIFTH",
    "SIXTH",
    "SEVENTH",
    "EIGHTH",
    "NINTH",
    "TENTH",
  ].forEach((n, i) => unary(n, (x) => car(nthTail(x, i))));
  unary("REST", cdr);
  pred("CONSP", (x) => x instanceof Cons);
  pred("ATOM", (x) => !(x instanceof Cons));
  pred("LISTP", (x) => x === NIL || x instanceof Cons);
  pred("NULL", (x) => x === NIL);
  pred("NOT", (x) => x === NIL);
  unary("ENDP", (x) => {
    if (x !== NIL && !(x instanceof Cons)) typeError(x, "list");
    return truth(x === NIL);
  });
  def("RPLACA", 2, 2, (a) => {
    if (!(a[0] instanceof Cons)) typeError(a[0], "cons");
    a[0].car = a[1];
    return a[0];
  });
  def("RPLACD", 2, 2, (a) => {
    if (!(a[0] instanceof Cons)) typeError(a[0], "cons");
    a[0].cdr = a[1];
    return a[0];
  });
  def("NTH", 2, 2, (a) => car(nthTail(a[1], naturalInteger(a[0]))));
  def("NTHCDR", 2, 2, (a) => nthTail(a[1], naturalInteger(a[0])));
  def("MAKE-LIST", 1, Infinity, (a) => {
    const k = keys(a.slice(1), ["INITIAL-ELEMENT"]);
    return fromArray(Array(index(a[0])).fill(k.get("INITIAL-ELEMENT") ?? NIL));
  });
  def("APPEND", 0, Infinity, (a) => {
    let result = a.at(-1) ?? NIL;
    for (let i = a.length - 2; i >= 0; i--)
      result = fromArray(toArray(a[i]), result);
    return result;
  });
  unary("COPY-LIST", (x) => {
    const [spine, tail] = listSpine(x);
    return fromArray(
      spine.map((c) => c.car),
      tail,
    );
  });
  const copyTree = (x) =>
    x instanceof Cons ? new Cons(copyTree(x.car), copyTree(x.cdr)) : x;
  unary("COPY-TREE", copyTree);
  unary("COPY-ALIST", (x) =>
    fromArray(
      toArray(x).map((v) => (v instanceof Cons ? new Cons(v.car, v.cdr) : v)),
    ),
  );
  def("ACONS", 3, 3, (a) => new Cons(new Cons(a[0], a[1]), a[2]));
  def("PAIRLIS", 2, 3, (a) => {
    const ks = toArray(a[0]),
      vs = toArray(a[1]);
    if (ks.length !== vs.length)
      fail("PROGRAM-ERROR", "PAIRLIS lengths differ");
    let out = a[2] ?? NIL;
    ks.forEach((k, i) => (out = new Cons(new Cons(k, vs[i]), out)));
    return out;
  });
  def("REVAPPEND", 2, 2, (a) => {
    let out = a[1];
    for (const x of toArray(a[0])) out = new Cons(x, out);
    return out;
  });
  def("NRECONC", 2, 2, (a) => {
    let tail = a[1];
    for (const cell of cells(a[0])) {
      cell.cdr = tail;
      tail = cell;
    }
    return tail;
  });
  def("NCONC", 0, Infinity, (a) => {
    let result = NIL,
      last;
    a.forEach((x, i) => {
      if (x === NIL) return;
      if (last) last.cdr = x;
      else result = x;
      if (i < a.length - 1) {
        const spine = cells(x);
        last = spine.at(-1);
      }
    });
    return result;
  });
  def("LAST", 1, 2, (a) => {
    const [cs, tail] = listSpine(a[0]),
      n = naturalInteger(a[1] ?? 1);
    if (n === 0n) return tail;
    if (n >= BigInt(cs.length)) return a[0];
    return cs[cs.length - Number(n)];
  });
  for (const destructive of [false, true])
    def(destructive ? "NBUTLAST" : "BUTLAST", 1, 2, (a) => {
      const [cs] = listSpine(a[0]);
      const n = naturalInteger(a[1] ?? 1);
      if (n >= BigInt(cs.length)) return NIL;
      const len = cs.length - Number(n);
      if (destructive) {
        cs[len - 1].cdr = NIL;
        return a[0];
      }
      return fromArray(cs.slice(0, len).map((c) => c.car));
    });
  unary("LIST-LENGTH", (x) => {
    let slow = x,
      fast = x,
      n = 0;
    while (fast instanceof Cons) {
      fast = fast.cdr;
      n++;
      if (!(fast instanceof Cons)) break;
      fast = fast.cdr;
      n++;
      slow = slow.cdr;
      if (fast === slow) return NIL;
    }
    if (fast !== NIL) typeError(fast, "proper list");
    return n;
  });
  def("TAILP", 2, 2, (a) => {
    let x = a[1];
    const seen = new Set();
    while (x instanceof Cons) {
      if (eql(a[0], x)) return T;
      if (seen.has(x)) return NIL;
      seen.add(x);
      x = x.cdr;
    }
    return truth(eql(a[0], x));
  });
  def("LDIFF", 2, 2, (a) => {
    const out = [];
    let x = a[0];
    while (x instanceof Cons && !eql(x, a[1])) {
      out.push(x.car);
      x = x.cdr;
    }
    return fromArray(out, eql(x, a[1]) ? NIL : x);
  });
  installNumbers(rt, def, unary, pred);
  installSequences(def, unary);
  installTreeOperations(def);
  installArrays(rt, def, unary, pred);
  installCharacters(rt, def, unary, pred);
  installSymbols(rt, def, unary, pred);
  installPrinting(rt, def, unary, pred);
  for (const name of nonClassTypeNames) rt.exportSymbol(rt.intern(name, 'COMMON-LISP'), 'COMMON-LISP');
  def("TYPEP", 2, 3, function* (a, r) {
    return truth(yield* requestedTypep(a[0], a[1], r, a[2] ?? NIL));
  });
  unary("TYPE-OF", (value) => {
    const cl = (name) => rt.intern(name, "COMMON-LISP");
    if (N.isInteger(value)) {
      if (value === 0 || value === 1) return cl("BIT");
      if (typeof value === "number") {
        if (value < 0) return cl("FIXNUM");
        return list(cl("INTEGER"), 0, N.FIXNUM_MAX);
      }
      if (value < 0n) return cl("BIGNUM");
      return list(cl("INTEGER"), list(N.FIXNUM_MAX));
    }
    if (value instanceof N.Complex) {
      const part = value.real instanceof N.DoubleFloat ? "DOUBLE-FLOAT"
        : value.real instanceof N.SingleFloat ? "SINGLE-FLOAT" : "RATIONAL";
      return list(cl("COMPLEX"), cl(part));
    }
    if (N.isFloat(value)) return cl(value instanceof N.DoubleFloat ? "DOUBLE-FLOAT" : "SINGLE-FLOAT");
    if (value instanceof N.Ratio) return cl("RATIO");
    if (rt.classOf) {
      const cls = rt.classOf(value);
      return cls.name !== NIL && rt.findClass(cls.name, false) === cls ? cls.name : cls;
    }
    if (value === NIL) return cl("NULL");
    if (value instanceof Cons) return cl("CONS");
    if (value instanceof LispSymbol) return cl("SYMBOL");
    if (isString(value)) return cl("STRING");
    return T;
  });
  def("SUBTYPEP", 2, 3, function* (a, r) {
    const environment = a[2] ?? NIL;
    const [sub] = yield* r.expandTypeSteps(a[0], environment);
    const [sup] = yield* r.expandTypeSteps(a[1], environment);
    if (equal(sub, sup) || sub === NIL || sup === T)
      return rt.multipleValues(T, T);
    const subComplex = yield* complexMask(sub, r, environment);
    if (subComplex !== null) {
      const supComplex = yield* complexMask(sup, r, environment);
      if (supComplex !== null) return rt.multipleValues(truth((subComplex & ~supComplex) === 0), T);
      if (commonLispName(sup, r) === "NUMBER") return rt.multipleValues(T, T);
    }
    const child = rt.findClassInEnvironment(sub, false, environment),
      parent = rt.findClassInEnvironment(sup, false, environment);
    if (child && parent)
      return rt.multipleValues(truth(rt.closPrecedence(child, rt.activeTask).includes(parent)), T);
    const ancestry = {
      NULL: ["LIST", "SYMBOL", "BOOLEAN"],
      CONS: ["LIST"],
      LIST: ["SEQUENCE"],
      KEYWORD: ["SYMBOL"],
      FIXNUM: ["INTEGER"],
      BIGNUM: ["INTEGER"],
      INTEGER: ["RATIONAL"],
      RATIO: ["RATIONAL"],
      RATIONAL: ["REAL"],
      REAL: ["NUMBER"],
      FLOAT: ["REAL"],
      "SHORT-FLOAT": ["SINGLE-FLOAT"],
      "SINGLE-FLOAT": ["FLOAT"],
      "LONG-FLOAT": ["DOUBLE-FLOAT"],
      "DOUBLE-FLOAT": ["FLOAT"],
      COMPLEX: ["NUMBER"],
      "STANDARD-CHAR": ["BASE-CHAR"],
      "BASE-CHAR": ["CHARACTER"],
      STRING: ["VECTOR"],
      VECTOR: ["ARRAY", "SEQUENCE"],
      "SIMPLE-VECTOR": ["VECTOR", "SIMPLE-ARRAY"],
      "SIMPLE-STRING": ["STRING", "SIMPLE-ARRAY"],
      "BASE-STRING": ["STRING"],
      "SIMPLE-BASE-STRING": ["BASE-STRING", "SIMPLE-STRING"],
      "BIT-VECTOR": ["VECTOR"],
      "SIMPLE-BIT-VECTOR": ["BIT-VECTOR", "SIMPLE-ARRAY"],
      "SIMPLE-ARRAY": ["ARRAY"],
      "COMPILED-FUNCTION": ["FUNCTION"],
    };
    const ancestors = (type, seen = new Set()) => {
      if (seen.has(type)) return seen;
      seen.add(type);
      for (const p of ancestry[type] ?? []) ancestors(p, seen);
      return seen;
    };
    const x = commonLispName(sub, r),
      y = commonLispName(sup, r),
      known = new Set([
        "T",
        "NIL",
        ...Object.keys(ancestry),
        ...Object.values(ancestry).flat(),
      ]);
    return known.has(x) && known.has(y)
      ? rt.multipleValues(truth(ancestors(x).has(y)), T)
      : rt.multipleValues(NIL, NIL);
  });
  def("COERCE", 2, 2, function* (a, r) {
    const value = a[0],
      [spec] = yield* r.expandTypeSteps(a[1]),
      parts = spec instanceof Cons ? toArray(spec) : [spec],
      target = name(parts[0]);
    if (yield* expandedTypep(value, spec, r, NIL)) return value;
    if (
      [
        "FLOAT",
        "SHORT-FLOAT",
        "SINGLE-FLOAT",
        "DOUBLE-FLOAT",
        "LONG-FLOAT",
      ].includes(target)
    )
      return N.coerceFloat(
        value,
        target === "DOUBLE-FLOAT" || target === "LONG-FLOAT" ? 2 : 1,
      );
    if (target === "CHARACTER") {
      const text = name(value);
      if ([...text].length === 1) return new LispCharacter(text);
      return typeError(value, "character designator");
    }
    if (target === "FUNCTION") {
      if (value instanceof LispSymbol || typeof value === "string")
        return r.readFunction(value);
      if (value instanceof Cons && nameSafe(value.car) === "LAMBDA")
        return yield* r.compileSteps(value);
      return typeError(value, "function designator");
    }
    if (target === "COMPLEX") {
      if (!N.isReal(value)) typeError(value, "real");
      const part = parts[1];
      return N.makeComplex(
        part === undefined
          ? value
          : yield* requestedValue(r, r.intern("COERCE", "COMMON-LISP"), [value, part]),
        0,
      );
    }
    if (
      [
        "LIST",
        "VECTOR",
        "SIMPLE-VECTOR",
        "STRING",
        "SIMPLE-STRING",
        "BASE-STRING",
        "SIMPLE-BASE-STRING",
        "BIT-VECTOR",
        "SIMPLE-BIT-VECTOR",
      ].includes(target)
    ) {
      const source = sequence(value),
        values = Array.from({ length: source.length }, (_, i) => source.get(i));
      if (target === "LIST") return fromArray(values);
      if (target.includes("STRING")) return sequenceResult("string", values);
      const elementType = target.includes("BIT-VECTOR")
        ? "BIT"
        : target === "VECTOR" &&
            parts[1] !== undefined &&
            nameSafe(parts[1]) !== "*"
          ? parts[1]
          : "T";
      const size = target === "VECTOR" ? parts[2] : parts[1];
      if (
        size !== undefined &&
        nameSafe(size) !== "*" &&
        index(size) !== values.length
      )
        typeError(value, "sequence of requested length");
      return new LispArray([values.length], {
        elementType,
        initialContents: values,
      });
    }
    return typeError(value, `coercible to ${target}`);
  });
  for (const n of [
    "SYMBOL",
    "KEYWORD",
    "NUMBER",
    "REAL",
    "RATIONAL",
    "INTEGER",
    "FLOAT",
    "COMPLEX",
    "CHARACTER",
    "STRING",
    "ARRAY",
    "VECTOR",
    "FUNCTION",
    "COMPILED-FUNCTION",
    "PACKAGE",
    "STREAM",
  ])
    pred(
      n === "COMPILED-FUNCTION"
        ? "COMPILED-FUNCTION-P"
        : n === "PACKAGE"
          ? "PACKAGEP"
          : n === "STREAM"
            ? "STREAMP"
            : `${n}P`,
      (x) => atomicTypep(x, rt.intern(n, "COMMON-LISP"), rt),
    );
  def("EVAL", 1, 1, (a, r) => r.evalSteps(a[0]));
  def("COMPILE", 1, 2, function* (a, r) {
    const macro = a[0] === NIL ? NIL : r.macroFunction(a[0]),
      isMacro = macro !== NIL && macro !== undefined;
    const result =
      a.length === 1
        ? yield* r.compileSteps(isMacro ? macro : r.readFunction(a[0]))
        : yield* r.compileSteps(a[1]);
    if (a[0] !== NIL) r.defineFunction(a[0], result, isMacro);
    return r.multipleValues(a[0] === NIL ? result : a[0], NIL, NIL);
  });
  for (const once of [false, true])
    def(once ? "MACROEXPAND-1" : "MACROEXPAND", 1, 2, function* (a, r) {
      const [v, expanded] = yield* r.macroexpandSteps(a[0], a[1] ?? NIL, once);
      return r.multipleValues(v, truth(expanded));
    });
  def(
    "MACRO-FUNCTION",
    1,
    2,
    (a, r) => r.macroFunction(a[0], a[1] ?? NIL) ?? NIL,
  );
  def("GET-SETF-EXPANSION", 1, 2, function* (a, r) {
    return r.multipleValues(...(yield* r.getSetfExpansionSteps(a[0], a[1] ?? NIL)));
  });
  def("FUNCTION-LAMBDA-EXPRESSION", 1, 1, (a) => {
    const fn = a[0];
    if (!atomicTypep(fn, rt.intern("FUNCTION", "COMMON-LISP"), rt))
      typeError(fn, "function");
    const code =
        fn instanceof O.Closure
          ? fn.code
          : fn instanceof O.LispCode
            ? fn
            : null,
      source = code?.entry?.debug?.source_expression;
    const expression =
      source == null
        ? NIL
        : typeof source === "number"
          ? code.constants[source]
          : source;
    return rt.multipleValues(
      expression ?? NIL,
      truth(fn instanceof O.Closure && fn.environment.length > 0),
      fn instanceof NativeFunction ? rt.intern(fn.name, "COMMON-LISP") : NIL,
    );
  });
  def("ZK:FUNCTION-LAMBDA-LIST", 1, 1, (a) => {
    const fn = rt.resolveFunction ? rt.resolveFunction(a[0]) : a[0],
      code =
        fn instanceof O.Closure
          ? fn.code
          : fn instanceof O.LispCode
            ? fn
            : null,
      lambda = code?.entry?.lambda_list;
    if (lambda == null)
      fail("PROGRAM-ERROR", "Lambda list is unavailable for this function");
    return typeof lambda === "number" ? code.constants[lambda] : lambda;
  });
  const specialOperators = new Set(
    "BLOCK CATCH EVAL-WHEN FLET FUNCTION GO IF LABELS LET LET* LOAD-TIME-VALUE LOCALLY MACROLET MULTIPLE-VALUE-CALL MULTIPLE-VALUE-PROG1 PROGN PROGV QUOTE RETURN-FROM SETQ SYMBOL-MACROLET TAGBODY THE THROW UNWIND-PROTECT"
      .split(" ").map(name => rt.intern(name, "COMMON-LISP")),
  );
  rt.isSpecialOperator = name => specialOperators.has(name);
  pred("SPECIAL-OPERATOR-P", rt.isSpecialOperator);
  def("%%IN-PACKAGE", 1, 1, (a, r, t) => installBuiltinNative(0, a, r, t));
  const hashTable = value => value instanceof LispHashTable ? value : typeError(value, "hash-table");
  const hashTableStep = def("%%HASH-TABLE-STEP", 2, 2, ([value, cursor]) => {
    const table = hashTable(value);
    if (cursor === -1) return table;
    if (cursor === 0) cursor = table.entries();
    const entry = cursor.next();
    return entry.done ? NIL : rt.multipleValues(cursor, ...entry.value);
  });
  rt.findPackage("COMMON-LISP").exports.delete("%%HASH-TABLE-STEP");
  rt.installers?.set(19, (args, runtime, task) => hashTableStep.invoke(args, runtime, task));
  unary("SXHASH", sxhash);
  def("MAKE-HASH-TABLE", 0, Infinity, (a) => {
    const k = keys(a, ["TEST", "SIZE", "REHASH-SIZE", "REHASH-THRESHOLD"]),
      test = k.get("TEST") ?? rt.intern("EQL", "COMMON-LISP");
    const named =
      Object.keys(hashTests).find(n => test === rt.intern(n, "COMMON-LISP") ||
        test === (rt.primitives?.get(n) ?? rt.builtins?.get(n)));
    if (!named) typeError(test, "hash table test");
    const table = new LispHashTable(named);
    table.size = index(k.get("SIZE") ?? 16);
    table.rehashSize = k.get("REHASH-SIZE") ?? new N.SingleFloat(1.5);
    if (
      N.isInteger(table.rehashSize)
        ? integer(table.rehashSize) <= 0n
        : !N.isFloat(table.rehashSize) || N.compare(table.rehashSize, 1) <= 0
    )
      typeError(table.rehashSize, "positive integer or float greater than one");
    table.rehashThreshold = k.get("REHASH-THRESHOLD") ?? new N.SingleFloat(1);
    if (
      !N.isReal(table.rehashThreshold) ||
      N.compare(table.rehashThreshold, 0) < 0 ||
      N.compare(table.rehashThreshold, 1) > 0
    )
      typeError(table.rehashThreshold, "real between zero and one");
    return table;
  });
  def("GETHASH", 2, 3, (a) => {
    return rt.multipleValues(...hashTable(a[1]).get(a[0], a[2] ?? NIL));
  });
  def("(SETF GETHASH)", 3, 4, (a) => {
    return hashTable(a[2]).set(a[1], a[0]);
  });
  def("REMHASH", 2, 2, (a) => truth(hashTable(a[1]).delete(a[0])));
  unary("CLRHASH", x => hashTable(x).clear());
  unary("HASH-TABLE-COUNT", x => hashTable(x).count);
  pred("HASH-TABLE-P", (x) => x instanceof LispHashTable);
  unary("HASH-TABLE-TEST", x => rt.intern(hashTable(x).testName, "COMMON-LISP"));
  unary("HASH-TABLE-SIZE", (x) =>
    Math.max(1, hashTable(x).size ?? 16, x.count),
  );
  unary("HASH-TABLE-REHASH-SIZE", (x) => hashTable(x).rehashSize);
  unary("HASH-TABLE-REHASH-THRESHOLD", (x) => hashTable(x).rehashThreshold);
  def("MAPHASH", 2, 2, function* (a, r) {
    for (const [k, v] of hashTable(a[1]).entries())
      yield* requestedValue(r, a[0], [k, v]);
    return NIL;
  });
  def("ERROR", 1, Infinity, function* (a, r, t) {
    const condition = yield* coerceCondition(r, a[0], a.slice(1), "SIMPLE-ERROR", t);
    t.associateSignalingCall(condition);
    throw condition;
  });
  def("MAKE-CONDITION", 1, Infinity, (a, r, t) =>
    r.makeConditionSteps(a[0], a.slice(1), t),
  );
  def("SIGNAL", 1, Infinity, function* (a, r, t) {
    const condition = yield* coerceCondition(r, a[0], a.slice(1), "SIMPLE-CONDITION", t);
    t.associateSignalingCall(condition);
    return yield* r.signal(condition, t);
  });
  const conditionFormat = rt.intern("FORMAT", "COMMON-LISP"), warningClass = rt.intern("WARNING", "COMMON-LISP");
  const styleWarningClass = rt.intern("STYLE-WARNING", "COMMON-LISP");
  const continueName = rt.intern("CONTINUE", "COMMON-LISP"), muffleName = rt.intern("MUFFLE-WARNING", "COMMON-LISP");
  def("CERROR", 2, Infinity, function* ([control, designator, ...args], r, t) {
    const condition = designator instanceof LispCondition ? designator
      : yield* coerceCondition(r, designator, args, "SIMPLE-ERROR", t);
    const report = new NativeFunction("REPORT-CONTINUE", ([stream]) => r.requestCall(conditionFormat, [stream, control, ...args]));
    t.associateSignalingCall(condition);
    t.enterNativeRestart(condition, continueName, report);
    throw condition;
  });
  const warningFormat = string("~&; Warning: ~A~%");
  def("WARN", 1, Infinity, function* ([designator, ...args], r, t) {
    const condition = yield* coerceCondition(r, designator, args, "SIMPLE-WARNING", t);
    if (!r.isInstanceOf(condition, warningClass, t))
      fail("TYPE-ERROR", "Expected a warning", { datum: condition, expectedType: warningClass });
    let nonStyle;
    for (const scope of t.scopes) {
      if (scope.kind !== 'compile-file-diagnostics') continue;
      nonStyle ??= !r.isInstanceOf(condition, styleWarningClass, t);
      scope.warnings = T;
      if (nonStyle) scope.failure = T;
    }
    t.associateSignalingCall(condition);
    t.enterNativeRestart(condition, muffleName, string("Suppress this warning."));
    yield* r.signal(condition, t);
    t.popScope("association");
    t.popScope("restarts");
    const stream = dynamicValue(r, t, "*ERROR-OUTPUT*", r.output);
    yield r.requestCall(conditionFormat, [stream, warningFormat, condition]);
    return NIL;
  });
  const findRestart = function* (designator, condition, r, t) {
    return (yield* r.findRestartsSteps(condition, t)).find(x => x === designator || x.name === designator) ?? NIL;
  };
  def("FIND-RESTART", 1, 2, (a, r, t) => findRestart(a[0], a[1] ?? NIL, r, t));
  def("COMPUTE-RESTARTS", 0, 1, function* (a, r, t) {
    return fromArray(yield* r.findRestartsSteps(a[0] ?? NIL, t));
  });
  def("INVOKE-RESTART", 1, Infinity, (a, r, t) =>
    r.invokeRestart(a[0], a.slice(1), t),
  );
  def("INVOKE-RESTART-INTERACTIVELY", 1, 1, (a, r, t) =>
    r.invokeRestart(a[0], [], t, true),
  );
  unary("RESTART-NAME", (x) => x instanceof LispRestart ? x.name : typeError(x, "restart"));
  for (const [name, takesValue, mandatory] of [["ABORT", false, true], ["MUFFLE-WARNING", false, true],
    ["CONTINUE", false, false], ["STORE-VALUE", true, false], ["USE-VALUE", true, false]]) {
    const restartName = rt.intern(name, "COMMON-LISP"), required = takesValue ? 1 : 0;
    def(name, required, required + 1, function* (a, r, t) {
      const restart = yield* findRestart(restartName, a[required] ?? NIL, r, t);
      if (restart !== NIL) return yield* r.invokeRestart(restart, takesValue ? [a[0]] : [], t);
      if (mandatory) fail("CONTROL-ERROR", `No active ${name} restart`);
      return NIL;
    });
  }
  for (const [cls, slots] of [
    ["SIMPLE-CONDITION", ["FORMAT-CONTROL", "FORMAT-ARGUMENTS"]], ["TYPE-ERROR", ["DATUM", "EXPECTED-TYPE"]],
    ["CELL-ERROR", ["NAME"]], ["UNBOUND-SLOT", ["INSTANCE"]], ["ARITHMETIC-ERROR", ["OPERATION", "OPERANDS"]],
    ["STREAM-ERROR", ["STREAM"]], ["PACKAGE-ERROR", ["PACKAGE"]], ["FILE-ERROR", ["PATHNAME"]], ["PRINT-NOT-READABLE", ["OBJECT"]],
  ]) for (const slot of slots) {
    const type = rt.intern(cls, "COMMON-LISP"), slotName = rt.intern(slot, "COMMON-LISP");
    def(`${cls}-${slot}`, 1, 1, (a, r, t) => {
      if (!r.isInstanceOf(a[0], type, t))
        fail("TYPE-ERROR", `Expected ${cls}`, { datum: a[0], expectedType: type });
      return r.slotValueSteps(a[0], slotName, t);
    });
  }
  def("LISP-IMPLEMENTATION-TYPE", 0, 0, () => string("zkcl"));
  def("LISP-IMPLEMENTATION-VERSION", 0, 0, () => string("JavaScript"));
  return rt;
}
