import * as O from "./objects.mjs";
import {
  NIL, T, UNBOUND, Cons, LispSymbol, LispString, LispCharacter, LispArray, list, fromArray,
} from "./objects.mjs";
import * as N from "./numbers.mjs";
import { MultipleValues, LispCondition } from "./execution.mjs";
import { StringStream, streamDirection, writeStreamSteps } from "./file_streams.mjs";

export const truth = (yes) => (yes ? T : NIL);
export const primary = (x) => (x instanceof MultipleValues ? (x.values[0] ?? NIL) : x);
export const fail = (name, message, fields = {}) => {
  throw new LispCondition(name, message, fields);
};
export const typeError = (value, expected) =>
  fail("TYPE-ERROR", `Expected ${expected}`, {
    datum: value,
    expectedType: expected,
  });
export const isString = (value) =>
  value instanceof LispString ||
  (value instanceof LispArray &&
    value.rank === 1 &&
    value.elementKind === "character");
export const str = (value) =>
  value instanceof LispString
    ? value.toString()
    : isString(value)
      ? Array.from({ length: value.length }, (_, i) =>
          value.get(i).toString(),
        ).join("")
      : typeof value === "string"
        ? value
        : typeError(value, "string");
export const string = (value) => new LispString(value);
export const name = (value) =>
  value === NIL
    ? "NIL"
    : value === T
      ? "T"
      : value instanceof LispSymbol
        ? value.name
        : value instanceof LispCharacter
          ? String.fromCodePoint(value.codePoint)
          : str(value);
export const sym = (value) =>
  value instanceof LispSymbol || value === NIL || value === T
    ? value
    : typeError(value, "symbol");
export const integer = (value) => N.toBigInt(value);
export function naturalInteger(value) {
  if (!N.isInteger(value)) typeError(value, "nonnegative integer");
  const n = integer(value);
  if (n < 0n) typeError(value, "nonnegative integer");
  return n;
}
export const index = (value) => {
  const n = naturalInteger(value);
  if (n > BigInt(Number.MAX_SAFE_INTEGER))
    typeError(value, "nonnegative index");
  return Number(n);
};
export const boxInteger = (value) => N.integer(value);
export const floatResult = (value, example) =>
  example instanceof N.DoubleFloat
    ? new N.DoubleFloat(value)
    : new N.SingleFloat(value);
export const booleOperations = [
  ["CLR", () => 0n], ["SET", () => -1n],
  ["1", (a) => a], ["2", (_, b) => b],
  ["C1", (a) => ~a], ["C2", (_, b) => ~b],
  ["AND", (a, b) => a & b], ["IOR", (a, b) => a | b],
  ["XOR", (a, b) => a ^ b], ["EQV", (a, b) => ~(a ^ b)],
  ["NAND", (a, b) => ~(a & b)], ["NOR", (a, b) => ~(a | b)],
  ["ANDC1", (a, b) => ~a & b], ["ANDC2", (a, b) => a & ~b],
  ["ORC1", (a, b) => ~a | b], ["ORC2", (a, b) => a | ~b],
];
export const call = (rt, task, fn, args) =>
  task ? task.call(fn, args) : rt.call(fn, args);
export const invoke = (rt, task, fn, args) => primary(call(rt, task, fn, args));
export function* requestedValue(rt, fn, args) {
  return primary(yield rt.requestCall(fn, args));
}
export function arity(args, min, max = min) {
  if (args.length < min || args.length > max)
    fail(
      "PROGRAM-ERROR",
      `Expected ${min}${max !== min ? `..${max}` : ""} arguments, received ${args.length}`,
    );
}
export function keys(args, allowed) {
  const result = new Map();
  let unknown;
  for (let i = 0; i + 1 < args.length; i += 2) {
    const symbol = args[i];
    if (symbol instanceof LispSymbol && symbol.package?.name === "KEYWORD" &&
        (symbol.name === "ALLOW-OTHER-KEYS" || allowed.includes(symbol.name))) {
      if (!result.has(symbol.name)) result.set(symbol.name, args[i + 1]);
    } else {
      unknown ??= symbol;
    }
  }
  if ((result.get("ALLOW-OTHER-KEYS") ?? NIL) === NIL) {
    if (args.length % 2) fail("PROGRAM-ERROR", "Odd number of keyword arguments");
    if (unknown !== undefined) {
      if (!(unknown instanceof LispSymbol) && unknown !== NIL && unknown !== T)
        fail("PROGRAM-ERROR", "Keyword argument names must be symbols");
      fail("PROGRAM-ERROR", `Unknown keyword :${name(unknown)}`);
    }
  }
  return result;
}
export function car(value) {
  return value === NIL
    ? NIL
    : value instanceof Cons
      ? value.car
      : typeError(value, "list");
}
export function cdr(value) {
  return value === NIL
    ? NIL
    : value instanceof Cons
      ? value.cdr
      : typeError(value, "list");
}
export function cells(value) {
  const result = [],
    seen = new Set();
  while (value instanceof Cons) {
    if (seen.has(value))
      fail("TYPE-ERROR", "Expected a finite proper list", { datum: value });
    seen.add(value);
    result.push(value);
    value = value.cdr;
  }
  if (value !== NIL) typeError(value, "proper list");
  return result;
}
export function listSpine(value) {
  if (value !== NIL && !(value instanceof Cons)) typeError(value, "list");
  const result = [],
    seen = new Set();
  while (value instanceof Cons) {
    if (seen.has(value)) typeError(value, "finite list");
    seen.add(value);
    result.push(value);
    value = value.cdr;
  }
  return [result, value];
}
export function nthTail(value, n) {
  if (value !== NIL && !(value instanceof Cons)) typeError(value, "list");
  if (n < 1024) {
    for (let i = Number(n); i !== 0 && value !== NIL; i--) value = cdr(value);
    return value;
  }
  const remaining = new Map();
  while (n !== 0n && value !== NIL) {
    if (remaining.has(value)) {
      const offset = n % (remaining.get(value) - n);
      for (let i = offset; i !== 0n; i--) value = cdr(value);
      break;
    }
    remaining.set(value, n--);
    value = cdr(value);
  }
  return value;
}
export function sequence(value) {
  if (value === NIL || value instanceof Cons) {
    const spine = cells(value);
    return {
      length: spine.length,
      get: (i) => spine[i].car,
      set: (i, x) => {
        spine[i].car = x;
      },
      kind: "list",
      spine,
    };
  }
  if (value instanceof LispString || (value instanceof LispArray && value.rank === 1))
    return {
      length: value.length,
      get: (i) => value.get(i),
      set: (i, x) => value.set(i, x),
      kind: isString(value) ? "string" : "vector",
      elementType: value.elementType,
    };
  return typeError(value, "sequence");
}
export function bounds(length, start = 0, end = NIL) {
  start = index(start);
  end = end === NIL ? length : index(end);
  if (start > end || end > length)
    fail("TYPE-ERROR", "Invalid sequence bounds");
  return [start, end];
}
export function array(values) {
  return new LispArray([values.length], { initialContents: values });
}
export function sequenceResult(kind, values, elementType = "T") {
  if (kind === "list") return fromArray(values);
  if (kind === "string") {
    const chars = values.map((value) => {
      if (!(value instanceof LispCharacter)) typeError(value, "character");
      return value.toString();
    });
    return string(chars.join(""));
  }
  return new LispArray([values.length], { initialContents: values, elementType });
}

export function eqlScalarKey(value) {
  if (N.isInteger(value)) return `integer:${integer(value)}`;
  if (value instanceof LispCharacter) return `character:${value.codePoint}`;
  if (value instanceof N.SingleFloat) return `single:${value.bits}`;
  if (value instanceof N.DoubleFloat) return `double:${value.bits}`;
  if (value instanceof N.Ratio) return `ratio:${value.numerator}/${value.denominator}`;
  if (value instanceof N.Complex)
    return `complex:${eqlScalarKey(value.real)};${eqlScalarKey(value.imaginary)}`;
  return null;
}

export function editSequence(original, source, selected, destructive, replacement) {
  if (selected.size === 0) return original;
  if (destructive && replacement !== undefined) {
    for (const i of selected) source.set(i, replacement);
    return original;
  }
  if (destructive && source.spine !== undefined) {
    let head = NIL;
    for (let i = source.length - 1; i >= 0; i--) {
      if (selected.has(i)) continue;
      const cell = source.spine[i];
      cell.cdr = head;
      head = cell;
    }
    return head;
  }
  const values = [];
  for (let i = 0; i < source.length; i++) {
    if (!selected.has(i)) values.push(source.get(i));
    else if (replacement !== undefined) values.push(replacement);
  }
  return sequenceResult(source.kind, values, source.elementType);
}
export function arrayObject(value) {
  if (!(value instanceof LispArray)) typeError(value, "array");
  return value;
}
export function rowIndex(value, subscripts) {
  const dims =
    value instanceof LispString
      ? [[...str(value)].length]
      : arrayObject(value).dimensions;
  if (dims.length !== subscripts.length)
    fail("PROGRAM-ERROR", "Wrong number of array subscripts");
  let result = 0;
  dims.forEach((dim, i) => {
    const sub = index(subscripts[i]);
    if (sub >= dim) fail("TYPE-ERROR", "Array subscript out of bounds");
    result = result * dim + sub;
  });
  return result;
}
export function arrayGet(value, i) {
  if (value instanceof LispString) return value.get(i);
  value = arrayObject(value);
  if (i >= value.totalSize) fail("TYPE-ERROR", "Array index out of bounds");
  return value.get(i);
}
export function arraySet(value, i, item) {
  arrayGet(value, i);
  if (value instanceof LispString && !(item instanceof LispCharacter))
    typeError(item, "character");
  value.set(i, item);
  return item;
}

export class FillPointerStream extends StringStream {
  constructor(destination) {
    super();
    this.destination = destination;
  }
  get column() {
    let column = 0;
    for (let i = this.destination.length - 1; i >= 0 && this.destination.get(i).codePoint !== 10; i--)
      column++;
    return column;
  }
  write(text) {
    if (!this.open) fail("STREAM-ERROR", "Stream is not open for output", { stream: this });
    const vector = this.destination;
    for (const character of text) {
      const position = vector.fillPointer;
      if (position === vector.totalSize) {
        if (!vector.adjustable) fail("STREAM-ERROR", "String fill pointer reached its capacity", { stream: this });
        const length = Math.max(1, Math.min(vector.totalSize * 2, O.ARRAY_TOTAL_SIZE_LIMIT - 1));
        vector.growVector(length);
      }
      vector.set(position, new LispCharacter(character));
      vector.fillPointer++;
    }
  }
}
export function dynamicValue(rt, task, spelling, fallback) {
  const symbol = rt.findSymbol(spelling, "COMMON-LISP");
  if (!symbol) return fallback;
  const value = rt.variableValue
    ? rt.variableValue(symbol, task)
    : symbol.value;
  return value === UNBOUND ? fallback : value;
}
export function readDefaultDouble(rt, task) {
  const format = dynamicValue(rt, task, "*READ-DEFAULT-FLOAT-FORMAT*", NIL);
  return format?.name === "DOUBLE-FLOAT" || format?.name === "LONG-FLOAT";
}
export function outputStream(rt, stream, task) {
  const target = stream == null || stream === NIL
    ? dynamicValue(rt, task, "*STANDARD-OUTPUT*", rt.output)
    : stream === T ? dynamicValue(rt, task, "*TERMINAL-IO*", rt.output) : stream;
  if (!streamDirection(rt, target, "output", task)) typeError(target, "output stream");
  return target;
}
export function* output(rt, stream, text, task) {
  yield* writeStreamSteps(rt, outputStream(rt, stream, task), text);
}
export class PprintDispatchTable {
  constructor(entries = []) {
    this.entries = entries.map((entry) => ({ ...entry }));
  }
}
