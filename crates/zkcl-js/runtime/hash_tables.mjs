import { Cons, LispArray, LispCharacter, LispPathname, LispString, LispSymbol, NIL, T, eq, eql, equal } from './objects.mjs';
import { Complex, DoubleFloat, Ratio, SingleFloat, isInteger, isNumber, numericEqual, rationalParts } from './numbers.mjs';

export const hashTests = Object.freeze({ EQ: eq, EQL: eql, EQUAL: equal, EQUALP: equalp });
const identities = new WeakMap();
let lastIdentity = 0;
const identity = object => {
  let id = identities.get(object);
  if (id === undefined) identities.set(object, id = ++lastIdentity);
  return id;
};
const isString = value => value instanceof LispString || value instanceof LispArray && value.rank === 1 && value.elementKind === 'character';
const isObject = value => value !== null && (typeof value === 'object' || typeof value === 'function');
const stringText = value => value instanceof LispString ? value.toString()
  : Array.from({ length: value.length }, (_, i) => value.get(i).toString()).join('');
function foldCharacter(character) {
  const lower = character.toLowerCase();
  return lower.toUpperCase() === character && [...lower].length === 1 ? lower : character;
}

// Use a finite unfolding, independent of which nodes are shared by identity.
// EQUAL regards e.g. a one-cell cycle and its doubled cycle as equivalent.
function structuralHash(value, test, portable = false) {
  let hash = 0x811c9dc5;
  let remaining = 128;
  const word = n => { hash = Math.imul(hash ^ n, 0x01000193) >>> 0; };
  const text = s => {
    word(s.length);
    for (let i = 0; i < s.length; i++) word(s.charCodeAt(i));
  };
  const ratio = (numerator, denominator) => { text(String(numerator)); text(String(denominator)); };
  const number = (value, numericAcrossTypes) => {
    if (numericAcrossTypes) {
      const real = value instanceof Complex ? value.real : value;
      const imaginary = value instanceof Complex ? value.imaginary : 0;
      word(1); ratio(...rationalParts(real)); ratio(...rationalParts(imaginary));
    } else if (isInteger(value)) { word(2); text(String(value)); }
    else if (value instanceof SingleFloat) { word(3); word(value.bits); }
    else if (value instanceof DoubleFloat) { word(4); text(String(value.bits)); }
    else if (value instanceof Ratio) { word(5); ratio(value.numerator, value.denominator); }
    else { word(6); number(value.real, false); number(value.imaginary, false); }
  };
  function visit(value, depth, test) {
    if (depth === 0 || remaining-- <= 0) { word(0); return; }
    if (isNumber(value)) { number(value, test === equalp); return; }
    if (value instanceof LispCharacter) {
      word(7);
      word(test === equalp ? foldCharacter(value.toString()).codePointAt(0) : value.codePoint);
      return;
    }
    if (test !== eql) {
      if (value instanceof Cons) {
        word(8); visit(value.car, depth - 1, test); visit(value.cdr, depth - 1, test); return;
      }
      if (value instanceof LispPathname) {
        word(9); word(+value.logical);
        for (const component of value.components) visit(component, depth - 1, equal);
        return;
      }
      if (test !== equalp && isString(value)) { word(10); text(stringText(value)); return; }
      if ((value instanceof LispArray || value instanceof LispString) &&
          (test === equalp || value.rank === 1 && value.elementKind === 'bit')) {
        word(11); word(value.rank);
        for (const dimension of value.rank === 1 ? [value.length] : value.dimensions) word(dimension);
        const length = value.rank === 1 ? value.length : value.totalSize;
        for (let i = 0; i < length && remaining > 0; i++) visit(value.get(i), depth - 1, test);
        return;
      }
      if (test === equalp && value instanceof LispHashTable) { word(12); text(value.testName); return; }
      if (test === equalp && value?.kind === 'structure-instance') { word(13); word(identity(value.classObject)); return; }
    }
    if (portable && value instanceof LispSymbol) { word(14); text(value.name); return; }
    if (portable) { word(15); return; }
    if (isObject(value)) { word(16); word(identity(value)); return; }
    word(17); text(typeof value); text(String(value));
  }
  visit(value, 16, test);
  return hash;
}

export const sxhash = value => structuralHash(value, equal, true) & 0x0fffffff;

export class LispHashTable {
  #entries = new Set();
  #buckets = new Map();
  #testName;
  constructor(testName = 'EQL') {
    this.#testName = testName;
    this.rehashSize = new SingleFloat(1.5);
    this.rehashThreshold = new SingleFloat(1);
  }
  get testName() { return this.#testName; }
  get test() { return hashTests[this.#testName]; }
  get count() { return this.#entries.size; }
  entries() { return this.#entries.values(); }
  #hash(key) {
    if (this.testName === 'EQ' || key instanceof LispSymbol || this.testName === 'EQL' && isObject(key) && !isNumber(key) && !(key instanceof LispCharacter)) return key;
    return structuralHash(key, this.test);
  }
  #bucket(hash) {
    let bucket = this.#buckets.get(hash);
    if (!bucket) this.#buckets.set(hash, bucket = new Set());
    return bucket;
  }
  #index() {
    if (this.#buckets) return;
    this.#buckets = new Map();
    for (const pair of this.#entries) this.#bucket(this.#hash(pair[0])).add(pair);
  }
  restoreEntries(pairs) {
    this.#entries = new Set(pairs);
    this.#buckets = null;
  }
  keysPatched() { this.#buckets = null; }
  find(key, compare = this.test) {
    this.#index();
    const bucket = this.#buckets.get(this.#hash(key));
    if (bucket) for (const pair of bucket) if (compare(pair[0], key)) return pair;
  }
  get(key, fallback = NIL) {
    const pair = this.find(key);
    return pair ? [pair[1], T] : [fallback, NIL];
  }
  set(key, value) {
    this.#index();
    const bucket = this.#bucket(this.#hash(key));
    for (const pair of bucket) if (this.test(pair[0], key)) { pair[1] = value; return value; }
    const pair = [key, value];
    bucket.add(pair); this.#entries.add(pair);
    return value;
  }
  delete(key) {
    this.#index();
    const hash = this.#hash(key), bucket = this.#buckets.get(hash);
    if (bucket) for (const pair of bucket) if (this.test(pair[0], key)) {
      bucket.delete(pair); this.#entries.delete(pair);
      if (!bucket.size) this.#buckets.delete(hash);
      return true;
    }
    return false;
  }
  clear() {
    this.#entries.clear();
    this.#buckets = new Map();
    return this;
  }
}

export function equalp(a, b, ancestors) {
  const pending = [[a, b]], seen = new Map();
  const previouslyCompared = (left, right) => seen.get(left)?.has(right) || ancestors?.(left, right);
  while (pending.length) {
    const [x, y] = pending.pop();
    if (eql(x, y)) continue;
    if (x instanceof LispPathname || y instanceof LispPathname) { if (!equal(x, y)) return false; continue; }
    if (isNumber(x) && isNumber(y)) { if (!numericEqual(x, y)) return false; continue; }
    if (x instanceof LispCharacter && y instanceof LispCharacter) {
      if (foldCharacter(x.toString()) !== foldCharacter(y.toString())) return false;
      continue;
    }
    if (!isObject(x) || !isObject(y)) return false;
    if (previouslyCompared(x, y)) continue;
    let peers = seen.get(x);
    if (!peers) seen.set(x, peers = new Set());
    peers.add(y);
    if (x instanceof Cons && y instanceof Cons) { pending.push([x.car, y.car], [x.cdr, y.cdr]); continue; }
    if ((x instanceof LispArray || x instanceof LispString) && (y instanceof LispArray || y instanceof LispString)) {
      const xd = x.rank === 1 ? [x.length] : x.dimensions, yd = y.rank === 1 ? [y.length] : y.dimensions;
      if (xd.length !== yd.length || !xd.every((n, i) => n === yd[i])) return false;
      const length = x.rank === 1 ? x.length : x.totalSize;
      for (let i = 0; i < length; i++) pending.push([x.get(i), y.get(i)]);
      continue;
    }
    if (x instanceof LispHashTable && y instanceof LispHashTable) {
      if (x.testName !== y.testName || x.count !== y.count) return false;
      const compare = x.test === equalp ? (right, left) => equalp(left, right, previouslyCompared) : x.test;
      for (const [key, value] of x.entries()) {
        const pair = y.find(key, compare);
        if (!pair) return false;
        pending.push([value, pair[1]]);
      }
      continue;
    }
    if (x?.kind === 'structure-instance' && y?.kind === 'structure-instance') {
      if (x.classObject !== y.classObject || x.values.length !== y.values.length) return false;
      for (let i = 0; i < x.values.length; i++) pending.push([x.values[i], y.values[i]]);
      continue;
    }
    return false;
  }
  return true;
}
