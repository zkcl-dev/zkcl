import { isNumber, isInteger, toBigInt, SingleFloat, DoubleFloat, NumericError, numericEql } from './numbers.mjs';

export const UNBOUND = Object.freeze({ kind: 'unbound' });
export const ARRAY_RANK_LIMIT = 128;
export const ARRAY_DIMENSION_LIMIT = 2 ** 32;
export const ARRAY_TOTAL_SIZE_LIMIT = 2 ** 32;
let emptyPlist = null;

function typeError(message, datum) {
  throw new NumericError('TYPE-ERROR', message, [datum]);
}

// A symbol's package is an identity supplied by its owning Runtime. The object
// itself owns the value and function slots; package lookup owns interning.
export class LispSymbol {
  constructor(name, packageIdentity = null) {
    if (typeof name !== 'string') typeError('Symbol name must be a string', name);
    this.name = name;
    this.package = packageIdentity;
    this.value = UNBOUND;
    this.function = UNBOUND;
    this.plist = emptyPlist;
    this.constant = false;
  }
}

export const NIL = new LispSymbol('NIL', 'COMMON-LISP');
emptyPlist = NIL;
export const T = new LispSymbol('T', 'COMMON-LISP');
for (const symbol of [NIL, T]) {
  symbol.value = symbol;
  symbol.plist = NIL;
  symbol.constant = true;
  Object.freeze(symbol);
}

export class Cons {
  constructor(car, cdr = NIL) { this.car = car; this.cdr = cdr; }
}

export class Cell {
  constructor(value = UNBOUND) { this.value = value; }
}

export class LispCode {
  constructor(entry, { name = null, arity = null } = {}) {
    if (entry === null || typeof entry !== 'object' || (typeof entry.step !== 'function' && !Array.isArray(entry.instructions))) typeError('Code metadata must contain a JavaScript step function or dynamic instructions', entry);
    this.entry = entry;
    this.name = name;
    this.arity = arity;
  }
}

export class Closure {
  constructor(code, environment = []) {
    if (!(code instanceof LispCode)) typeError('Closure code must be a LispCode', code);
    if (!Array.isArray(environment)) typeError('Closure environment must be an array', environment);
    this.code = code;
    this.environment = environment;
  }
}

function codePoint(value) {
  if (typeof value === 'string') {
    const characters = Array.from(value);
    if (characters.length !== 1) typeError('Expected one Unicode character', value);
    value = value.codePointAt(0);
  }
  if (!Number.isInteger(value) || value < 0 || value > 0x10ffff || (value >= 0xd800 && value <= 0xdfff)) {
    typeError('Character must be a Unicode scalar value', value);
  }
  return value;
}

const characterNames = new Map([
  [0, 'Null'], [8, 'Backspace'], [9, 'Tab'], [10, 'Newline'], [12, 'Page'],
  [13, 'Return'], [32, 'Space'], [127, 'Rubout'],
]);
const characterCodes = new Map([...characterNames].map(([code, name]) => [name.toUpperCase(), code]));
export const characterName = code => characterNames.get(code);
export const namedCharacterCode = name => characterCodes.get(name.toUpperCase());

export class LispCharacter {
  constructor(value) {
    this.codePoint = codePoint(value);
    Object.freeze(this);
  }
  toString() { return String.fromCodePoint(this.codePoint); }
}

/** A pathname's six Lisp components, independent of host filesystem access. */
export class LispPathname {
  constructor(components) {
    if (!Array.isArray(components) || components.length !== 6) {
      typeError('Expected six pathname components', components);
    }
    this.kind = 'pathname';
    this.components = Object.freeze(components.slice());
    Object.freeze(this);
  }
  get logical() { return this.components[0] instanceof LispString; }
}

function indexInBounds(index, size) {
  if (!Number.isSafeInteger(index) || index < 0 || index >= size) typeError('Array index out of bounds', index);
  return index;
}

// Strings store Unicode scalar values, so CHAR indexes characters rather than
// UTF-16 code units. Mutating a character does not change the string's length.
export class LispString {
  constructor(text = '') {
    if (typeof text !== 'string') typeError('String contents must be a host string', text);
    this.characters = Array.from(text, char => codePoint(char));
    this.dimensions = Object.freeze([this.characters.length]);
  }
  get length() { return this.characters.length; }
  get rank() { return 1; }
  get totalSize() { return this.characters.length; }
  get elementKind() { return 'character'; }
  get elementType() { return 'CHARACTER'; }
  get(index) { return new LispCharacter(this.characters[indexInBounds(index, this.length)]); }
  set(index, value) {
    if (!(value instanceof LispCharacter)) typeError('String element must be a character', value);
    this.characters[indexInBounds(index, this.length)] = value.codePoint;
    return value;
  }
  toString() { return this.characters.map(char => String.fromCodePoint(char)).join(''); }
}

function arrayElementKind(type) {
  const name = (type instanceof LispSymbol ? type.name : typeof type === 'string' ? type : '').toUpperCase();
  switch (name) {
    case 'BIT': return 'bit';
    case 'CHARACTER': case 'BASE-CHAR': case 'STANDARD-CHAR': return 'character';
    case 'OCTET': return 'octet';
    case 'SINGLEFLOAT': case 'SINGLE-FLOAT': case 'SHORT-FLOAT': return 'single';
    case 'DOUBLEFLOAT': case 'DOUBLE-FLOAT': case 'LONG-FLOAT': return 'double';
  }
  if (type instanceof Cons && type.car instanceof LispSymbol && type.car.name === 'UNSIGNED-BYTE'
      && type.cdr instanceof Cons && type.cdr.car === 8 && type.cdr.cdr === NIL) return 'octet';
  return 'general';
}

const arrayTypeNames = { general: 'T', bit: 'BIT', character: 'CHARACTER', octet: 'OCTET', single: 'SINGLE-FLOAT', double: 'DOUBLE-FLOAT' };
function checkArrayElement(kind, value) {
  let valid;
  switch (kind) {
    case 'general': return;
    case 'character': valid = value instanceof LispCharacter; break;
    case 'single': valid = value instanceof SingleFloat; break;
    case 'double': valid = value instanceof DoubleFloat; break;
    case 'bit': valid = isInteger(value) && (toBigInt(value) === 0n || toBigInt(value) === 1n); break;
    case 'octet': valid = isInteger(value) && toBigInt(value) >= 0n && toBigInt(value) <= 255n; break;
  }
  if (!valid) throw new NumericError('TYPE-ERROR', `Invalid ${arrayTypeNames[kind]} array element`, [value]);
}

function defaultArrayElement(kind) {
  if (kind === 'general') return NIL;
  if (kind === 'character') return new LispCharacter(0);
  if (kind === 'single') return new SingleFloat(0);
  if (kind === 'double') return new DoubleFloat(0);
  return 0;
}

export class LispArray {
  constructor(dimensions, { initialElement, initialContents, elementType = 'T', adjustable = false, fillPointer = null, displacedTo = null, displacedIndexOffset = 0 } = {}) {
    if (typeof dimensions === 'number') dimensions = [dimensions];
    if (!Array.isArray(dimensions)) typeError('Array dimensions must be nonnegative integers below ARRAY-DIMENSION-LIMIT', dimensions);
    dimensions = [...dimensions];
    if (!dimensions.every(n => Number.isInteger(n) && n >= 0 && n < ARRAY_DIMENSION_LIMIT)) typeError('Array dimensions must be nonnegative integers below ARRAY-DIMENSION-LIMIT', dimensions);
    if (dimensions.length >= ARRAY_RANK_LIMIT) typeError('Array rank must be below ARRAY-RANK-LIMIT', dimensions);
    const exactSize = dimensions.reduce((product, dimension) => product * BigInt(dimension), 1n);
    if (exactSize >= BigInt(ARRAY_TOTAL_SIZE_LIMIT)) typeError('Array total size must be below ARRAY-TOTAL-SIZE-LIMIT', dimensions);
    const size = Number(exactSize);
    this.dimensions = Object.freeze(dimensions);
    this._totalSize = size;
    this.elementKind = arrayElementKind(elementType);
    this.elementType = arrayTypeNames[this.elementKind];
    this.adjustable = Boolean(adjustable);
    this.fillPointer = fillPointer;
    this.displacedTo = displacedTo;
    this.displacedIndexOffset = displacedIndexOffset;
    if (fillPointer === true) this.fillPointer = size;
    if (this.fillPointer !== null && (dimensions.length !== 1 || !Number.isSafeInteger(this.fillPointer) || this.fillPointer < 0 || this.fillPointer > size)) typeError('Invalid fill pointer', fillPointer);
    if (!Number.isSafeInteger(displacedIndexOffset) || displacedIndexOffset < 0) typeError('Invalid array displacement', displacedIndexOffset);
    if (displacedTo !== null) {
      if (!(displacedTo instanceof LispArray || displacedTo instanceof LispString) || displacedIndexOffset + size > displacedTo.totalSize) typeError('Invalid displaced array', displacedTo);
      if (this.elementKind !== displacedTo.elementKind) typeError('Displaced arrays must have the same upgraded element type', displacedTo);
      if (initialContents !== undefined) throw new NumericError('PROGRAM-ERROR', 'Displaced arrays cannot have initial contents');
      this.storage = null;
    } else {
      if (initialElement === undefined) initialElement = defaultArrayElement(this.elementKind);
      if (initialContents === undefined) {
        checkArrayElement(this.elementKind, initialElement);
        this.storage = Array(size).fill(initialElement);
      } else {
        this.storage = Array.from(initialContents);
        for (const value of this.storage) checkArrayElement(this.elementKind, value);
      }
      if (this.storage.length !== size) throw new NumericError('PROGRAM-ERROR', 'Initial contents must match total array size');
    }
  }
  get rank() { return this.dimensions.length; }
  get totalSize() { return this._totalSize; }
  get length() {
    if (this.rank !== 1) typeError('Length requires a vector', this);
    return this.fillPointer ?? this.totalSize;
  }
  rowMajorIndex(indices) {
    if (indices.length !== this.rank) throw new NumericError('PROGRAM-ERROR', 'Wrong number of array subscripts');
    return indices.reduce((offset, index, axis) => offset * this.dimensions[axis] + indexInBounds(index, this.dimensions[axis]), 0);
  }
  get(index) {
    indexInBounds(index, this.totalSize);
    return this.displacedTo === null ? this.storage[index] : this.displacedTo.get(index + this.displacedIndexOffset);
  }
  set(index, value) {
    indexInBounds(index, this.totalSize);
    if (this.displacedTo === null) {
      checkArrayElement(this.elementKind, value);
      this.storage[index] = value;
    }
    else this.displacedTo.set(index + this.displacedIndexOffset, value);
    return value;
  }
  growVector(length) {
    if (!this.adjustable || this.rank !== 1) typeError('Expected an adjustable vector', this);
    if (length <= this.totalSize || length >= ARRAY_TOTAL_SIZE_LIMIT)
      throw new NumericError('STORAGE-CONDITION', 'Cannot extend vector to the requested length');
    const previousSize = this.totalSize;
    const storage = Array(length).fill(defaultArrayElement(this.elementKind));
    for (let i = 0; i < previousSize; i++) storage[i] = this.get(i);
    this.displacedTo = null;
    this.displacedIndexOffset = 0;
    this.dimensions = Object.freeze([length]);
    this._totalSize = length;
    this.storage = storage;
  }
  aref(...indices) { return this.get(this.rowMajorIndex(indices)); }
  aset(value, ...indices) { return this.set(this.rowMajorIndex(indices), value); }
}

export function fromArray(values, tail = NIL) {
  for (let i = values.length - 1; i >= 0; i--) tail = new Cons(values[i], tail);
  return tail;
}
export function list(...values) { return fromArray(values); }
export function toArray(value) {
  const values = [], seen = new Set();
  while (value !== NIL) {
    if (!(value instanceof Cons)) typeError('Expected a proper list', value);
    if (seen.has(value)) typeError('Expected an acyclic proper list', value);
    seen.add(value);
    values.push(value.car);
    value = value.cdr;
  }
  return values;
}

export function eq(left, right) { return left === right; }
export function eql(left, right) {
  if (left === right) return true;
  if (isNumber(left)) return numericEql(left, right);
  return left instanceof LispCharacter && right instanceof LispCharacter && left.codePoint === right.codePoint;
}
export function equal(left, right) {
  const pending = [[left, right]], seen = new Map();
  while (pending.length !== 0) {
    const [a, b] = pending.pop();
    if (eql(a, b)) continue;
    const aString = a instanceof LispString || a instanceof LispArray && a.rank === 1 && a.elementKind === 'character';
    const bString = b instanceof LispString || b instanceof LispArray && b.rank === 1 && b.elementKind === 'character';
    const aBits = a instanceof LispArray && a.rank === 1 && a.elementKind === 'bit';
    const bBits = b instanceof LispArray && b.rank === 1 && b.elementKind === 'bit';
    if ((aString && bString) || (aBits && bBits)) {
      if (a.length !== b.length) return false;
      for (let index = 0; index < a.length; index++) if (!eql(a.get(index), b.get(index))) return false;
    } else if (a instanceof LispPathname && b instanceof LispPathname) {
      if (a.logical !== b.logical) return false;
      for (let index = 0; index < 6; index++) pending.push([a.components[index], b.components[index]]);
    } else if (a instanceof Cons && b instanceof Cons) {
      let counterparts = seen.get(a);
      if (counterparts?.has(b)) continue;
      if (!counterparts) seen.set(a, counterparts = new Set());
      counterparts.add(b);
      pending.push([a.car, b.car], [a.cdr, b.cdr]);
    } else return false;
  }
  return true;
}
