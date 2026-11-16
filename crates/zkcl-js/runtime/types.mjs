import * as O from "./objects.mjs";
import {
  NIL, T, Cons, LispSymbol, LispString, LispCharacter, LispArray, LispPathname,
  list, fromArray, toArray, eql,
} from "./objects.mjs";
import * as N from "./numbers.mjs";
import { LispHashTable } from "./hash_tables.mjs";
import { NativeFunction, LispCondition } from "./execution.mjs";
import { LispClass } from "./clos.mjs";
import { LispFileStream, StringStream } from "./file_streams.mjs";
import {
  fail, typeError, isString, str, integer, index, requestedValue,
} from "./builtins_helpers.mjs";

const INTEGER_PART = 1, RATIO_PART = 2, SINGLE_PART = 4, DOUBLE_PART = 8, NON_REAL_PART = 16;
const RATIONAL_PART = INTEGER_PART | RATIO_PART, FLOAT_PART = SINGLE_PART | DOUBLE_PART;
const REAL_PART = RATIONAL_PART | FLOAT_PART, ANY_PART = REAL_PART | NON_REAL_PART;
export const nonClassTypeNames = [
  'ATOM', 'BOOLEAN', 'KEYWORD', 'BASE-CHAR', 'STANDARD-CHAR', 'BASE-STRING', 'SIMPLE-BASE-STRING',
  'SIMPLE-STRING', 'SIMPLE-ARRAY', 'SIMPLE-VECTOR', 'SIMPLE-BIT-VECTOR', 'COMPILED-FUNCTION',
  'SATISFIES', 'BYTE-SPECIFIER', '*',
];
export const partRepresentation = new Map([
  ["NIL", 0], ["INTEGER", INTEGER_PART], ["FIXNUM", INTEGER_PART], ["BIGNUM", INTEGER_PART], ["BIT", INTEGER_PART],
  ["SIGNED-BYTE", INTEGER_PART], ["UNSIGNED-BYTE", INTEGER_PART], ["MOD", INTEGER_PART],
  ["RATIO", RATIO_PART], ["RATIONAL", RATIONAL_PART], ["SHORT-FLOAT", SINGLE_PART], ["SINGLE-FLOAT", SINGLE_PART],
  ["DOUBLE-FLOAT", DOUBLE_PART], ["LONG-FLOAT", DOUBLE_PART], ["FLOAT", FLOAT_PART], ["REAL", REAL_PART],
  ["COMPLEX", NON_REAL_PART], ["NUMBER", ANY_PART], ["T", ANY_PART],
]);
const representationOf = (value) => N.isInteger(value) ? INTEGER_PART : value instanceof N.Ratio ? RATIO_PART
  : value instanceof N.SingleFloat ? SINGLE_PART : value instanceof N.DoubleFloat ? DOUBLE_PART : NON_REAL_PART;
export function commonLispName(value, rt) {
  if (value instanceof LispClass) {
    if (rt.findClass(value.name, false) !== value) return null;
    value = value.name;
  }
  const spelling = nameSafe(value);
  return spelling !== null && rt.findSymbol(spelling, "COMMON-LISP") === value ? spelling : null;
}

export function* possiblePartRepresentations(spec, rt, environment) {
  const [expanded] = yield* rt.expandTypeSteps(spec, environment);
  if (!(expanded instanceof Cons))
    return partRepresentation.get(commonLispName(expanded, rt)) ?? ANY_PART;
  const [operator, ...args] = toArray(expanded);
  const op = commonLispName(operator, rt);
  if (op === "EQL" || op === "MEMBER") return args.reduce((mask, value) => mask | representationOf(value), 0);
  if (op === "OR" || op === "AND") {
    let mask = op === "AND" ? ANY_PART : 0;
    for (const arg of args) {
      const child = yield* possiblePartRepresentations(arg, rt, environment);
      mask = op === "AND" ? mask & child : mask | child;
    }
    return mask;
  }
  if (op === "NOT") {
    // Only complement types that include an entire representation. For
    // instance (not fixnum) may still contain integers of that representation.
    const source = commonLispName(args[0], rt);
    return ["NIL", "INTEGER", "RATIO", "RATIONAL", "SHORT-FLOAT", "SINGLE-FLOAT",
      "DOUBLE-FLOAT", "LONG-FLOAT", "FLOAT", "REAL", "T"].includes(source)
      ? ANY_PART ^ partRepresentation.get(source) : ANY_PART;
  }
  if (["INTEGER", "RATIONAL", "REAL", "FLOAT", "SHORT-FLOAT", "SINGLE-FLOAT", "DOUBLE-FLOAT", "LONG-FLOAT"].includes(op)) {
    const [low, high] = args;
    const from = low instanceof Cons ? low.car : low;
    const to = high instanceof Cons ? high.car : high;
    if (N.isReal(from) && N.isReal(to)) {
      const order = N.compare(from, to);
      if (order > 0 || (order === 0 && (low instanceof Cons || high instanceof Cons))) return 0;
      if (op === "INTEGER" && low instanceof Cons && high instanceof Cons &&
          N.isInteger(from) && N.isInteger(to) && integer(from) + 1n === integer(to)) return 0;
    }
  }
  return partRepresentation.get(op) ?? ANY_PART;
}

export function* complexPartType(spec, rt, environment) {
  const mask = yield* possiblePartRepresentations(spec, rt, environment);
  if (mask & NON_REAL_PART) fail("ERROR", "UPGRADED-COMPLEX-PART-TYPE requires a subtype of REAL");
  if (mask === 0) return [NIL, 0];
  const type = mask <= RATIONAL_PART ? "RATIONAL" : mask === SINGLE_PART ? "SINGLE-FLOAT"
    : mask === DOUBLE_PART ? "DOUBLE-FLOAT" : mask === FLOAT_PART ? "FLOAT" : "REAL";
  return [rt.intern(type, "COMMON-LISP"), partRepresentation.get(type)];
}

export function* complexMask(spec, rt, environment) {
  if (spec instanceof Cons) {
    const [operator, arg] = toArray(spec);
    if (commonLispName(operator, rt) !== "COMPLEX") return null;
    if (arg !== undefined && commonLispName(arg, rt) !== "*") return (yield* complexPartType(arg, rt, environment))[1];
    return REAL_PART;
  }
  return commonLispName(spec, rt) === "COMPLEX" ? REAL_PART : null;
}

export function* requestedTypep(value, spec, rt, environment = NIL) {
  const [expanded] = yield* rt.expandTypeSteps(spec, environment);
  return yield* expandedTypep(value, expanded, rt, environment);
}

function* matchesArrayElement(value, elementType, rt, environment) {
  if (elementType === undefined || commonLispName(elementType, rt) === "*") return true;
  const [expanded] = yield* rt.expandTypeSteps(elementType, environment);
  const { elementKind } = new LispArray([0], { elementType: expanded });
  return value.elementKind === elementKind;
}

export function* expandedTypep(value, spec, rt, environment) {
  if (spec instanceof Cons) {
    const items = toArray(spec),
      op = commonLispName(items[0], rt);
    if (op === "OR") {
      for (const type of items.slice(1))
        if (yield* requestedTypep(value, type, rt, environment)) return true;
      return false;
    }
    if (op === "AND") {
      for (const type of items.slice(1))
        if (!(yield* requestedTypep(value, type, rt, environment))) return false;
      return true;
    }
    if (op === "NOT") return !(yield* requestedTypep(value, items[1], rt, environment));
    if (op === "EQL") return eql(value, items[1]);
    if (op === "MEMBER") return items.slice(1).some((x) => eql(x, value));
    if (op === "SATISFIES") return (yield* requestedValue(rt, items[1], [value])) !== NIL;
    if (["INTEGER", "RATIONAL", "REAL", "FLOAT", "SHORT-FLOAT", "SINGLE-FLOAT", "DOUBLE-FLOAT", "LONG-FLOAT"].includes(op)) {
      if (!atomicTypep(value, items[0], rt)) return false;
      return items.slice(1, 3).every((limit, i) => {
        if (commonLispName(limit, rt) === "*") return true;
        const exclusive = limit instanceof Cons;
        const n = exclusive ? limit.car : limit;
        const c = N.compare(value, n);
        return i ? (exclusive ? c < 0 : c <= 0) : exclusive ? c > 0 : c >= 0;
      });
    }
    if (op === "CONS")
      return (
        value instanceof Cons &&
        (items[1] == null ||
          commonLispName(items[1], rt) === "*" ||
          (yield* requestedTypep(value.car, items[1], rt, environment))) &&
        (items[2] == null ||
          commonLispName(items[2], rt) === "*" ||
          (yield* requestedTypep(value.cdr, items[2], rt, environment)))
      );
    if (op === "UNSIGNED-BYTE" || op === "SIGNED-BYTE") {
      if (!N.isInteger(value)) return false;
      const n = integer(value),
        width = items[1];
      if (width == null || commonLispName(width, rt) === "*")
        return op === "SIGNED-BYTE" || n >= 0n;
      const bits = integer(width);
      if (bits < 0n || (op === "SIGNED-BYTE" && bits === 0n))
        typeError(spec, "integer type");
      return op === "UNSIGNED-BYTE"
        ? n >= 0n && n < 1n << bits
        : n >= -(1n << (bits - 1n)) && n < 1n << (bits - 1n);
    }
    if (op === "MOD")
      return (
        N.isInteger(value) &&
        integer(value) >= 0n &&
        integer(value) < integer(items[1])
      );
    if (op === "COMPLEX") {
      const mask = yield* complexMask(spec, rt, environment);
      if (mask === null) return typeError(spec, "supported type specifier");
      return value instanceof N.Complex && (mask & representationOf(value.real)) !== 0;
    }
    if (op === "ARRAY" || op === "SIMPLE-ARRAY") {
      if (!atomicTypep(value, items[0], rt)) return false;
      if (!(yield* matchesArrayElement(value, items[1], rt, environment))) return false;
      const dims = value.dimensions,
        expected = items[2];
      if (expected === undefined || commonLispName(expected, rt) === "*") return true;
      if (N.isInteger(expected)) return dims.length === index(expected);
      const dimensions = toArray(expected);
      return (
        dimensions.length === dims.length &&
        dimensions.every((x, i) => commonLispName(x, rt) === "*" || index(x) === dims[i])
      );
    }
    if (
      [
        "VECTOR",
        "SIMPLE-VECTOR",
        "STRING",
        "BASE-STRING",
        "SIMPLE-BASE-STRING",
        "SIMPLE-STRING",
        "BIT-VECTOR",
        "SIMPLE-BIT-VECTOR",
      ].includes(op)
    ) {
      if (!atomicTypep(value, items[0], rt)) return false;
      let size = items[1];
      if (op === "VECTOR") {
        if (!(yield* matchesArrayElement(value, items[1], rt, environment))) return false;
        size = items[2];
      }
      return (
        size == null ||
        commonLispName(size, rt) === "*" ||
        value.dimensions[0] === index(size)
      );
    }
    return typeError(spec, "supported type specifier");
  }
  return atomicTypep(value, spec, rt, environment);
}
export function atomicTypep(value, spec, rt, environment = NIL) {
  if (spec instanceof LispClass) return rt.isInstanceOf(value, spec);
  const type = commonLispName(spec, rt);
  switch (type) {
    case "T":
      return true;
    case "NIL":
      return false;
    case "NULL":
      return value === NIL;
    case "BOOLEAN":
      return value === NIL || value === T;
    case "ATOM":
      return !(value instanceof Cons);
    case "CONS":
      return value instanceof Cons;
    case "LIST":
      return value === NIL || value instanceof Cons;
    case "SYMBOL":
      return value === NIL || value === T || value instanceof LispSymbol;
    case "KEYWORD":
      return (
        value instanceof LispSymbol &&
        (value.package === "KEYWORD" || value.package?.name === "KEYWORD")
      );
    case "NUMBER":
      return N.isNumber(value);
    case "RANDOM-STATE":
      return value instanceof N.RandomState;
    case "INTEGER":
    case "SIGNED-BYTE":
      return N.isInteger(value);
    case "UNSIGNED-BYTE":
      return N.isInteger(value) && value >= 0;
    case "FIXNUM":
      return (
        typeof value === "number" &&
        Number.isInteger(value) &&
        value >= -268435456 &&
        value <= 268435455
      );
    case "BIGNUM":
      return typeof value === "bigint";
    case "RATIO":
      return value instanceof N.Ratio;
    case "RATIONAL":
      return N.isInteger(value) || value instanceof N.Ratio;
    case "REAL":
      return N.isNumber(value) && !(N.Complex && value instanceof N.Complex);
    case "FLOAT":
      return N.isFloat(value);
    case "SHORT-FLOAT":
    case "SINGLE-FLOAT":
      return value instanceof N.SingleFloat;
    case "DOUBLE-FLOAT":
    case "LONG-FLOAT":
      return value instanceof N.DoubleFloat;
    case "COMPLEX":
      return !!N.Complex && value instanceof N.Complex;
    case "BIT":
      return (
        N.isInteger(value) && (integer(value) === 0n || integer(value) === 1n)
      );
    case "CHARACTER":
    case "BASE-CHAR":
      return value instanceof LispCharacter;
    case "STANDARD-CHAR":
      return (
        value instanceof LispCharacter &&
        (value.codePoint === 10 ||
          (value.codePoint >= 32 && value.codePoint <= 126))
      );
    case "STRING":
    case "BASE-STRING":
      return isString(value);
    case "SIMPLE-STRING":
    case "SIMPLE-BASE-STRING":
      return (
        value instanceof LispString ||
        (isString(value) &&
          !value.adjustable &&
          value.fillPointer === null &&
          value.displacedTo === null)
      );
    case "ARRAY":
      return value instanceof LispArray || value instanceof LispString;
    case "SIMPLE-ARRAY":
      return (
        value instanceof LispString ||
        (value instanceof LispArray &&
          !value.adjustable &&
          value.fillPointer === null &&
          value.displacedTo === null)
      );
    case "VECTOR":
      return (
        value instanceof LispString ||
        (value instanceof LispArray && value.dimensions.length === 1)
      );
    case "SIMPLE-VECTOR":
      return (
        value instanceof LispArray &&
        value.dimensions.length === 1 &&
        value.fillPointer == null &&
        !value.adjustable &&
        value.displacedTo === null &&
        value.elementKind === "general"
      );
    case "BIT-VECTOR":
      return (
        value instanceof LispArray &&
        value.rank === 1 &&
        value.elementKind === "bit"
      );
    case "SIMPLE-BIT-VECTOR":
      return (
        value instanceof LispArray &&
        value.rank === 1 &&
        value.elementKind === "bit" &&
        !value.adjustable &&
        value.fillPointer === null &&
        value.displacedTo === null
      );
    case "SEQUENCE":
      return (
        value === NIL ||
        value instanceof Cons ||
        value instanceof LispString ||
        (value instanceof LispArray && value.dimensions.length === 1)
      );
    case "FUNCTION":
    case "COMPILED-FUNCTION":
      return (
        value instanceof NativeFunction ||
        value instanceof O.Closure ||
        value instanceof O.LispCode ||
        typeof value?.invoke === "function"
      );
    case "HASH-TABLE":
      return value instanceof LispHashTable;
    case "STREAM":
      return rt?.streamClassName(value) !== undefined;
    case "TWO-WAY-STREAM":
    case "ECHO-STREAM":
    case "CONCATENATED-STREAM":
    case "SYNONYM-STREAM":
    case "BROADCAST-STREAM":
      return rt?.streamClassName(value) === type;
    case "PATHNAME":
      return value instanceof LispPathname;
    case "LOGICAL-PATHNAME":
      return value instanceof LispPathname && value.logical;
    case "READTABLE":
      return !!rt?.isReadtable?.(value);
    case "FILE-STREAM":
      return value instanceof LispFileStream;
    case "STRING-STREAM":
      return value instanceof StringStream;
    case "BYTE-SPECIFIER":
      return value instanceof Cons && N.isInteger(value.car) && value.car >= 0 &&
        N.isInteger(value.cdr) && value.cdr >= 0;
    case "PACKAGE":
      return !!value && value.symbols instanceof Map;
    default: {
      const cls = rt?.findClassInEnvironment?.(spec, false, environment);
      if (cls) return rt.isInstanceOf(value, cls);
      fail("TYPE-ERROR", `Unknown type specifier ${nameSafe(spec) ?? spec}`, {
        datum: spec,
      });
    }
  }
}
export function nameSafe(value) {
  return value === NIL
    ? "NIL"
    : value === T
      ? "T"
      : value instanceof LispSymbol
        ? value.name
        : null;
}
export function expectedSpecifier(expected, rt) {
  if (typeof expected !== "string") return expected;
  const spelling = expected.toUpperCase(),
    symbol = (n) => rt.intern(n, "COMMON-LISP");
  if (/^(AN?|THE) /u.test(spelling))
    return expectedSpecifier(spelling.replace(/^(AN?|THE) /u, ""), rt);
  if (
    [
      "NONNEGATIVE INDEX",
      "NONNEGATIVE INTEGER",
      "ARRAY AXIS",
      "VALID FILL POINTER",
      "EXACT HOST INTEGER",
    ].includes(spelling)
  )
    return list(symbol("INTEGER"), 0, symbol("*"));
  if (spelling === "RADIX 2..36") return list(symbol("INTEGER"), 2, 36);
  if (spelling === "PROPER LIST" || spelling === "FINITE LIST")
    return symbol("LIST");
  if (spelling === "REAL NUMBER") return symbol("REAL");
  if (spelling === "OR STRING FUNCTION")
    return list(symbol("OR"), symbol("STRING"), symbol("FUNCTION"));
  if (spelling === "CHARACTER DESIGNATOR")
    return list(
      symbol("OR"),
      symbol("CHARACTER"),
      symbol("STRING"),
      symbol("SYMBOL"),
    );
  if (spelling === "CONDITION DESIGNATOR")
    return list(
      symbol("OR"),
      symbol("CONDITION"),
      symbol("STRING"),
      symbol("SYMBOL"),
    );
  if (spelling.includes("STREAM")) return symbol("STREAM");
  if (spelling === "ARRAY WITH FILL POINTER") return symbol("VECTOR");
  if (spelling === "BYTE SPECIFIER") return symbol("BYTE-SPECIFIER");
  return symbol(spelling.replaceAll(" ", "-"));
}
export function bridgeCondition(error, rt, operation, args) {
  if (error instanceof N.NumericError)
    return new LispCondition(error.kind, error.message, {
      datum: error.operands?.[0],
      operation: rt.intern(operation, "COMMON-LISP"),
      operands: args,
      expectedType:
        error.expectedType === undefined
          ? undefined
          : expectedSpecifier(error.expectedType, rt),
    });
  if (error instanceof LispCondition && typeof error.expectedType === "string")
    error.expectedType = expectedSpecifier(error.expectedType, rt);
  return error;
}

export function* coerceCondition(rt, designator, args, defaultClass, task) {
  if (designator instanceof LispCondition) {
    if (args.length) fail("TYPE-ERROR", "A condition object cannot be followed by arguments", {
      datum: fromArray(args), expectedType: rt.intern("NULL", "COMMON-LISP"),
    });
    return designator;
  }
  if (isString(designator) || atomicTypep(designator, rt.intern("FUNCTION", "COMMON-LISP"), rt)) {
    // The report is Lisp code. In particular, constructing or signaling a
    // condition does not execute a formatter before a handler can see it.
    return new LispCondition(defaultClass, isString(designator) ? str(designator) : defaultClass, {
      formatControl: designator, formatArguments: fromArray(args),
    });
  }
  return yield* rt.makeConditionSteps(designator, args, task);
}
