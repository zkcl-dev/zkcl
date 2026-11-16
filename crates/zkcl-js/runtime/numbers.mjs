// Lisp integers are canonical: Number only for fixnums, BigInt otherwise.
// Host Number floats never serve as Lisp floats; the two float classes preserve
// format identity, signed zeros, and binary32 rounding at every operation.
export const FIXNUM_MIN = -(2 ** 28);
export const FIXNUM_MAX = 2 ** 28 - 1;
const MIN_BIG = BigInt(FIXNUM_MIN);
const MAX_BIG = BigInt(FIXNUM_MAX);
const scratch = new DataView(new ArrayBuffer(8));
const U64_MODULUS = 1n << 64n;
const U64_MASK = U64_MODULUS - 1n;
const INITIAL_RANDOM_WORDS = [
  0xE220A8397B1DCDAFn, 0x6E789E6AA1B965F4n, 0x06C45D188009454Fn, 0xF88BB8A8724C81ECn,
];

export class RandomState {
  #words;

  constructor(words = INITIAL_RANDOM_WORDS) {
    if (words instanceof RandomState) {
      this.#words = [...words.#words];
      return;
    }
    this.#words = [...words];
    if (this.#words.length !== 4 || this.#words.some(word => typeof word !== 'bigint' || word < 0n || word > U64_MASK))
      throw new TypeError('Expected four unsigned 64-bit random state words');
    if (this.#words.every(word => word === 0n)) throw new TypeError('Random state cannot be zero');
  }

  static seeded() {
    const seed = new Uint32Array(8);
    if (globalThis.crypto?.getRandomValues) globalThis.crypto.getRandomValues(seed);
    else for (let i = 0; i < seed.length; i++) seed[i] = Math.floor(Math.random() * 2 ** 32);
    const words = [0, 2, 4, 6].map(i => BigInt(seed[i]) | (BigInt(seed[i + 1]) << 32n));
    return new RandomState(words.some(word => word !== 0n) ? words : INITIAL_RANDOM_WORDS);
  }

  static fromHex(text) {
    if (text.length !== 64 || !/^[0-9A-Fa-f]{64}$/.test(text)) throw new TypeError('#Y requires a quoted 64-digit hexadecimal state');
    return new RandomState(text.match(/.{16}/g).map(word => BigInt(`0x${word}`)));
  }

  get words() { return [...this.#words]; }

  toHex() {
    return this.#words.map(word => word.toString(16).toUpperCase().padStart(16, '0')).join('');
  }

  #next() {
    // xoshiro256**. Keep the same state representation and transitions as the native runtime.
    const rotate = (word, bits) => ((word << bits) | (word >> (64n - bits))) & U64_MASK;
    let [a, b, c, d] = this.#words;
    const result = (rotate((b * 5n) & U64_MASK, 7n) * 9n) & U64_MASK;
    c ^= a;
    d ^= b;
    this.#words = [a ^ d, b ^ c, c ^ ((b << 17n) & U64_MASK), rotate(d, 45n)];
    return result;
  }

  #belowInteger(limit) {
    if (limit < U64_MODULUS) {
      // Remove the incomplete residue band before reducing modulo the bound.
      const threshold = U64_MODULUS % limit;
      for (;;) {
        const sample = this.#next();
        if (sample >= threshold) return sample % limit;
      }
    }
    const bits = (limit - 1n).toString(2).length;
    const mask = (1n << BigInt(bits)) - 1n;
    for (;;) {
      let sample = 0n;
      for (let shift = 0; shift < bits; shift += 64) sample |= this.#next() << BigInt(shift);
      sample &= mask;
      if (sample < limit) return sample;
    }
  }

  // The Lisp entry point validates the positive integer or float bound.
  sample(limit) {
    if (isInteger(limit)) return integer(this.#belowInteger(toBigInt(limit)));
    const single = limit instanceof SingleFloat;
    const digits = single ? 24 : 53;
    for (;;) {
      const fraction = Number(this.#next() >> BigInt(64 - digits)) * 2 ** -digits;
      const product = fraction * limit.value;
      const sample = single ? Math.fround(product) : product;
      if (sample < limit.value) return single ? new SingleFloat(sample) : new DoubleFloat(sample);
    }
  }
}

export class NumericError extends Error {
  constructor(kind, message, operands = []) {
    super(message);
    this.name = 'NumericError';
    this.kind = kind;
    this.operands = operands;
  }
}

function typeError(expected, value) {
  const error = new NumericError('TYPE-ERROR', `Expected ${expected}`, [value]);
  error.expectedType = expected;
  throw error;
}

function finite(value) {
  if (!Number.isFinite(value)) {
    throw new NumericError(Number.isNaN(value) ? 'FLOATING-POINT-INVALID-OPERATION' : 'FLOATING-POINT-OVERFLOW', 'Non-finite floating point result');
  }
  return value;
}

export function integer(value) {
  if (typeof value === 'number') {
    if (!Number.isSafeInteger(value)) typeError('an exact host integer', value);
    return value >= FIXNUM_MIN && value <= FIXNUM_MAX ? (value === 0 ? 0 : value) : BigInt(value);
  }
  if (typeof value === 'string' && /^[+-]?\d+$/.test(value)) value = BigInt(value);
  if (typeof value !== 'bigint') typeError('an integer', value);
  return value >= MIN_BIG && value <= MAX_BIG ? Number(value) : value;
}

export function isInteger(value) {
  return typeof value === 'bigint' || (typeof value === 'number' && Number.isInteger(value) && value >= FIXNUM_MIN && value <= FIXNUM_MAX);
}

export function toBigInt(value) {
  if (!isInteger(value)) typeError('an integer', value);
  return typeof value === 'bigint' ? value : BigInt(value);
}

export class SingleFloat {
  constructor(value) {
    if (typeof value !== 'number') typeError('a host Number', value);
    this.value = finite(Math.fround(value));
    Object.freeze(this);
  }
  get bits() {
    scratch.setFloat32(0, this.value, false);
    return scratch.getUint32(0, false);
  }
}

export class DoubleFloat {
  constructor(value) {
    if (typeof value !== 'number') typeError('a host Number', value);
    this.value = finite(value);
    Object.freeze(this);
  }
  get bits() {
    scratch.setFloat64(0, this.value, false);
    return scratch.getBigUint64(0, false);
  }
}

export function singleFromBits(bits) {
  if (!Number.isInteger(bits) || bits < 0 || bits > 0xffffffff) typeError('32-bit float bits', bits);
  scratch.setUint32(0, bits, false);
  return new SingleFloat(scratch.getFloat32(0, false));
}

export function doubleFromBits(bits) {
  bits = typeof bits === 'string' && /^\d+$/.test(bits) ? BigInt(bits) : bits;
  if (typeof bits !== 'bigint' || bits < 0n || bits > 0xffffffffffffffffn) typeError('64-bit float bits', bits);
  scratch.setBigUint64(0, bits, false);
  return new DoubleFloat(scratch.getFloat64(0, false));
}

function gcd(a, b) {
  a = a < 0n ? -a : a;
  b = b < 0n ? -b : b;
  while (b !== 0n) [a, b] = [b, a % b];
  return a;
}

export class Ratio {
  constructor(numerator, denominator) {
    const n = toBigInt(numerator), d = toBigInt(denominator);
    // Construct through makeRatio when reduction might yield an integer.
    if (d <= 1n || n === 0n || gcd(n, d) !== 1n) {
      throw new NumericError('TYPE-ERROR', 'Use makeRatio to construct and normalize a ratio', [numerator, denominator]);
    }
    this.numerator = integer(n);
    this.denominator = integer(d);
    Object.freeze(this);
  }
}

export function makeRatio(numerator, denominator) {
  let n = toBigInt(numerator), d = toBigInt(denominator);
  if (d === 0n) throw new NumericError('DIVISION-BY-ZERO', 'Zero denominator', [numerator, denominator]);
  if (d < 0n) { n = -n; d = -d; }
  const factor = gcd(n, d);
  n /= factor;
  d /= factor;
  return d === 1n ? integer(n) : new Ratio(integer(n), integer(d));
}

export function isFloat(value) { return value instanceof SingleFloat || value instanceof DoubleFloat; }
export function isReal(value) { return isInteger(value) || value instanceof Ratio || isFloat(value); }
export function isNumber(value) { return isReal(value) || value instanceof Complex; }
export function isRational(value) { return isInteger(value) || value instanceof Ratio; }

function real(value) {
  if (!isReal(value)) typeError('a real number', value);
  return value;
}

function precision(value) {
  if (value instanceof Complex) return Math.max(precision(value.real), precision(value.imaginary));
  real(value);
  return value instanceof DoubleFloat ? 2 : value instanceof SingleFloat ? 1 : 0;
}

// Return the exact rational denoted by a finite binary float, not a decimal
// approximation or a coercion of a neighboring bignum to Number.
export function rationalParts(value) {
  real(value);
  if (isInteger(value)) return [toBigInt(value), 1n];
  if (value instanceof Ratio) return [toBigInt(value.numerator), toBigInt(value.denominator)];
  scratch.setFloat64(0, value.value, false);
  const bits = scratch.getBigUint64(0, false);
  const negative = (bits >> 63n) !== 0n;
  const exponent = Number((bits >> 52n) & 0x7ffn);
  let significand = bits & 0xfffffffffffffn;
  let shift;
  if (exponent === 0) shift = -1074;
  else { significand |= 1n << 52n; shift = exponent - 1023 - 52; }
  if (negative) significand = -significand;
  if (shift >= 0) return [significand << BigInt(shift), 1n];
  const denominator = 1n << BigInt(-shift);
  const factor = gcd(significand, denominator);
  return [significand / factor, denominator / factor];
}

function roundedQuotient(numerator, denominator) {
  const quotient = numerator / denominator;
  const twiceRemainder = (numerator % denominator) * 2n;
  return quotient + (twiceRemainder > denominator || (twiceRemainder === denominator && (quotient & 1n) !== 0n) ? 1n : 0n);
}

function binaryExponent(numerator, denominator) {
  const estimate = numerator.toString(2).length - denominator.toString(2).length;
  const below = estimate >= 0
    ? numerator < (denominator << BigInt(estimate))
    : (numerator << BigInt(-estimate)) < denominator;
  return below ? estimate - 1 : estimate;
}

// Convert directly from the exact fraction to the destination binary format.
// Number(n)/Number(d) can overflow both operands or round twice for binary32.
function rationalToFloat(numerator, denominator, format) {
  if (numerator === 0n) return 0;
  const negative = numerator < 0n;
  if (negative) numerator = -numerator;
  const digits = format === 1 ? 24 : 53;
  const minExponent = format === 1 ? -126 : -1022;
  const maxExponent = format === 1 ? 127 : 1023;
  const exponent = binaryExponent(numerator, denominator);
  if (exponent > maxExponent) finite(Infinity);
  const scaleExponent = Math.max(exponent, minExponent) - (digits - 1);
  const significand = scaleExponent <= 0
    ? roundedQuotient(numerator << BigInt(-scaleExponent), denominator)
    : roundedQuotient(numerator, denominator << BigInt(scaleExponent));
  const result = Number(significand) * (2 ** scaleExponent);
  const signed = negative ? -result : result;
  return finite(format === 1 ? Math.fround(signed) : signed);
}

export function toNumber(value) {
  real(value);
  if (isFloat(value)) return value.value;
  const [n, d] = rationalParts(value);
  return rationalToFloat(n, d, 2);
}

export function coerceFloat(value, format = 1) {
  real(value);
  if (format instanceof DoubleFloat || format === 'double') format = 2;
  else if (format instanceof SingleFloat || format === 'single') format = 1;
  if (format !== 1 && format !== 2) typeError('single or double float format', format);
  const number = isFloat(value) ? value.value : rationalToFloat(...rationalParts(value), format);
  return format === 1 ? new SingleFloat(number) : new DoubleFloat(number);
}

export function floatDigits(value) {
  if (!isFloat(value)) typeError('a float', value);
  return value instanceof SingleFloat ? 24 : 53;
}

function floatParts(value) {
  const digits = floatDigits(value);
  const single = digits === 24;
  const fractionBits = BigInt(digits - 1);
  const implicitBit = 1n << fractionBits;
  const bits = BigInt(value.bits);
  const storedExponent = Number((bits >> fractionBits) & (single ? 0xffn : 0x7ffn));
  const fraction = bits & (implicitBit - 1n);
  const sign = value.value < 0 || Object.is(value.value, -0) ? -1 : 1;
  const bias = single ? 127 : 1023;
  if (storedExponent !== 0) return {
    significand: fraction | implicitBit, exponent: storedExponent - bias + 1 - digits,
    sign, digits, precision: digits,
  };
  if (fraction === 0n) return { significand: 0n, exponent: 0, sign, digits, precision: 0 };
  const precision = fraction.toString(2).length;
  const shift = digits - precision;
  return {
    significand: fraction << BigInt(shift), exponent: 2 - bias - digits - shift,
    sign, digits, precision,
  };
}

export function decodeFloat(value, asInteger) {
  const { significand, exponent, sign, digits } = floatParts(value);
  if (asInteger) return [integer(significand), exponent, sign];
  const Float = digits === 24 ? SingleFloat : DoubleFloat;
  return [new Float(Number(significand) * 2 ** -digits), significand === 0n ? 0 : exponent + digits, new Float(sign)];
}

export function floatPrecision(value) { return floatParts(value).precision; }

export function scaleFloat(value, scale) {
  const amount = toBigInt(scale);
  const { significand, exponent, sign, digits } = floatParts(value);
  if (significand === 0n || amount === 0n) return value;
  const single = digits === 24;
  const Float = single ? SingleFloat : DoubleFloat;
  const shifted = BigInt(exponent) + amount;
  // Bound the shift before building the exact fraction. Subnormal results
  // need a single rounding in their destination format.
  const leastExponent = single ? -149 : -1074;
  if (shifted < BigInt(leastExponent - digits)) return new Float(sign < 0 ? -0 : 0);
  const greatestExponent = single ? 127 : 1023;
  if (shifted + BigInt(digits - 1) > BigInt(greatestExponent)) finite(Infinity);
  const signed = BigInt(sign) * significand;
  const [numerator, denominator] = shifted >= 0n ? [signed << shifted, 1n] : [signed, 1n << -shifted];
  return new Float(rationalToFloat(numerator, denominator, single ? 1 : 2));
}

// Descend through the common continued-fraction terms of two exact rational
// bounds. The first integer strictly inside the remaining interval gives the
// simplest fraction strictly inside the original interval.
function simplestBetween(lower, upper) {
  let [lowN, lowD] = lower, [highN, highD] = upper;
  const terms = [];
  let whole;
  for (;;) {
    whole = lowN / lowD;
    if (highD === 0n || (whole + 1n) * highD < highN) break;
    terms.push(whole);
    [lowN, lowD, highN, highD] = [highD, highN - whole * highD, lowD, lowN - whole * lowD];
  }
  let numerator = whole + 1n, denominator = 1n;
  for (let i = terms.length - 1; i >= 0; i--)
    [numerator, denominator] = [terms[i] * numerator + denominator, numerator];
  return [numerator, denominator];
}

export function rationalize(value) {
  real(value);
  if (!isFloat(value)) return value;
  const single = value instanceof SingleFloat;
  if (Number.isSafeInteger(value.value) && (!single || Math.abs(value.value) <= 2 ** 24))
    return integer(value.value);
  const negative = value.value < 0;
  const bits = BigInt(value.bits) & (single ? 0x7fffffffn : 0x7fffffffffffffffn);
  if (bits === (single ? 0x7f7fffffn : 0x7fefffffffffffffn))
    return makeRatio(...rationalParts(value).map(integer));
  const fromBits = single ? word => singleFromBits(Number(word)) : doubleFromBits;
  const exact = rationalParts(fromBits(bits));
  const midpoint = ([n, d], [otherN, otherD]) => [n * otherD + otherN * d, 2n * d * otherD];
  const lower = midpoint(exact, rationalParts(fromBits(bits - 1n)));
  const upper = midpoint(exact, rationalParts(fromBits(bits + 1n)));
  const [numerator, denominator] = simplestBetween(lower, upper);
  return makeRatio(integer(negative ? -numerator : numerator), integer(denominator));
}

export class Complex {
  constructor(realPart, imaginaryPart) {
    real(realPart); real(imaginaryPart);
    const format = Math.max(precision(realPart), precision(imaginaryPart));
    if (format === 0 && isZero(imaginaryPart)) throw new NumericError('TYPE-ERROR', 'Use makeComplex to reduce a rational zero imaginary part', [realPart, imaginaryPart]);
    this.real = format === 0 ? realPart : coerceFloat(realPart, format);
    this.imaginary = format === 0 ? imaginaryPart : coerceFloat(imaginaryPart, format);
    Object.freeze(this);
  }
}

export function makeComplex(realPart, imaginaryPart = 0) {
  real(realPart); real(imaginaryPart);
  if (isRational(realPart) && isRational(imaginaryPart) && isZero(imaginaryPart)) return realPart;
  return new Complex(realPart, imaginaryPart);
}

function sqrtInteger(n) {
  if (n < 2n) return n;
  let root = 1n << BigInt(Math.ceil(n.toString(2).length / 2));
  for (;;) {
    const next = (root + n / root) >> 1n;
    if (next >= root) return root;
    root = next;
  }
}

export function integerSquareRoot(value) {
  if (!isInteger(value) || value < 0) typeError('a nonnegative integer', value);
  return integer(sqrtInteger(toBigInt(value)));
}

function sqrtRational(numerator, denominator) {
  if (numerator === 0n) return 0;
  const exponent = Math.floor(binaryExponent(numerator, denominator) / 2);
  if (exponent > 127) finite(Infinity);
  if (exponent < -150) return 0;
  const scale = Math.max(exponent, -126) - 23;
  const shift = BigInt(2 * scale);
  if (shift < 0n) numerator <<= -shift;
  else denominator <<= shift;
  const root = sqrtInteger(numerator / denominator);
  // Compare to the square of the midpoint between the two significands.
  const midpoint = denominator * (2n * root + 1n) ** 2n;
  const scaled = 4n * numerator;
  const roundUp = scaled > midpoint || (scaled === midpoint && (root & 1n) !== 0n);
  return Number(root + (roundUp ? 1n : 0n)) * 2 ** scale;
}

export function absolute(value) {
  checkedNumber(value);
  if (!(value instanceof Complex)) {
    if (value instanceof SingleFloat) return new SingleFloat(Math.abs(value.value));
    if (value instanceof DoubleFloat) return new DoubleFloat(Math.abs(value.value));
    return compare(value, 0) < 0 ? negate(value) : value;
  }
  const { real: re, imaginary: im } = value;
  if (isFloat(re)) {
    const Float = re instanceof SingleFloat ? SingleFloat : DoubleFloat;
    return new Float(Math.hypot(re.value, im.value));
  }
  const [n, d] = rationalParts(add(multiply(re, re), multiply(im, im)));
  const numerator = sqrtInteger(n), denominator = sqrtInteger(d);
  if (numerator ** 2n === n && denominator ** 2n === d)
    return makeRatio(integer(numerator), integer(denominator));
  return new SingleFloat(sqrtRational(n, d));
}

export function signum(value) {
  if (isZero(value)) return value;
  if (value instanceof Complex) return divide(value, absolute(value));
  const sign = compare(value, 0) < 0 ? -1 : 1;
  return isFloat(value) ? coerceFloat(sign, value) : sign;
}

function oneLike(value) {
  if (value instanceof Complex) {
    const { real: part } = value;
    return isFloat(part) ? makeComplex(coerceFloat(1, part), coerceFloat(0, part)) : 1;
  }
  return isFloat(value) ? coerceFloat(1, value) : 1;
}

function underflow() {
  throw new NumericError('FLOATING-POINT-UNDERFLOW', 'Floating point result underflowed');
}

function integerPower(base, exponent) {
  if (exponent === 0n) return oneLike(base);
  if (isZero(base)) {
    if (exponent < 0n) throw new NumericError('DIVISION-BY-ZERO', 'Zero cannot be raised to a negative power');
    return base;
  }
  if (isRational(base)) {
    let [n, d] = rationalParts(base);
    if (exponent < 0n) { [n, d] = [d, n]; exponent = -exponent; }
    try {
      return makeRatio(integer(n ** exponent), integer(d ** exponent));
    } catch (error) {
      if (!(error instanceof RangeError)) throw error;
      throw new NumericError('STORAGE-CONDITION', 'Exact power exceeds integer storage');
    }
  }
  if (exponent < 0n) {
    base = divide(base);
    if (isZero(base)) underflow();
    exponent = -exponent;
  }
  let result = oneLike(base);
  for (;;) {
    if (exponent & 1n) {
      result = multiply(result, base);
      if (isZero(result)) underflow();
    }
    exponent >>= 1n;
    if (exponent === 0n) return result;
    base = multiply(base, base);
    if (isZero(base)) underflow();
  }
}

const negativeSign = value => value < 0 || Object.is(value, -0);

function sqrtComplex(a, b) {
  if (a === 0 && b === 0) return [a, b];
  const scale = Math.max(Math.abs(a), Math.abs(b));
  const root = Math.sqrt(scale) * Math.sqrt((Math.abs(a / scale) + Math.hypot(a / scale, b / scale)) / 2);
  const other = (Math.abs(b) / root) / 2;
  if (a >= 0) return [root, negativeSign(b) ? -other : other];
  return [other, b < 0 ? -root : root];
}

function exactLogAbs(n, d) {
  if (n === 0n) throw new NumericError('DIVISION-BY-ZERO', 'Logarithm of zero');
  n = n < 0n ? -n : n;
  const nBits = n.toString(2).length, dBits = d.toString(2).length;
  if (Math.abs(nBits - dBits) <= 1) {
    // Compute the difference before converting ratios that round to one.
    const delta = n - d;
    const shift = dBits - (delta < 0n ? -delta : delta).toString(2).length;
    if (delta !== 0n && shift >= 1000) return [rationalToFloat(delta << BigInt(shift), d, 2), -shift];
    return [Math.log1p(rationalToFloat(delta, d, 2)), 0];
  }
  const nShift = Math.max(0, nBits - 53), dShift = Math.max(0, dBits - 53);
  const leading = Number(n >> BigInt(nShift)) / Number(d >> BigInt(dShift));
  return [Math.log(leading) + (nShift - dShift) * Math.LN2, 0];
}

function logMagnitude(a, b) {
  const large = Math.max(Math.abs(a), Math.abs(b));
  const small = Math.min(Math.abs(a), Math.abs(b));
  if (large === 0) throw new NumericError('DIVISION-BY-ZERO', 'Logarithm of zero');
  const radius = Math.hypot(a, b);
  if (radius >= 0.75 && radius <= 1.25 && small !== 0) {
    const [n, d] = rationalParts(new DoubleFloat(large));
    const [p, q] = rationalParts(new DoubleFloat(small));
    const denominator = d * d * q * q;
    const delta = n * n * q * q + p * p * d * d - denominator;
    return Math.log1p(rationalToFloat(delta, denominator, 2)) / 2;
  }
  return Math.log(large) + Math.log1p((small / large) ** 2) / 2;
}

function logComplex(a, b) { return [logMagnitude(a, b), Math.atan2(b, a)]; }

function asinhComplex(a, b) {
  if (a === 0) {
    if (Math.abs(b) <= 1) return [0, Math.asin(b)];
    return [Math.sign(b) * Math.acosh(Math.abs(b)), Math.sign(b) * Math.PI / 2];
  }
  if (a < 0) return asinhComplex(-a, -b).map(value => -value);
  if (Math.max(a, Math.abs(b)) > 2 ** 500) return [Math.LN2 + logMagnitude(a, b), Math.atan2(b, a)];
  const [x, y] = sqrtComplex(1 + a * a - b * b, 2 * a * b);
  return logComplex(a + x, b + y);
}

function atanhComplex(a, b) {
  const [re, im] = logComplex(1 + a, b);
  const [denRe, denIm] = logComplex(1 - a, 0 - b);
  return [(re - denRe) / 2, (im - denIm) / 2];
}

function tanComplex(a, b) {
  const sin = Math.sin(a), cos = Math.cos(a);
  if (Math.abs(b) > 20) return [4 * sin * cos * Math.exp(-2 * Math.abs(b)), Math.sign(b)];
  const sinh = Math.sinh(b), cosh = Math.cosh(b);
  const norm = cos * cos + sinh * sinh;
  return [sin * cos / norm, sinh * cosh / norm];
}

const complexFunctions = {
  SQRT: sqrtComplex,
  EXP: (a, b) => [Math.exp(a) * Math.cos(b), Math.exp(a) * Math.sin(b)],
  SIN: (a, b) => [Math.sin(a) * Math.cosh(b), Math.cos(a) * Math.sinh(b)],
  COS: (a, b) => [Math.cos(a) * Math.cosh(b), -Math.sin(a) * Math.sinh(b)],
  TAN: tanComplex,
  ASIN: (a, b) => { const [re, im] = asinhComplex(-b, a); return [im, -re]; },
  ACOS: (a, b) => { const [re, im] = complexFunctions.ASIN(a, b); return [Math.PI / 2 - re, -im]; },
  ATAN: (a, b) => { const [re, im] = atanhComplex(-b, a); return [im, -re]; },
  SINH: (a, b) => [Math.sinh(a) * Math.cos(b), Math.cosh(a) * Math.sin(b)],
  COSH: (a, b) => [Math.cosh(a) * Math.cos(b), Math.sinh(a) * Math.sin(b)],
  TANH: (a, b) => { const [re, im] = tanComplex(-b, a); return [im, -re]; },
  ASINH: asinhComplex,
  ACOSH: (a, b) => {
    const [x1, y1] = sqrtComplex(a / 2 + 0.5, b / 2);
    const [x2, y2] = sqrtComplex(a / 2 - 0.5, b / 2);
    return logComplex(x1 + x2, y1 + y2).map(value => 2 * value);
  },
  ATANH: atanhComplex,
};

export function transcendental(operation, value) {
  checkedNumber(value);
  if (operation === 'SQRT' && isRational(value)) {
    const [n, d] = rationalParts(value);
    if (n >= 0n) return new SingleFloat(sqrtRational(n, d));
    return makeComplex(new SingleFloat(0), new SingleFloat(sqrtRational(-n, d)));
  }
  const format = Math.max(1, precision(value));
  const Float = format === 2 ? DoubleFloat : SingleFloat;
  const [re, im] = components(value);
  const a = coerceFloat(re, format).value;
  const complex = value instanceof Complex;
  if (!complex) {
    const domain = operation === 'SQRT' ? a >= 0
      : operation === 'ASIN' || operation === 'ACOS' ? Math.abs(a) <= 1
      : operation === 'ACOSH' ? a >= 1
      : operation === 'ATANH' ? Math.abs(a) < 1
      : true;
    if (domain) {
      const result = new Float(Math[operation.toLowerCase()](a));
      if (operation === 'EXP' && result.value === 0) underflow();
      return result;
    }
  }
  const [x, y] = complexFunctions[operation](a, coerceFloat(im, format).value);
  return makeComplex(new Float(x), new Float(y));
}

function logParts(value) {
  const [re, im] = components(value);
  if (value instanceof Complex && isRational(re)) {
    const [n, d] = rationalParts(add(multiply(re, re), multiply(im, im)));
    const [magnitude, scale] = exactLogAbs(n, d);
    const absRe = absolute(re), absIm = absolute(im);
    const phase = compare(absRe, absIm) >= 0
      ? Math.atan2(toNumber(divide(im, absRe)), compare(re, 0))
      : Math.atan2(compare(im, 0), toNumber(divide(re, absIm)));
    return [magnitude / 2, phase, scale];
  }
  if (!(value instanceof Complex) && isRational(re)) {
    const [n, d] = rationalParts(re);
    const [magnitude, scale] = exactLogAbs(n, d);
    return [magnitude, n < 0n ? Math.PI : 0, scale];
  }
  return [...logComplex(toNumber(re), toNumber(im)), 0];
}

function scaleHostFloat(value, n) {
  if (n === 0 || value === 0) return value;
  return scaleFloat(new DoubleFloat(value), integer(n)).value;
}

export function logarithm(value, base) {
  checkedNumber(value);
  if (base !== undefined) checkedNumber(base);
  const format = base === undefined ? precision(value) : Math.max(precision(value), precision(base));
  const Float = format === 2 ? DoubleFloat : SingleFloat;
  if (base !== undefined && isZero(base)) return new Float(0);
  if (base !== undefined && !(value instanceof Complex) && !(base instanceof Complex) &&
      compare(base, 2) === 0 && compare(value, 0) > 0) {
    if (isFloat(value)) return new Float(Math.log2(value.value));
    if (isInteger(value)) {
      const n = toBigInt(value), shift = Math.max(0, n.toString(2).length - 53);
      return new Float(Math.log2(Number(n >> BigInt(shift))) + shift);
    }
  }
  let [re, im, scale] = logParts(value);
  if (im !== 0 && scale !== 0) { re = scaleHostFloat(re, scale); scale = 0; }
  if (base !== undefined) {
    let [c, d, denominatorScale] = logParts(base);
    if (d !== 0 && denominatorScale !== 0) { c = scaleHostFloat(c, denominatorScale); denominatorScale = 0; }
    if (d === 0) {
      if (c === 0) throw new NumericError('DIVISION-BY-ZERO', 'Logarithm base is one');
      re /= c;
      im /= c;
    } else {
      const n = makeComplex(new DoubleFloat(re), new DoubleFloat(im));
      const denominator = makeComplex(new DoubleFloat(c), new DoubleFloat(d));
      const result = divide(n, denominator);
      re = result.real.value;
      im = result.imaginary.value;
    }
    scale -= denominatorScale;
  }
  const result = new Float(scaleHostFloat(re, scale));
  return im !== 0 || value instanceof Complex || base instanceof Complex
    ? makeComplex(result, new Float(scaleHostFloat(im, scale))) : result;
}

export function power(base, exponent) {
  checkedNumber(base);
  checkedNumber(exponent);
  if (isInteger(exponent)) return integerPower(base, toBigInt(exponent));
  const format = Math.max(precision(base), precision(exponent));
  const Float = format === 2 ? DoubleFloat : SingleFloat;
  const complex = base instanceof Complex || exponent instanceof Complex;
  const [re, im] = components(exponent);
  if (isZero(base)) {
    if (isZero(exponent)) return complex ? makeComplex(new Float(1), new Float(0)) : new Float(1);
    if (compare(re, 0) <= 0) throw new NumericError('DIVISION-BY-ZERO', 'Zero cannot be raised to this power');
    return complex ? (format === 0 ? 0 : makeComplex(new Float(0), new Float(0))) : new Float(0);
  }
  const a = toNumber(re), b = toNumber(im);
  if (a === 0 && b === 0) return complex ? makeComplex(new Float(1), new Float(0)) : new Float(1);
  if (!complex && (compare(base, 0) > 0 || Number.isInteger(a))) {
    let approximate;
    try { approximate = toNumber(base); }
    catch (error) {
      if (!(error instanceof NumericError) || error.kind !== 'FLOATING-POINT-OVERFLOW') throw error;
    }
    let magnitude;
    if (approximate) magnitude = Math.pow(approximate, a);
    else {
      const [log, , scale] = logParts(base);
      magnitude = Math.exp(a * scaleHostFloat(log, scale));
      if (compare(base, 0) < 0 && Math.abs(a % 2) === 1) magnitude = -magnitude;
    }
    const result = new Float(magnitude);
    if (result.value === 0) underflow();
    return result;
  }
  const [log, phase, scale] = logParts(base);
  const logarithm = scaleHostFloat(log, scale);
  const magnitude = Math.exp(a * logarithm - b * phase), angle = a * phase + b * logarithm;
  const result = makeComplex(new Float(magnitude * Math.cos(angle)), new Float(magnitude * Math.sin(angle)));
  if (isZero(result)) underflow();
  return result;
}

export function isZero(value) {
  if (value instanceof Complex) return isZero(value.real) && isZero(value.imaginary);
  real(value);
  return isFloat(value) ? value.value === 0 : value instanceof Ratio ? false : value === 0 || value === 0n;
}

function components(value) {
  if (!isNumber(value)) typeError('a number', value);
  return value instanceof Complex ? [value.real, value.imaginary] : [value, 0];
}

function realBinary(left, right, operation) {
  const format = Math.max(precision(left), precision(right));
  if (operation === '/' && isZero(right)) throw new NumericError('DIVISION-BY-ZERO', 'Division by zero', [left, right]);
  if (format !== 0) {
    const a = coerceFloat(left, format).value, b = coerceFloat(right, format).value;
    const result = operation === '+' ? a + b : operation === '-' ? a - b : operation === '*' ? a * b : a / b;
    return format === 1 ? new SingleFloat(result) : new DoubleFloat(result);
  }
  const [a, b] = rationalParts(left), [c, d] = rationalParts(right);
  if (operation === '+') return makeRatio(integer(a * d + c * b), integer(b * d));
  if (operation === '-') return makeRatio(integer(a * d - c * b), integer(b * d));
  if (operation === '*') return makeRatio(integer(a * c), integer(b * d));
  return makeRatio(integer(a * d), integer(b * c));
}

function binary(left, right, operation) {
  if (!(left instanceof Complex) && !(right instanceof Complex)) return realBinary(left, right, operation);
  let [a, b] = components(left), [c, d] = components(right);
  const format = Math.max(precision(left), precision(right));
  if (format !== 0) [a, b, c, d] = [a, b, c, d].map(value => coerceFloat(value, format));
  if (operation === '+') return makeComplex(realBinary(a, c, '+'), realBinary(b, d, '+'));
  if (operation === '-') return makeComplex(realBinary(a, c, '-'), realBinary(b, d, '-'));
  if (operation === '*') return makeComplex(
    realBinary(realBinary(a, c, '*'), realBinary(b, d, '*'), '-'),
    realBinary(realBinary(a, d, '*'), realBinary(b, c, '*'), '+'));
  if (isZero(c) && isZero(d)) throw new NumericError('DIVISION-BY-ZERO', 'Division by zero', [left, right]);
  if (format !== 0) {
    const lower = format === 1 ? 2 ** -63 : 2 ** -511;
    const upper = Math.sqrt(format === 1 ? (2 - 2 ** -23) * 2 ** 127 : Number.MAX_VALUE) * 0.5;
    if (![a, b, c, d].every(value => value.value === 0 || (Math.abs(value.value) >= lower && Math.abs(value.value) <= upper))) {
      // The ordinary formula squares the denominator components. At the
      // format limits this can overflow or lose both products to underflow,
      // although the quotient is representable. Use their exact dyadic values
      // there, and round the final components to the selected float format.
      const exact = [a, b, c, d].map(value => makeRatio(...rationalParts(value).map(integer)));
      const result = binary(makeComplex(exact[0], exact[1]), makeComplex(exact[2], exact[3]), '/');
      const [re, im] = components(result);
      const [sa, sb, sc, sd] = [a, b, c, d].map(value => value.value === 0 ? value.value : Math.sign(value.value));
      const rounded = (value, sign) => isZero(value)
        ? format === 1 ? new SingleFloat(Object.is(sign, -0) ? -0 : 0) : new DoubleFloat(Object.is(sign, -0) ? -0 : 0)
        : coerceFloat(value, format);
      return makeComplex(rounded(re, sa * sc + sb * sd), rounded(im, sb * sc - sa * sd));
    }
  }
  const denominator = realBinary(realBinary(c, c, '*'), realBinary(d, d, '*'), '+');
  return makeComplex(
    realBinary(realBinary(realBinary(a, c, '*'), realBinary(b, d, '*'), '+'), denominator, '/'),
    realBinary(realBinary(realBinary(b, c, '*'), realBinary(a, d, '*'), '-'), denominator, '/'));
}

export function add(...values) { return values.length === 0 ? 0 : values.slice(1).reduce((a, b) => binary(a, b, '+'), checkedNumber(values[0])); }
export function multiply(...values) { return values.length === 0 ? 1 : values.slice(1).reduce((a, b) => binary(a, b, '*'), checkedNumber(values[0])); }
function checkedNumber(value) { if (!isNumber(value)) typeError('a number', value); return value; }
export function negate(value) {
  checkedNumber(value);
  if (value instanceof Complex) return makeComplex(negate(value.real), negate(value.imaginary));
  if (value instanceof SingleFloat) return new SingleFloat(-value.value);
  if (value instanceof DoubleFloat) return new DoubleFloat(-value.value);
  if (value instanceof Ratio) return makeRatio(integer(-toBigInt(value.numerator)), value.denominator);
  return integer(-toBigInt(value));
}
export function subtract(first, ...rest) {
  checkedNumber(first);
  return rest.length === 0 ? negate(first) : rest.reduce((a, b) => binary(a, b, '-'), first);
}
export function divide(first, ...rest) {
  checkedNumber(first);
  return rest.length === 0 ? binary(1, first, '/') : rest.reduce((a, b) => binary(a, b, '/'), first);
}

export function compare(left, right) {
  const [a, b] = rationalParts(left), [c, d] = rationalParts(right);
  const delta = a * d - c * b;
  return delta < 0n ? -1 : delta > 0n ? 1 : 0;
}
export function numericEqual(left, right) {
  const [a, b] = components(left), [c, d] = components(right);
  return compare(a, c) === 0 && compare(b, d) === 0;
}

export function numericEql(left, right) {
  if (!isNumber(left) || !isNumber(right)) return false;
  if (isInteger(left) || isInteger(right)) return isInteger(left) && isInteger(right) && toBigInt(left) === toBigInt(right);
  if (left.constructor !== right.constructor) return false;
  if (isFloat(left)) return left.bits === right.bits;
  if (left instanceof Ratio) return left.numerator === right.numerator && left.denominator === right.denominator;
  return numericEql(left.real, right.real) && numericEql(left.imaginary, right.imaginary);
}

// The two results of CL's quotient/remainder operations. The remainder is
// computed as n - q*d after contagion, preserving its required float format.
export function quotientRemainder(numerator, denominator = 1, mode = 'truncate', floatQuotient = false) {
  real(numerator); real(denominator);
  if (isZero(denominator)) throw new NumericError('DIVISION-BY-ZERO', 'Division by zero', [numerator, denominator]);
  let [a, b] = rationalParts(numerator), [c, d] = rationalParts(denominator);
  let n = a * d, den = b * c;
  if (den < 0n) { n = -n; den = -den; }
  let q = n / den;
  const r = n % den;
  if (mode === 'floor' && r < 0n) q--;
  else if (mode === 'ceiling' && r > 0n) q++;
  else if (mode === 'round') {
    const magnitude = r < 0n ? -r : r;
    if (magnitude * 2n > den || (magnitude * 2n === den && (q & 1n) !== 0n)) q += n < 0n ? -1n : 1n;
  } else if (!['floor', 'ceiling', 'truncate'].includes(mode)) typeError('a rounding mode', mode);
  const quotient = integer(q);
  const format = Math.max(precision(numerator), precision(denominator));
  const remainderExact = makeRatio(integer(a * d - q * c * b), integer(b * d));
  const remainder = format === 0 ? remainderExact : coerceFloat(remainderExact, format);
  return [floatQuotient ? coerceFloat(quotient, format || 1) : quotient, remainder];
}
