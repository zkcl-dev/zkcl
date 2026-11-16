import { NIL, T, LispSymbol, Cons, LispString, LispCharacter, LispArray, UNBOUND, ARRAY_RANK_LIMIT, fromArray, toArray, namedCharacterCode } from './objects.mjs';
import { integer, SingleFloat, DoubleFloat, RandomState, makeRatio, makeComplex, isReal } from './numbers.mjs';
import { CallRequest, LispCondition, NativeFunction } from './execution.mjs';

export const EOF = Object.freeze({ type: 'reader-eof' });
const SKIP = Symbol('reader skip');
const standardSyntax = c => c === undefined ? 'eof' : ' \t\n\r\f'.includes(c) ? 'whitespace' : c === '\\' ? 'single-escape' : c === '|' ? 'multiple-escape' : '()\";\'`,#'.includes(c) ? 'macro' : 'constituent';

export class ReaderError extends LispCondition {
  constructor(message, position, kind = 'READER-ERROR', stream) { super(kind, `${message} at character ${position}`, { position, conditionClass: kind, kind, stream }); this.name = 'ReaderError'; }
}

// A cursor is shared with user reader macros, so recursive READ consumes the
// same input and read-time evaluation uses the same runtime as ordinary EVAL.
export class Reader {
  constructor(runtime, source, options = {}) {
    this.runtime = runtime;
    this.source = String(source);
    if (/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(this.source)) throw new ReaderError('invalid Unicode scalar', 0);
    this.characterOffsets = [0]; let offset = 0;
    for (const character of this.source) { offset += character.length; this.characterOffsets.push(offset); }
    const start = options.start ?? 0, end = options.end ?? this.characterOffsets.length - 1;
    if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 0 || end < start || end >= this.characterOffsets.length) throw new ReaderError('invalid string bounds', start);
    this.index = this.characterOffsets[start];
    this.end = this.characterOffsets[end];
    this.options = options;
    this.labels = new Map();
    this.depth = 0;
  }
  special(name, fallback) { const symbol = this.runtime.findSymbol?.(name, 'COMMON-LISP'); const value = symbol ? this.runtime.variableValue(symbol, this.runtime.activeTask) : UNBOUND; return value === UNBOUND || value === undefined ? fallback : value; }
  get table() { const table = this.special('*READTABLE*', this.runtime.readerTable); if (table !== undefined && !this.runtime.isReadtable?.(table)) throw new ReaderError('*READTABLE* requires a readtable', this.index, 'TYPE-ERROR'); return table; }
  get suppressed() { return this.options.suppress || this.special('*READ-SUPPRESS*', NIL) !== NIL; }
  syntax(c) { return this.table?.syntax(c) ?? standardSyntax(c); }
  delimiter(c) { const syntax = this.syntax(c), macro = this.table?.macros.get(c); return c === undefined || syntax === 'whitespace' || (syntax === 'macro' && !(macro?.nonterminating ?? c === '#')); }
  fail(message) { throw new ReaderError(message, this.index, 'READER-ERROR', this.options.stream ?? this); }
  peek(offset = 0) {
    this.options.stream?.checkOpen?.();
    let index = this.index;
    while (offset-- > 0 && index < this.end) index += this.source.codePointAt(index) > 0xffff ? 2 : 1;
    return index < this.end ? String.fromCodePoint(this.source.codePointAt(index)) : undefined;
  }
  take() { const c = this.peek(); if (c !== undefined) this.index += c.length; return c; }
  symbol(name, packageName) { return this.runtime.intern(name, packageName ?? this.special('*PACKAGE*', this.runtime.currentPackage)); }
  cl(name) { return this.symbol(name, 'COMMON-LISP'); }
  *peekSteps(offset = 0) {
    return this.options.cursor ? yield* this.options.cursor.peek(offset) : this.peek(offset);
  }
  *takeSteps() {
    if (!this.options.cursor) return this.take();
    const character = yield* this.options.cursor.take();
    if (character !== undefined) this.index += character.length;
    return character;
  }
  *unreadSteps(character) {
    if (this.options.cursor) yield* this.options.cursor.unread(character);
    else if (this.source.slice(this.index - character.length, this.index) !== character)
      throw new LispCondition('STREAM-ERROR', 'Cannot unread a different character', { stream: this.options.stream ?? this });
    this.index -= character.length;
    return NIL;
  }
  *whitespaceSteps() {
    let character;
    while ((character = yield* this.peekSteps()) !== undefined && this.syntax(character) === 'whitespace') yield* this.takeSteps();
  }
  macroResult(result) { return result.values.length ? result.values[0] : SKIP; }
  read(eofError = true, eofValue = EOF, recursive = false) { return runReader(this.runtime, 'READ', this.readSteps(eofError, eofValue, recursive)).values[0]; }
  readDelimited(delimiter, recursive = false) { return runReader(this.runtime, 'READ-DELIMITED-LIST', this.readDelimitedSteps(delimiter, recursive)).values[0]; }
  *readSteps(eofError = true, eofValue = EOF, recursive = false) {
    const previousLabels = this.labels, nested = this.depth > 0, outermost = !recursive || !nested;
    if (!recursive) this.labels = new Map();
    this.depth++;
    try {
      for (;;) {
        (yield* this.whitespaceSteps());
        if ((yield* this.peekSteps()) === undefined) { if (eofError) throw new ReaderError('end of input', this.index, 'END-OF-FILE', this.options.stream ?? this); return eofValue; }
        const value = yield* this.formSteps();
        if (value !== SKIP) return this.suppressed ? NIL : outermost ? this.resolveLabels(value) : value;
      }
    } finally { this.depth--; if (!recursive && nested) this.labels = previousLabels; }
  }
  *formSteps() {
    const c = yield* this.peekSteps();
    const descriptor = this.table?.macros.get(c);
    if (!descriptor && this.syntax(c) !== 'macro') return yield* this.tokenSteps();
    yield* this.takeSteps();
    if (descriptor) {
      if (descriptor.standard) return yield* this.standardSteps(descriptor.standard, c);
      if (descriptor.dispatch) return yield* this.dispatchSteps(c);
      return this.macroResult(yield new CallRequest(descriptor.fn, [this.options.stream ?? this, new LispCharacter(c.codePointAt(0))]));
    }
    return yield* this.standardSteps(c, c);
  }
  *standardSteps(c, actual = c) {
    switch (c) {
      case '(': return yield* this.listSteps();
      case ')': this.fail('unmatched closing parenthesis'); break;
      case '"': return new LispString(yield* this.stringSteps(actual));
      case ';': while ((yield* this.peekSteps()) !== undefined && (yield* this.takeSteps()) !== '\n') {} return SKIP;
      case "'": return fromArray([this.cl('QUOTE'), yield* this.readSteps(true, EOF, true)]);
      case '`': return fromArray([this.cl('QUASIQUOTE'), yield* this.readSteps(true, EOF, true)]);
      case ',': {
        const next = yield* this.peekSteps(), splice = next === '@', destructive = next === '.';
        if (splice || destructive) (yield* this.takeSteps());
        return fromArray([this.cl(splice ? 'UNQUOTE-SPLICING' : destructive ? 'UNQUOTE-NSPLICING' : 'UNQUOTE'), yield* this.readSteps(true, EOF, true)]);
      }
      case '#': return yield* this.dispatchSteps();
      default: this.fail('unsupported standard reader macro');
    }
  }
  *stringSteps(terminator = '"') {
    let result = '';
    for (;;) {
      let c = (yield* this.takeSteps());
      if (c === undefined) this.fail('unterminated string');
      if (c === terminator) return result;
      if (this.syntax(c) === 'single-escape') { c = (yield* this.takeSteps()); if (c === undefined) this.fail('unterminated string escape'); }
      result += c;
    }
  }
  *readDelimitedSteps(delimiter, recursive = false) {
    const previousLabels = this.labels, nested = this.depth > 0, outermost = !recursive || !nested, items = [];
    if (!recursive) this.labels = new Map();
    this.depth++;
    try {
      for (;;) { (yield* this.whitespaceSteps()); if ((yield* this.peekSteps()) === undefined) this.fail('unterminated delimited list'); if ((yield* this.peekSteps()) === delimiter) { (yield* this.takeSteps()); const result = this.suppressed ? NIL : fromArray(items); return outermost ? this.resolveLabels(result) : result; } const value = yield* this.formSteps(); if (value !== SKIP) items.push(value); }
    } finally { this.depth--; if (!recursive && nested) this.labels = previousLabels; }
  }
  *listSteps() {
    const items = [];
    for (;;) {
      (yield* this.whitespaceSteps());
      if ((yield* this.peekSteps()) === undefined) this.fail('unterminated list');
      if ((yield* this.peekSteps()) === ')' && (!this.table || this.table.macros.get(')')?.standard === ')')) { (yield* this.takeSteps()); return fromArray(items); }
      if ((yield* this.peekSteps()) === '.' && this.delimiter((yield* this.peekSteps(1)))) {
        if (!items.length) this.fail('dot requires a preceding list item');
        (yield* this.takeSteps());
        const tail = yield* this.readSteps(true, EOF, true);
        for (;;) { (yield* this.whitespaceSteps()); if ((yield* this.peekSteps()) === ')') { (yield* this.takeSteps()); break; } if ((yield* this.peekSteps()) === undefined || (yield* this.formSteps()) !== SKIP) this.fail('dotted list requires exactly one tail'); }
        return fromArray(items, tail);
      }
      const value = yield* this.formSteps();
      if (value !== SKIP) items.push(value);
    }
  }
  *tokenSteps(raw = false) {
    let text = '', escaped = false, bars = false;
    const protectedIndices = new Set();
    const separators = [];
    while ((yield* this.peekSteps()) !== undefined) {
      let c = (yield* this.peekSteps());
      if (!bars && this.delimiter(c)) break;
      (yield* this.takeSteps());
      if (this.syntax(c) === 'single-escape') { escaped = true; c = (yield* this.takeSteps()); if (c === undefined) this.fail('unterminated symbol escape'); protectedIndices.add(text.length); text += c; }
      else if (this.syntax(c) === 'multiple-escape') { escaped = true; bars = !bars; }
      else { if (!bars && !this.suppressed && /[\x00-\x20\x7f]/.test(c)) this.fail('invalid constituent character'); if (!bars && c === ':') separators.push(text.length); if (bars) protectedIndices.add(text.length); text += c; }
    }
    if (bars) this.fail('unterminated escaped symbol');
    if (!text.length && !escaped) this.fail('expected a token');
    const mode = this.table?.case ?? 'UPCASE';
    let tokenOffset = 0;
    const units = [...text].map(c => { const protectedUnit = protectedIndices.has(tokenOffset); tokenOffset += c.length; return { c, protectedUnit }; });
    const letters = units.filter(x => !x.protectedUnit && /[a-z]/i.test(x.c)).map(x => x.c);
    const invert = mode === 'INVERT' ? letters.every(c => c === c.toUpperCase()) ? 'DOWNCASE' : letters.every(c => c === c.toLowerCase()) ? 'UPCASE' : 'PRESERVE' : mode;
    text = units.map(({ c, protectedUnit }) => protectedUnit ? c : invert === 'UPCASE' ? c.replace(/[a-z]/g, x => x.toUpperCase()) : invert === 'DOWNCASE' ? c.replace(/[A-Z]/g, x => x.toLowerCase()) : c).join('');
    if (raw) return text;
    if (this.suppressed) return NIL;
    if (!escaped) {
      const number = this.number(text);
      if (number !== undefined) return number;
      if (/^\.+$/.test(text)) this.fail('invalid dot token');
    }
    if (!separators.length) return this.symbol(text);
    if (separators.length === 1 && separators[0] === 0) return this.symbol(text.slice(1), 'KEYWORD');
    const colon = separators[0], internal = separators.length === 2 && separators[1] === colon + 1;
    if (colon === 0 || (!internal && separators.length !== 1) || (internal && separators.length !== 2)) this.fail('invalid package marker');
    const packageName = text.slice(0, colon), name = text.slice(colon + (internal ? 2 : 1));
    if (!name.length) this.fail('empty package-qualified symbol');
    const pkg = this.runtime.findPackage(packageName);
    if (!pkg) this.fail(`unknown package ${packageName}`);
    if (!internal && this.runtime.findExternalSymbol) {
      const symbol = this.runtime.findExternalSymbol(name, pkg);
      if (!symbol) this.fail(`${name} is not external in ${packageName}`);
      return symbol;
    }
    return this.symbol(name, pkg);
  }
  number(text, radix = this.options.base ?? this.special('*READ-BASE*', 10), rationalOnly = false) {
    const ratio = /^([+-]?[0-9A-Z]+)\/([+-]?[0-9A-Z]+)$/i.exec(text);
    if (ratio) {
      const n = this.radixInteger(ratio[1], radix), d = this.radixInteger(ratio[2], radix);
      if (n !== undefined && d !== undefined) { if (d === 0n) this.fail('zero denominator'); return makeRatio(integer(n), integer(d)); }
    }
    if (/^[+-]?\d+\.$/.test(text)) return integer(text.slice(0, -1));
    const n = this.radixInteger(text, radix);
    if (n !== undefined) return integer(n);
    if (rationalOnly) return undefined;
    if (/^[+-]?(?:(?:\d+\.\d*|\.\d+)(?:[ESFDL][+-]?\d+)?|\d+[ESFDL][+-]?\d+)$/i.test(text)) {
      const value = Number(text.replace(/[ESFDL]/i, 'e')), marker = /[ESFDL]/i.exec(text)?.[0].toUpperCase();
      let double = marker === 'D' || marker === 'L';
      if (marker === undefined || marker === 'E') { const format = this.special('*READ-DEFAULT-FLOAT-FORMAT*', { name: 'SINGLE-FLOAT' })?.name; if (!['SHORT-FLOAT', 'SINGLE-FLOAT', 'DOUBLE-FLOAT', 'LONG-FLOAT'].includes(format)) this.fail('*READ-DEFAULT-FLOAT-FORMAT* requires a float type'); double = format === 'DOUBLE-FLOAT' || format === 'LONG-FLOAT'; }
      const result = double ? new DoubleFloat(value) : new SingleFloat(value);
      if (!Number.isFinite(result.value)) this.fail('floating-point literal is outside its format range');
      return result;
    }
    const base = text.includes('.') ? 10 : radix, chars = [...text];
    const digit = c => /^[0-9A-Z]$/i.test(c) && parseInt(c, 36) < base;
    const marker = c => /^[DESFL]$/i.test(c) && !digit(c);
    const potential = chars.length && (digit(chars[0]) || '+-.^'.includes(chars[0])) && !/[+-]$/.test(text) && chars.some(digit) && chars.every(c => digit(c) || marker(c) || '+-/.^_'.includes(c)) && !chars.some((c, i) => i && /[a-z]/i.test(c) && /[a-z]/i.test(chars[i - 1]) && (marker(c) || marker(chars[i - 1])));
    const interiorSign = chars.some((c, i) => i > 0 && '+-'.includes(c) && !(marker(chars[i - 1])));
    if (potential && !interiorSign) this.fail('invalid numeric token');
    return undefined;
  }
  radixInteger(text, radix) {
    if (!Number.isInteger(radix) || radix < 2 || radix > 36) this.fail('invalid read base');
    let negative = false, index = 0, n = 0n;
    if (text[0] === '+' || text[0] === '-') { negative = text[0] === '-'; index++; }
    if (index === text.length) return undefined;
    for (; index < text.length; index++) {
      const digit = parseInt(text[index], 36);
      if (!Number.isInteger(digit) || digit >= radix) return undefined;
      n = n * BigInt(radix) + BigInt(digit);
    }
    return negative ? -n : n;
  }
  *dispatchSteps(dispatchCharacter = '#') {
    let digits = '';
    while (/[0-9]/.test((yield* this.peekSteps()) ?? '') && (yield* this.peekSteps()) !== undefined) digits += (yield* this.takeSteps());
    const arg = digits ? integer(BigInt(digits)) : null, actual = (yield* this.takeSteps());
    const upper = actual?.toUpperCase(), c = upper && [...upper].length === 1 && upper.toLowerCase() === actual ? upper : actual;
    const descriptor = this.table?.macros.get(dispatchCharacter);
    const custom = descriptor?.dispatch?.get(c);
    if (descriptor?.dispatch && !custom && this.suppressed && c !== undefined && !['<', ')', ' ', '\t', '\n', '\r', '\f'].includes(c)) { while (!this.delimiter((yield* this.peekSteps()))) (yield* this.takeSteps()); return NIL; }
    if (descriptor?.dispatch && !custom) this.fail(`undefined dispatch reader character ${dispatchCharacter}${c ?? ''}`);
    if (custom && !custom.standardDispatch) return this.macroResult(yield new CallRequest(custom, [this.options.stream ?? this, new LispCharacter(actual.codePointAt(0)), arg ?? NIL]));
    return yield* this.standardDispatchSteps(custom?.standardDispatch ?? c, arg);
  }
  *standardDispatchSteps(c, arg) {
    if (!this.suppressed && arg !== null && !['R', '(', '*', 'A', '=', '#'].includes(c)) this.fail('dispatch reader does not accept a numeric argument');
    switch (c) {
      case "'": return fromArray([this.cl('FUNCTION'), yield* this.readSteps(true, EOF, true)]);
      case ':': { const token = yield* this.tokenSteps(true); return this.suppressed ? NIL : new LispSymbol(token, null); }
      case '\\': {
        let name = (yield* this.takeSteps());
        if (name === undefined) this.fail('missing character');
        if (!this.delimiter(name)) { while (!this.delimiter((yield* this.peekSteps()))) name += (yield* this.takeSteps()); }
        const code = [...name].length === 1 ? name.codePointAt(0)
          : name.toUpperCase() === 'LINEFEED' ? 10 : namedCharacterCode(name);
        if (this.suppressed) return NIL;
        if (code === undefined) this.fail(`unknown character ${name}`);
        return new LispCharacter(code);
      }
      case 'B': case 'O': case 'X': case 'R': {
        const radix = c === 'B' ? 2 : c === 'O' ? 8 : c === 'X' ? 16 : arg;
        if (radix === null && !this.suppressed) this.fail('#R requires a radix');
        const text = (yield* this.tokenSteps(true)); if (this.suppressed) return NIL; const n = this.number(text, radix, true);
        if (n === undefined) this.fail(`invalid base-${radix} number`);
        return n;
      }
      case '(': {
        const values = yield* this.listSteps(), array = [], seen = new Set();
        for (let x = values; x !== NIL; x = x.cdr) { if (!(x instanceof Cons) || seen.has(x)) this.fail('vector contents must be a proper list'); seen.add(x); array.push(x.car); }
        if (arg !== null && !this.suppressed) { if (array.length > arg || (arg > 0 && !array.length)) this.fail('invalid vector length'); while (array.length < arg) array.push(array.at(-1)); }
        return this.runtime.makeVector?.(array) ?? new LispArray([array.length], { initialContents: array });
      }
      case '*': {
        if (this.suppressed) { while (!this.delimiter((yield* this.peekSteps()))) (yield* this.takeSteps()); return NIL; }
        let bits = ''; while ((yield* this.peekSteps()) === '0' || (yield* this.peekSteps()) === '1') bits += (yield* this.takeSteps());
        if (!this.delimiter((yield* this.peekSteps()))) this.fail('invalid bit vector');
        if (arg !== null) { if (bits.length > arg || (arg > 0 && !bits.length)) this.fail('invalid bit vector length'); bits = bits.padEnd(arg, bits.at(-1)); }
        return this.runtime.makeBitVector?.([...bits].map(Number)) ?? new LispArray([bits.length], { initialContents: [...bits].map(Number), elementType: 'BIT' });
      }
      case 'A': {
        const contents = yield* this.readSteps(true, EOF, true); if (this.suppressed) return NIL;
        if (!Number.isSafeInteger(arg) || arg < 0 || arg >= ARRAY_RANK_LIMIT) this.fail('#A requires a valid array rank');
        const dimensions = [], values = [];
        const flatten = (value, axis) => {
          if (axis === arg) { values.push(value); return; }
          let elements;
          if (value instanceof LispArray && value.rank === 1 || value instanceof LispString) elements = Array.from({ length: value.length }, (_, i) => value.get(i));
          else { try { elements = toArray(value); } catch { this.fail('#A contents must be nested proper sequences'); } }
          if (dimensions[axis] === undefined) dimensions[axis] = elements.length;
          else if (dimensions[axis] !== elements.length) this.fail('#A contents have inconsistent dimensions');
          for (const element of elements) flatten(element, axis + 1);
        };
        flatten(contents, 0); while (dimensions.length < arg) dimensions.push(0);
        return new LispArray(dimensions, { initialContents: values });
      }
      case 'S': {
        const form = yield* this.readSteps(true, EOF, true); if (this.suppressed) return NIL;
        let elements; try { elements = toArray(form); } catch { this.fail('#S requires a proper structure description'); }
        const name = elements.shift(), definition = this.runtime.structureDefinition?.(name);
        if (!(name instanceof LispSymbol) || !definition || definition.representation || elements.length % 2) this.fail('invalid #S structure description');
        for (let i = 0; i < elements.length; i += 2) { const key = elements[i]; if (!(key instanceof LispSymbol || key instanceof LispString)) this.fail('invalid structure slot name'); const spelling = key instanceof LispSymbol ? key.name : key.toString(); if (!definition.slots.some(slot => slot.name.name === spelling)) this.fail('unknown structure slot'); elements[i] = this.runtime.intern(spelling, 'KEYWORD'); }
        return yield* this.runtime.constructStructureSteps(name, fromArray(elements), this.runtime.activeTask);
      }
      case 'P': {
        const value = yield* this.readSteps(true, EOF, true);
        if (this.suppressed) return NIL;
        return this.runtime.readPathname(value, this.runtime.activeTask, message => this.fail(message));
      }
      case 'Y': {
        const value = yield* this.readSteps(true, EOF, true);
        if (this.suppressed) return NIL;
        if (!(value instanceof LispString)) this.fail('#Y requires a quoted 64-digit hexadecimal state');
        try { return RandomState.fromHex(value.toString()); }
        catch (error) { this.fail(error.message); }
      }
      case 'C': {
        const pair = yield* this.readSteps(true, EOF, true);
        if (this.suppressed) return NIL;
        if (!(pair instanceof Cons) || !(pair.cdr instanceof Cons) || pair.cdr.cdr !== NIL) this.fail('#C requires two components');
        if (!isReal(pair.car) || !isReal(pair.cdr.car)) this.fail('#C components must be real numbers');
        return makeComplex(pair.car, pair.cdr.car);
      }
      case '.': {
        if (this.suppressed) { yield* this.readSteps(true, EOF, true); return NIL; }
        if (this.options.readEval === false || this.special('*READ-EVAL*', this.runtime.readEval ? T : NIL) === NIL) this.fail('read-time evaluation is disabled');
        const form = yield* this.readSteps(true, EOF, true);
        const result = yield* this.runtime.evalSteps(form);
        return result.values[0] ?? NIL;
      }
      case '|': {
        let nesting = 1;
        while (nesting) {
          const char = yield* this.takeSteps();
          if (char === undefined) this.fail('unterminated block comment');
          if (char === '#' && (yield* this.peekSteps()) === '|') { nesting++; yield* this.takeSteps(); }
          if (char === '|' && (yield* this.peekSteps()) === '#') { nesting--; yield* this.takeSteps(); }
        }
        return SKIP;
      }
      case ';': yield* this.readSteps(true, EOF, true); return SKIP;
      case '+': case '-': {
        const alreadySuppressed = this.suppressed;
        const task = this.runtime.activeTask;
        const packageSymbol = this.runtime.intern('*PACKAGE*', 'COMMON-LISP');
        task.bind(task.currentFrame, [[packageSymbol, this.runtime.findPackage('KEYWORD')]]);
        const feature = yield* this.readSteps(true, EOF, true);
        task.popScope('special');
        if (alreadySuppressed) { yield* this.readSteps(true, EOF, true); return NIL; }
        const selected = this.feature(feature) === (c === '+');
        if (selected) return yield* this.readSteps(true, EOF, true);
        // User macros still run with *READ-SUPPRESS* dynamically true.
        const suppressSymbol = this.runtime.intern('*READ-SUPPRESS*', 'COMMON-LISP');
        task.bind(task.currentFrame, [[suppressSymbol, T]]);
        yield* this.readSteps(true, EOF, true);
        task.popScope('special');
        return SKIP;
      }
      case '=': {
        if (this.suppressed) return SKIP;
        if (arg === null || this.labels.has(arg)) this.fail('invalid or duplicate reader label');
        const placeholder = { readerLabel: arg };
        this.labels.set(arg, placeholder);
        const value = yield* this.readSteps(true, EOF, true);
        if (value === placeholder) this.fail('reader label refers only to itself');
        this.labels.set(arg, value); return value;
      }
      case '#': {
        if (this.suppressed) return NIL;
        if (arg === null || !this.labels.has(arg)) this.fail('undefined reader label');
        return this.labels.get(arg);
      }
      default: this.fail(`unsupported dispatch reader character #${c ?? ''}`);
    }
  }
  feature(form) {
    if (form instanceof LispSymbol) {
      let features = this.special('*FEATURES*', this.runtime.features ?? ['JAVASCRIPT', 'ZKCL']);
      if (features instanceof Cons || features === NIL) { const out = []; for (let x = features; x !== NIL; x = x.cdr) { if (!(x instanceof Cons)) this.fail('invalid *FEATURES* list'); out.push(x.car); } features = out; }
      return features.some(x => (x.name ?? x).toUpperCase() === form.name.toUpperCase());
    }
    if (!(form instanceof Cons)) this.fail('invalid feature expression');
    const op = form.car?.name, args = [];
    for (let x = form.cdr; x !== NIL; x = x.cdr) { if (!(x instanceof Cons)) this.fail('invalid feature expression'); args.push(x.car); }
    if (op === 'AND') return args.every(x => this.feature(x));
    if (op === 'OR') return args.some(x => this.feature(x));
    if (op === 'NOT' && args.length === 1) return !this.feature(args[0]);
    this.fail('invalid feature expression');
  }
  resolveLabels(root) {
    const seen = new Set();
    const resolve = x => {
      if (x && Object.hasOwn(x, 'readerLabel')) { const value = this.labels.get(x.readerLabel); if (value === x) this.fail('unresolved reader label'); return value; }
      if (seen.has(x)) return x;
      if (x instanceof Cons) { seen.add(x); x.car = resolve(x.car); x.cdr = resolve(x.cdr); }
      else if (x instanceof LispArray) { seen.add(x); for (let i = 0; i < x.totalSize; i++) x.set(i, resolve(x.get(i))); }
      else if (x?.classObject?.structureDefinition && Array.isArray(x.values)) { seen.add(x); for (let i = 0; i < x.values.length; i++) x.values[i] = resolve(x.values[i]); }
      else if (Array.isArray(x)) { seen.add(x); for (let i = 0; i < x.length; i++) x[i] = resolve(x[i]); }
      return x;
    };
    return resolve(root);
  }
}

export function* readFromStringSteps(runtime, source, options = {}) {
  const reader = new Reader(runtime, source, options);
  const value = yield* reader.readSteps(options.eofError ?? true, options.eofValue ?? NIL);
  if (!options.preserveWhitespace && reader.syntax(reader.peek()) === 'whitespace' && reader.peek() !== undefined) reader.take();
  return runtime.multipleValues(value, reader.characterOffsets.indexOf(reader.index));
}
export function* readSteps(runtime, source, options = {}) {
  if (source instanceof Reader) return yield* source.readSteps(options.eofError ?? true, options.eofValue ?? NIL, true);
  return (yield* readFromStringSteps(runtime, source, options)).values[0];
}
export function* readAllSteps(runtime, source, options = {}) {
  const reader = new Reader(runtime, source, options), forms = [];
  for (;;) { const form = yield* reader.readSteps(false, EOF); if (form === EOF) return forms; forms.push(form); }
}
function runReader(runtime, name, steps, task = runtime.activeTask) { return runtime.call(new NativeFunction(name, () => steps), [], task); }
export function readFromString(runtime, source, options = {}) { return runReader(runtime, 'READ-FROM-STRING', readFromStringSteps(runtime, source, options), options.task).values; }
export function read(runtime, source, options = {}) { return runReader(runtime, 'READ', readSteps(runtime, source, options), options.task).values[0]; }
export function readAll(runtime, source, options = {}) { return runReader(runtime, 'READ-ALL', readAllSteps(runtime, source, options), options.task).values[0]; }
