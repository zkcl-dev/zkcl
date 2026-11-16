import * as N from "./numbers.mjs";

const units = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen"];
const tens = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];
const scales = ["", "thousand", "million", "billion", "trillion", "quadrillion", "quintillion", "sextillion", "septillion", "octillion", "nonillion", "decillion"];
const ordinals = { one: "first", two: "second", three: "third", five: "fifth", eight: "eighth", nine: "ninth", twelve: "twelfth" };
const oldRoman = [[1000, "M"], [500, "D"], [100, "C"], [50, "L"], [10, "X"], [5, "V"], [1, "I"]];
const roman = [[1000, "M"], [900, "CM"], [500, "D"], [400, "CD"], [100, "C"], [90, "XC"], [50, "L"], [40, "XL"], [10, "X"], [9, "IX"], [5, "V"], [4, "IV"], [1, "I"]];

function cardinalGroup(number) {
  const belowHundred = (n) => n < 20
    ? units[n]
    : tens[Math.floor(n / 10)] + (n % 10 ? `-${units[n % 10]}` : "");
  if (number < 100) return belowHundred(number);
  return `${units[Math.floor(number / 100)]} hundred` + (number % 100 ? ` ${belowHundred(number % 100)}` : "");
}

export function englishOrRoman(number, colon, atsign, fail) {
  if (atsign) {
    const limit = colon ? 4999n : 3999n;
    if (number <= 0n || number > limit) fail("Roman numeral is out of range");
    let remainder = Number(number), text = "";
    for (const [amount, digits] of colon ? oldRoman : roman) {
      text += digits.repeat(Math.floor(remainder / amount));
      remainder %= amount;
    }
    return text;
  }
  const sign = number < 0n ? "negative " : "";
  let magnitude = number < 0n ? -number : number;
  const words = [];
  for (let scale = 0; magnitude; scale++, magnitude /= 1000n) {
    if (scale >= scales.length) fail("Integer is out of range for English notation");
    const group = Number(magnitude % 1000n);
    if (group) words.push(cardinalGroup(group) + (scale ? ` ${scales[scale]}` : ""));
  }
  const text = words.reverse().join(" ") || "zero";
  return sign + (colon ? text.replace(/[a-z]+$/, (word) =>
    ordinals[word] ?? (word.endsWith("y") ? `${word.slice(0, -1)}ieth` : `${word}th`)) : text);
}

function power10(exponent, params) {
  // Keep decimal arithmetic inside the host's string and bigint limits.
  if (exponent > 1_000_000) params.fail("Decimal precision or scale is too large");
  return 10n ** BigInt(exponent);
}

function roundDecimal(numerator, denominator, scale, params) {
  if (scale >= 0) numerator *= power10(scale, params);
  else denominator *= power10(-scale, params);
  const quotient = numerator / denominator;
  const twiceRemainder = 2n * (numerator % denominator);
  return twiceRemainder > denominator || (twiceRemainder === denominator && quotient % 2n)
    ? quotient + 1n : quotient;
}

function decimalPoint(digits, fractional) {
  digits = digits.padStart(fractional + 1, "0");
  const point = digits.length - fractional;
  return `${digits.slice(0, point)}.${digits.slice(point)}`;
}

function shortestRealDecimal(value) {
  const magnitude = Math.abs(N.toNumber(value));
  if (value instanceof N.SingleFloat) {
    for (let digits = 1; digits < 9; digits++) {
      const decimal = Number(magnitude.toPrecision(digits));
      if (Math.fround(decimal) === magnitude) return decimal.toString();
    }
    return Number(magnitude.toPrecision(9)).toString();
  }
  return magnitude.toString();
}

function decimalParts(text) {
  const [mantissa, exp = "0"] = text.split("e");
  const point = mantissa.includes(".") ? mantissa.indexOf(".") : mantissa.length;
  const digits = mantissa.replace(".", "");
  const leadingZeroes = digits.length - digits.replace(/^0+/, "").length;
  return [digits.replace(/^0+|0+$/g, "") || "0", point + Number(exp) - leadingZeroes - 1];
}

/** Shortest round-trip float spelling, with the marker required by the reader. */
export function printFloat(value, defaultDouble, readably) {
  const double = value instanceof N.DoubleFloat;
  const marker = readably || double !== defaultDouble ? (double ? "d" : "f") : undefined;
  const negative = value.value < 0 || Object.is(value.value, -0);
  const sign = negative ? "-" : "";
  const magnitude = Math.abs(value.value);
  const shortest = shortestRealDecimal(value);
  if (magnitude === 0 || magnitude >= 1e-3 && magnitude < 1e7) {
    const text = shortest.includes(".") ? shortest : `${shortest}.0`;
    return sign + text + (marker ? `${marker}0` : "");
  }
  const [digits, exponent] = decimalParts(shortest);
  return `${sign}${digits[0]}.${digits.slice(1) || "0"}${marker ?? "e"}${exponent}`;
}

function decimalRational(digits, exponent, params) {
  const shift = exponent + 1 - digits.length;
  return shift >= 0
    ? [BigInt(digits) * power10(shift, params), 1n]
    : [BigInt(digits), power10(-shift, params)];
}

function decimalExponent(n, d, params) {
  const guess = n.toString().length - d.toString().length;
  const less = guess >= 0 ? n < d * power10(guess, params) : n * power10(-guess, params) < d;
  return less ? guess - 1 : guess;
}

function freePoint(digits, point, params) {
  if (Math.abs(point) > 1_000_000) params.fail("Decimal scale is too large");
  if (point <= 0) return `0.${"0".repeat(-point)}${digits}`;
  if (point >= digits.length) return `${digits.padEnd(point, "0")}.0`;
  return `${digits.slice(0, point)}.${digits.slice(point)}`;
}

function trimFraction(text) { return text.replace(/(\.[0-9]*?)0+$/, "$1").replace(/\.$/, ".0"); }
const chars = (text) => [...text].length;

function signedField(sign, body, width, overflow, padchar) {
  const padding = width === undefined ? 0 : width - chars(body) - sign.length;
  if (padding < 0 && overflow !== undefined) return overflow.repeat(width);
  return padchar.repeat(Math.max(0, padding)) + sign + body;
}

function fixed(number, { width, fraction, scale, overflow, padchar }, params) {
  const { n, d, sign, digits, exponent } = number;
  const fitted = fraction === undefined;
  let body;
  if (fitted && width === undefined) {
    body = n ? freePoint(digits, exponent + 1 + scale, params) : "0.0";
  } else {
    if (fitted) fraction = Math.max(0, width - sign.length - Math.max(1, exponent + scale + 1) - 1);
    body = decimalPoint(roundDecimal(n, d, fraction + scale, params).toString(), fraction);
    if (fitted) body = trimFraction(body);
  }
  if (width !== undefined && (width > 0 || fitted) && body.length + sign.length > width && body.startsWith("0."))
    body = body.slice(1);
  return signedField(sign, body, width, overflow, padchar);
}

function exponential(number, { width, fraction, expDigits, scale, overflow, padchar, marker }, params) {
  const { sign } = number;
  let exponent = number.n ? number.exponent - scale + 1 : 0;
  let fractionCount = fraction === undefined ? undefined : Math.max(0, scale > 0 ? 1 + fraction - scale : fraction);
  let body;
  const exponentText = () => `${marker}${exponent < 0 ? "-" : "+"}${String(Math.abs(exponent)).padStart(expDigits, "0")}`;
  if (fractionCount === undefined && width === undefined) {
    body = freePoint(number.digits, number.n ? scale : 1, params);
  } else {
    const fitted = fractionCount === undefined;
    if (fitted) fractionCount = Math.max(0, width - sign.length - chars(exponentText()) - Math.max(scale, 1) - 1);
    let rounded = roundDecimal(number.decimalN, number.decimalD, fractionCount - exponent, params).toString();
    if (number.n && rounded.length > scale + fractionCount && scale + fractionCount > 0) {
      rounded = rounded.slice(0, -1);
      exponent++;
    }
    body = decimalPoint(rounded, fractionCount);
    if (fitted) body = trimFraction(body);
    if (fitted && body.endsWith(".0") && body.length + sign.length + chars(exponentText()) > width)
      body = body.slice(0, -1);
  }
  if (scale <= 0 && width !== undefined && body.length + sign.length + chars(exponentText()) > width && body.startsWith("0."))
    body = body.slice(1);
  if (overflow !== undefined && width !== undefined && String(Math.abs(exponent)).length > expDigits && expDigits > 0)
    return overflow.repeat(width);
  return signedField(sign, body + exponentText(), width, overflow, padchar);
}

export function floatingField(op, params, value, options) {
  let [n, d] = N.rationalParts(value);
  const negative = n < 0n || (N.isFloat(value) && Object.is(value.value, -0));
  if (n < 0n) n = -n;
  const sign = negative ? "-" : params.node.atsign ? "+" : "";
  const floating = N.isFloat(value);
  const shortest = N.isInteger(value) ? n.toString() : shortestRealDecimal(value);
  let [digits, exponent] = Number.isFinite(Number(shortest)) || N.isInteger(value)
    ? decimalParts(shortest) : ["0", 0];
  if (!n) exponent = 0;
  // A ratio can lie outside the range of a JavaScript number.
  if (n && digits === "0") {
    exponent = decimalExponent(n, d, params);
    digits = roundDecimal(n, d, 15 - exponent, params).toString().replace(/0+$/, "");
  }
  const [decimalN, decimalD] = floating && n ? decimalRational(digits, exponent, params) : [n, d];
  const number = { n, d, sign, digits, exponent, decimalN, decimalD };
  if (op === "$") {
    const fraction = params.size(0, 2), minDigits = params.size(1, 1), width = params.size(2), padchar = params.character(3, " ");
    const rounded = roundDecimal(n, d, fraction, params).toString().padStart(fraction + minDigits, "0");
    const point = Math.max(0, rounded.length - fraction);
    const body = `${rounded.slice(0, point)}.${rounded.slice(point).padStart(fraction, "0")}`;
    const padding = padchar.repeat(Math.max(0, width - body.length - sign.length));
    return params.node.colon ? sign + padding + body : padding + sign + body;
  }
  const width = params.integer(0), fraction = params.integer(1);
  if (width !== undefined && width < 0 || fraction !== undefined && fraction < 0)
    params.fail("Width and fractional precision must be nonnegative");
  if (op === "F") {
    return fixed(number, { width, fraction, scale: params.integer(2, 0), overflow: params.character(3), padchar: params.character(4, " ") }, params);
  }
  const actualFormat = value instanceof N.DoubleFloat ? "double" : "single";
  const marker = params.character(6, actualFormat === options.defaultFloat || !floating ? "e" : actualFormat === "double" ? "d" : "f");
  const info = { width, fraction, expDigits: params.size(2), scale: params.integer(3, 1),
    overflow: params.character(4), padchar: params.character(5, " "), marker };
  if (op === "G") {
    const significant = fraction ?? Math.max(digits.length, Math.min(exponent + 1, 7));
    const places = significant - (exponent + 1);
    if (places >= 0 && places <= significant) {
      const expSpace = params.integer(2) === undefined ? 4 : info.expDigits + 2;
      return fixed(number, { ...info, width: width === undefined ? undefined : Math.max(0, width - expSpace),
        fraction: n ? places : fraction ?? 1, scale: 0 }, params) + " ".repeat(expSpace);
    }
    info.fraction = significant;
  }
  return exponential(number, info, params);
}
