import { NIL, T, UNBOUND, LispString, LispCharacter, LispArray } from './objects.mjs';
import { NativeFunction, LispCondition } from './execution.mjs';
import { Reader, readFromStringSteps } from './reader.mjs';
import { withStreamReader } from './file_streams.mjs';

const fail = (kind, message, datum) => { throw new LispCondition(kind, message, { datum }); };
const character = value => { if (!(value instanceof LispCharacter)) fail('TYPE-ERROR', 'Expected a character', value); return value.toString(); };
const text = value => { if (typeof value === 'string') return value; if (value instanceof LispString) return value.toString(); if (value instanceof LispArray && value.elementKind === 'character' && value.rank === 1) return Array.from({ length: value.length }, (_, i) => value.get(i).toString()).join(''); fail('TYPE-ERROR', 'Expected a string', value); };
const standardSyntax = c => c === undefined ? 'eof' : ' \t\n\r\f'.includes(c) ? 'whitespace' : c === '\\' ? 'single-escape' : c === '|' ? 'multiple-escape' : '()";\'`,#'.includes(c) ? 'macro' : 'constituent';
export class Readtable {
  constructor(source) { this.case = source?.case ?? 'UPCASE'; this.macros = new Map(); this.overrides = new Map(source?.overrides); if (source) for (const [c, definition] of source.macros) this.macros.set(c, { ...definition, ...(definition.dispatch ? { dispatch: new Map(definition.dispatch) } : {}) }); }
  syntax(c) { return this.macros.has(c) ? 'macro' : this.overrides.get(c) ?? (standardSyntax(c) === 'macro' ? 'constituent' : standardSyntax(c)); }
}

const simpleUppercase = c => { const upper = c.toUpperCase(); return [...upper].length === 1 && upper.toLowerCase() === c ? upper : c; };

export function installReadtable(rt) {
  rt.isReadtable = value => value instanceof Readtable;
  rt.isReaderStream = value => value instanceof Reader;
  const cl = name => rt.intern(name, 'COMMON-LISP');
  const onReader = (value, task, action) => withStreamReader(rt, value, task, action);
  const special = (name, initial) => { const symbol = cl(name); symbol.special = true; if (symbol.value === UNBOUND) symbol.value = initial; rt.exportSymbol(symbol, 'COMMON-LISP'); return symbol; };
  const standard = new Readtable();
  for (const c of '()";\'`,') { const fn = new NativeFunction(`reader ${c}`, (a, _r, task) => onReader(a[0], task, reader => reader.standardSteps(c, character(a[1])))); standard.macros.set(c, { standard: c, fn, nonterminating: false }); }
  const dispatch = new Map();
  for (const c of "'\\:BOXR(*C.+-=#|;ASPY") { const fn = new NativeFunction(`reader #${c}`, (a, _r, task) => onReader(a[0], task, reader => reader.standardDispatchSteps(c, a[2] === NIL ? null : a[2]))); fn.standardDispatch = c; dispatch.set(c, fn); }
  const dispatchFn = new NativeFunction('dispatch reader', (a, _r, task) => onReader(a[0], task, reader => reader.dispatchSteps(character(a[1]))));
  standard.macros.set('#', { dispatch, fn: dispatchFn, nonterminating: true });
  rt.standardReaderTable = standard; rt.readerTable = new Readtable(standard);
  const readtableSymbol = special('*READTABLE*', rt.readerTable);
  special('*READ-BASE*', 10); special('*READ-DEFAULT-FLOAT-FORMAT*', cl('SINGLE-FLOAT')); special('*READ-SUPPRESS*', NIL); special('*READ-EVAL*', rt.readEval ? T : NIL); special('*PACKAGE*', rt.currentPackage); special('*FEATURES*', rt.features);
  const inputStream = (value, task) => {
    let stream = value;
    if (value === undefined || value === NIL || value === T) {
      const symbol = rt.findSymbol(value === T ? '*TERMINAL-IO*' : '*STANDARD-INPUT*', 'COMMON-LISP');
      stream = symbol ? rt.variableValue(symbol, task ?? rt.activeTask) : UNBOUND;
      if (stream === UNBOUND || stream === undefined) stream = value === T ? rt.terminalIO : rt.input;
    }
    return stream;
  };
  const current = task => rt.readVariable(readtableSymbol, task ?? rt.activeTask);
  const table = (value, task, nilStandard = true) => { const candidate = value === undefined ? current(task) : value === NIL && nilStandard ? standard : value; if (!(candidate instanceof Readtable)) fail('TYPE-ERROR', 'Expected a readtable', candidate); return candidate; };
  const define = (name, min, max, fn) => rt.defineBuiltin(name, (args, runtime, task) => { if (args.length < min || args.length > max) fail('PROGRAM-ERROR', `Wrong number of arguments to ${name}`); return fn(args, runtime, task); });
  define('READTABLEP', 1, 1, a => a[0] instanceof Readtable ? T : NIL);
  define('COPY-READTABLE', 0, 2, (a, r, t) => { const source = table(a[0], t), copy = new Readtable(source); if (a[1] === undefined || a[1] === NIL) return copy; const target = table(a[1], t, false); target.case = copy.case; target.macros = copy.macros; target.overrides = copy.overrides; return target; });
  define('READTABLE-CASE', 1, 1, (a, r, t) => r.intern(table(a[0], t, false).case, 'KEYWORD'));
  define('(SETF READTABLE-CASE)', 2, 2, (a, r, t) => { const mode = a[0]?.name; if (!['UPCASE', 'DOWNCASE', 'PRESERVE', 'INVERT'].includes(mode)) fail('TYPE-ERROR', 'Invalid readtable case', a[0]); table(a[1], t, false).case = mode; return a[0]; });
  define('SET-MACRO-CHARACTER', 2, 4, (a, r, t) => { const target = table(a[3], t, false); r.resolveFunction(a[1]); target.macros.set(character(a[0]), { fn: a[1], nonterminating: a[2] !== undefined && a[2] !== NIL }); return T; });
  define('GET-MACRO-CHARACTER', 1, 2, (a, r, t) => { const definition = table(a[1], t).macros.get(character(a[0])); return r.multipleValues(definition?.fn ?? NIL, definition?.nonterminating ? T : NIL); });
  define('MAKE-DISPATCH-MACRO-CHARACTER', 1, 3, (a, r, t) => { table(a[2], t, false).macros.set(character(a[0]), { fn: dispatchFn, dispatch: new Map(), nonterminating: a[1] !== undefined && a[1] !== NIL }); return T; });
  const dispatchTable = (value, char, task) => { const definition = table(value, task).macros.get(character(char)); if (!definition?.dispatch) fail('TYPE-ERROR', 'Character is not a dispatching macro character', char); return definition.dispatch; };
  define('SET-DISPATCH-MACRO-CHARACTER', 3, 4, (a, r, t) => { const c = simpleUppercase(character(a[1])); if (/^[0-9]$/.test(c)) fail('PROGRAM-ERROR', 'Dispatch subcharacter cannot be a decimal digit'); r.resolveFunction(a[2]); dispatchTable(a[3], a[0], t).set(c, a[2]); return T; });
  define('GET-DISPATCH-MACRO-CHARACTER', 2, 3, (a, r, t) => dispatchTable(a[2], a[0], t).get(simpleUppercase(character(a[1]))) ?? NIL);
  define('SET-SYNTAX-FROM-CHAR', 2, 4, (a, r, t) => { const to = character(a[0]), from = character(a[1]), target = table(a[2], t, false), source = table(a[3] === undefined ? NIL : a[3], t), syntax = source.syntax(from), definition = source.macros.get(from); target.macros.delete(to); target.overrides.set(to, syntax); if (definition) target.macros.set(to, { ...definition, ...(definition.dispatch ? { dispatch: new Map(definition.dispatch) } : {}) }); return T; });
  define('READ-FROM-STRING', 1, Infinity, function* (a, r) {
    const options = { eofError: a[1] === undefined || a[1] !== NIL, eofValue: a[2] ?? NIL }, keys = new Map();
    if (a.length > 3 && (a.length - 3) % 2) fail('PROGRAM-ERROR', 'Odd number of keyword arguments');
    for (let i = 3; i < a.length; i += 2) { const key = a[i]?.name; if (!keys.has(key)) keys.set(key, a[i + 1]); }
    if (keys.get('ALLOW-OTHER-KEYS') === undefined || keys.get('ALLOW-OTHER-KEYS') === NIL) for (const key of keys.keys()) if (!['START', 'END', 'PRESERVE-WHITESPACE', 'ALLOW-OTHER-KEYS'].includes(key)) fail('PROGRAM-ERROR', 'Unknown READ-FROM-STRING keyword');
    if (keys.has('START')) options.start = keys.get('START');
    if (keys.has('END')) options.end = keys.get('END') === NIL ? undefined : keys.get('END');
    if (keys.has('PRESERVE-WHITESPACE')) options.preserveWhitespace = keys.get('PRESERVE-WHITESPACE') !== NIL;
    return yield* readFromStringSteps(r, text(a[0]), options);
  });
  for (const preserve of [false, true]) define(preserve ? 'READ-PRESERVING-WHITESPACE' : 'READ', 0, 4, (a, r, task) => {
    const input = inputStream(a[0], task), recursive = a[3] !== undefined && a[3] !== NIL;
    return onReader(input, task, function* (reader) {
      const value = yield* reader.readSteps(a[1] !== NIL, a[2] ?? NIL, recursive);
      if (!preserve && !recursive && reader.syntax(yield* reader.peekSteps()) === 'whitespace') yield* reader.takeSteps();
      return value;
    });
  });
  define('READ-DELIMITED-LIST', 1, 3, (a, r, task) =>
    onReader(inputStream(a[1], task), task, reader => reader.readDelimitedSteps(character(a[0]), a[2] !== undefined && a[2] !== NIL)));
  define('PEEK-CHAR', 0, 5, (a, r, task) => {
    const input = inputStream(a[1], task);
    return onReader(input, task, function* (reader) {
      if (a[0] === T) yield* reader.whitespaceSteps();
      else if (a[0] !== undefined && a[0] !== NIL) {
        const wanted = character(a[0]);
        let char;
        while ((char = yield* reader.peekSteps()) !== undefined && char !== wanted) yield* reader.takeSteps();
      }
      const char = yield* reader.peekSteps();
      if (char !== undefined) return new LispCharacter(char);
      if (a[2] === NIL && (a[4] === undefined || a[4] === NIL)) return a[3] ?? NIL;
      throw new LispCondition('END-OF-FILE', 'End of input stream', { stream: input });
    });
  });
  return rt;
}
