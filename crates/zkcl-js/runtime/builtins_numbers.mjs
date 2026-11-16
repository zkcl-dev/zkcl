import { NIL, T, Cons, list } from "./objects.mjs";
import * as N from "./numbers.mjs";
import {
  truth, fail, typeError, name, integer, boxInteger, floatResult, booleOperations,
} from "./builtins_helpers.mjs";
import { partRepresentation, complexPartType } from "./types.mjs";

export function installNumbers(rt, def, unary, pred) {
  const cl = (name) => rt.intern(name, "COMMON-LISP");
  for (const name of partRepresentation.keys()) rt.exportSymbol(cl(name));
  for (const name of ["OR", "AND", "NOT", "EQL", "MEMBER"]) rt.exportSymbol(cl(name));
  const currentState = rt.exportSymbol(cl("*RANDOM-STATE*"));
  currentState.value = new N.RandomState();
  rt.specials.add(currentState);
  const randomStateType = rt.exportSymbol(cl("RANDOM-STATE"));
  const randomState = (value) => value instanceof N.RandomState
    ? value
    : typeError(value, randomStateType);
  pred("RANDOM-STATE-P", (value) => value instanceof N.RandomState);
  def("MAKE-RANDOM-STATE", 0, 1, ([value = NIL], r, task) => {
    if (value === T) return N.RandomState.seeded();
    if (value === NIL) return new N.RandomState(randomState(r.readVariable(currentState, task)));
    if (value instanceof N.RandomState) return new N.RandomState(value);
    fail("TYPE-ERROR", "Expected a random state, T, or NIL", {
      datum: value, expectedType: list(cl("OR"), randomStateType, cl("BOOLEAN")),
    });
  });
  def("RANDOM", 1, 2, ([limit, state], r, task) => {
    state = randomState(state === undefined ? r.readVariable(currentState, task) : state);
    if (!((N.isInteger(limit) && limit > 0) || (N.isFloat(limit) && limit.value > 0)))
      fail("TYPE-ERROR", "Expected a positive integer or float", {
        datum: limit, expectedType: list(cl("OR"), list(cl("INTEGER"), 1, cl("*")), list(cl("FLOAT"), list(new N.SingleFloat(0)))),
      });
    return state.sample(limit);
  });
  for (const [n, min, fn] of [
    ["+", 0, N.add],
    ["*", 0, N.multiply],
    ["-", 1, N.subtract],
    ["/", 1, N.divide],
  ])
    def(n, min, Infinity, (a) => fn(...a));
  for (const [op, test] of [
    ["=", (c) => c === 0],
    ["<", (c) => c < 0],
    ["<=", (c) => c <= 0],
    [">", (c) => c > 0],
    [">=", (c) => c >= 0],
  ])
    def(op, 1, Infinity, (a) => {
      if (op === "=") {
        a.forEach((x) => {
          if (!N.isNumber(x)) typeError(x, "number");
        });
        return truth(a.slice(1).every((x, i) => N.numericEqual(a[i], x)));
      }
      a.forEach((x) => {
        if (!N.isReal(x)) typeError(x, "real");
      });
      return truth(a.slice(1).every((x, i) => test(N.compare(a[i], x))));
    });
  def("/=", 1, Infinity, (a) => {
    for (const x of a) if (!N.isNumber(x)) typeError(x, "number");
    for (let i = 0; i < a.length; i++) {
      for (let j = i + 1; j < a.length; j++)
        if (N.numericEqual(a[i], a[j])) return NIL;
    }
    return T;
  });
  unary("1+", (x) => N.add(x, 1));
  unary("1-", (x) => N.subtract(x, 1));
  unary("ABS", N.absolute);
  pred("ZEROP", N.isZero);
  pred("PLUSP", (x) => N.compare(x, 0) > 0);
  pred("MINUSP", (x) => N.compare(x, 0) < 0);
  pred("ODDP", (x) => (integer(x) & 1n) !== 0n);
  pred("EVENP", (x) => (integer(x) & 1n) === 0n);
  unary("SIGNUM", N.signum);
  for (const [op, direction] of [["MAX", 1], ["MIN", -1]])
    def(op, 1, Infinity, (a) => {
      if (!N.isReal(a[0])) typeError(a[0], "real");
      return a.reduce((x, y) => direction * N.compare(x, y) >= 0 ? x : y);
    });
  const requireFloat = (value) => N.isFloat(value) ? value : typeError(value, "float");
  def("FLOAT", 1, 2, ([value, prototype]) => {
    if (!N.isReal(value)) typeError(value, "real");
    if (prototype === undefined && N.isFloat(value)) return value;
    return N.coerceFloat(value, prototype === undefined ? 1 : requireFloat(prototype));
  });
  unary("DECODE-FLOAT", (value) => rt.multipleValues(...N.decodeFloat(value, false)));
  unary("INTEGER-DECODE-FLOAT", (value) => rt.multipleValues(...N.decodeFloat(value, true)));
  unary("FLOAT-RADIX", (value) => { requireFloat(value); return 2; });
  unary("FLOAT-DIGITS", N.floatDigits);
  unary("FLOAT-PRECISION", N.floatPrecision);
  def("FLOAT-SIGN", 1, 2, ([sign, magnitude]) => {
    const signValue = requireFloat(sign).value;
    const negative = signValue < 0 || Object.is(signValue, -0);
    const absolute = magnitude === undefined ? 1 : Math.abs(requireFloat(magnitude).value);
    return floatResult(negative ? -absolute : absolute, magnitude ?? sign);
  });
  def("SCALE-FLOAT", 2, 2, ([value, scale]) => N.scaleFloat(value, scale));
  unary("RATIONAL", (x) => {
    if (N.isRational(x)) return x;
    const [n, d] = N.rationalParts(x);
    return N.makeRatio(boxInteger(n), boxInteger(d));
  });
  unary("RATIONALIZE", N.rationalize);
  def("UPGRADED-COMPLEX-PART-TYPE", 1, 2, function* ([type, environment = NIL], r) {
    return (yield* complexPartType(type, r, environment))[0];
  });
  unary("NUMERATOR", (x) => {
    if (!N.isRational(x)) typeError(x, "rational");
    return x instanceof N.Ratio ? x.numerator : x;
  });
  unary("DENOMINATOR", (x) => {
    if (!N.isRational(x)) typeError(x, "rational");
    return x instanceof N.Ratio ? x.denominator : 1;
  });
  def("COMPLEX", 1, 2, (a) => N.makeComplex(a[0], a[1] ?? 0));
  unary("REALPART", (x) => {
    if (!N.isNumber(x)) typeError(x, "number");
    return x instanceof N.Complex ? x.real : x;
  });
  unary("IMAGPART", (x) => {
    if (!N.isNumber(x)) typeError(x, "number");
    return x instanceof N.Complex
      ? x.imaginary
      : N.isFloat(x)
        ? N.coerceFloat(0, x)
        : 0;
  });
  unary("CONJUGATE", (x) => {
    if (!N.isNumber(x)) typeError(x, "number");
    return x instanceof N.Complex ? N.makeComplex(x.real, N.negate(x.imaginary)) : x;
  });
  for (const mode of ["floor", "ceiling", "truncate", "round"])
    for (const f of [false, true])
      def(`${f ? "F" : ""}${mode.toUpperCase()}`, 1, 2, (a) =>
        rt.multipleValues(...N.quotientRemainder(a[0], a[1] ?? 1, mode, f)),
      );
  def("MOD", 2, 2, (a) => N.quotientRemainder(a[0], a[1], "floor")[1]);
  def("REM", 2, 2, (a) => N.quotientRemainder(a[0], a[1], "truncate")[1]);
  def("ASH", 2, 2, (a) => {
    const n = integer(a[0]),
      shift = integer(a[1]);
    return boxInteger(shift >= 0n ? shiftIntegerLeft(n, shift) : n >> -shift);
  });
  const shiftIntegerLeft = (value, bits) => {
    try { return value << bits; }
    catch (error) {
      if (!(error instanceof RangeError)) throw error;
      throw new N.NumericError("STORAGE-CONDITION", "Result exceeds integer storage");
    }
  };
  unary("INTEGER-LENGTH", (x) => {
    let n = integer(x);
    if (n < 0n) n = ~n;
    return n.toString(2).length - (n === 0n ? 1 : 0);
  });
  for (const [op, identity, fn] of [
    ["LOGAND", -1n, (a, b) => a & b],
    ["LOGIOR", 0n, (a, b) => a | b],
    ["LOGXOR", 0n, (a, b) => a ^ b],
    ["LOGEQV", -1n, (a, b) => ~(a ^ b)],
  ])
    def(op, 0, Infinity, (a) =>
      boxInteger(a.map(integer).reduce(fn, identity)),
    );
  for (const [op, fn] of booleOperations.slice(10))
    def(`LOG${op}`, 2, 2, ([a, b]) => boxInteger(fn(integer(a), integer(b))));
  def("BOOLE", 3, 3, ([code, a, b]) => {
    if (!N.isInteger(code) || code < 0 || code > 15)
      return typeError(code, list(cl("INTEGER"), 0, 15));
    return boxInteger(booleOperations[code][1](integer(a), integer(b)));
  });
  unary("LOGNOT", (x) => boxInteger(~integer(x)));
  def("LOGTEST", 2, 2, (a) => truth((integer(a[0]) & integer(a[1])) !== 0n));
  const nonnegativeInteger = (value) => {
    if (!N.isInteger(value) || value < 0) typeError(value, "nonnegative integer");
    return integer(value);
  };
  def("LOGBITP", 2, 2, (a) =>
    truth(((integer(a[1]) >> nonnegativeInteger(a[0])) & 1n) !== 0n),
  );
  unary("LOGCOUNT", (x) => {
    let n = integer(x),
      count = 0;
    if (n < 0n) n = ~n;
    while (n) {
      n &= n - 1n;
      count++;
    }
    return count;
  });
  const gcd = (a, b) => {
    a = a < 0n ? -a : a;
    b = b < 0n ? -b : b;
    while (b) [a, b] = [b, a % b];
    return a;
  };
  def("GCD", 0, Infinity, (a) => boxInteger(a.map(integer).reduce(gcd, 0n)));
  def("LCM", 0, Infinity, (a) => {
    const result = a
      .map(integer)
      .reduce((a, b) => (a === 0n || b === 0n ? 0n : (a / gcd(a, b)) * b), 1n);
    return boxInteger(result < 0n ? -result : result);
  });
  def("EXPT", 2, 2, ([base, exponent]) => N.power(base, exponent));
  unary("ISQRT", N.integerSquareRoot);
  for (const op of ["SQRT", "EXP", "SIN", "COS", "TAN", "ASIN", "ACOS", "SINH", "COSH", "TANH", "ASINH", "ACOSH", "ATANH"])
    unary(op, (value) => N.transcendental(op, value));
  def("ATAN", 1, 2, ([y, x]) => {
    if (x === undefined) return N.transcendental("ATAN", y);
    const double = y instanceof N.DoubleFloat ? y : x instanceof N.DoubleFloat ? x : undefined;
    const format = double ? 2 : 1;
    return floatResult(Math.atan2(N.coerceFloat(y, format).value, N.coerceFloat(x, format).value), double);
  });
  def("LOG", 1, 2, ([value, base]) => N.logarithm(value, base));
  const cis = (value) => {
    const radians = N.coerceFloat(value, value instanceof N.DoubleFloat ? 2 : 1).value;
    return N.makeComplex(floatResult(Math.cos(radians), value), floatResult(Math.sin(radians), value));
  };
  unary("CIS", cis);
  unary("PHASE", (value) => {
    if (!N.isNumber(value)) typeError(value, "number");
    const re = value instanceof N.Complex ? value.real : value;
    const im = value instanceof N.Complex ? value.imaginary : 0;
    const format = re instanceof N.DoubleFloat ? 2 : 1;
    return floatResult(Math.atan2(N.coerceFloat(im, format).value, N.coerceFloat(re, format).value), re);
  });
  def("BYTE", 2, 2, ([size, position]) => {
    nonnegativeInteger(size);
    nonnegativeInteger(position);
    return new Cons(size, position);
  });
  const byteSpecParts = (value) => {
    if (!(value instanceof Cons)) typeError(value, "cons");
    return [nonnegativeInteger(value.car), nonnegativeInteger(value.cdr)];
  };
  unary("BYTE-SIZE", (value) => boxInteger(byteSpecParts(value)[0]));
  unary("BYTE-POSITION", (value) => boxInteger(byteSpecParts(value)[1]));
  const lowBits = (value, size) => {
    if (size === 0n) return 0n;
    if (value >= 0n && value >> size === 0n) return value;
    return value & (shiftIntegerLeft(1n, size) - 1n);
  };
  const field = (value, spec) => {
    const [size, position] = byteSpecParts(spec);
    return [integer(value) >> position, size, position];
  };
  def("LDB", 2, 2, ([spec, value]) => {
    const [shifted, size] = field(value, spec);
    return boxInteger(lowBits(shifted, size));
  });
  def("LDB-TEST", 2, 2, ([spec, value]) => {
    const [shifted, size] = field(value, spec);
    return truth(shifted !== 0n && (shifted & -shifted) >> size === 0n);
  });
  def("MASK-FIELD", 2, 2, ([spec, value]) => {
    const [shifted, size, position] = field(value, spec);
    return boxInteger(shiftIntegerLeft(lowBits(shifted, size), position));
  });
  for (const [op, positioned] of [["DPB", false], ["DEPOSIT-FIELD", true]])
    def(op, 3, 3, ([newValue, spec, target]) => {
      const replacement = integer(newValue);
      const [shifted, size, position] = field(target, spec);
      const changed = lowBits(shifted ^ (positioned ? replacement >> position : replacement), size);
      return boxInteger(integer(target) ^ shiftIntegerLeft(changed, position));
    });
}
