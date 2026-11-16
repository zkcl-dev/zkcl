import { NIL, T, UNBOUND, LispSymbol, Cons, Cell, LispCode, Closure, LispString, LispPathname, fromArray, eql } from './objects.mjs';
import { read, readAll, readFromString, readSteps, readAllSteps, readFromStringSteps, Reader, EOF } from './reader.mjs';
import { installReadtable } from './readtable.mjs';
import { installMacros } from './macros.mjs';
import { compileJavaScript } from './codegen.mjs';
import { CallRequest, LispCondition, NativeFunction } from './execution.mjs';
import { keys, StringStream } from './builtins.mjs';
import { compiledModuleHeader, emitResidentModule } from './loader.mjs';
import { LispFileStream, resolveSynonymStream, streamDirection, withStreamReader } from './file_streams.mjs';

const ALL = null;
export const FRONTEND_SYMBOLS = Object.freeze(('QUOTE IF PROGN LOCALLY SETQ FUNCTION LAMBDA LET LET* DEFUN DEFMACRO DEFINE-COMPILER-MACRO FLET LABELS MACROLET SYMBOL-MACROLET BLOCK RETURN-FROM TAGBODY GO CATCH THROW UNWIND-PROTECT PROGV MULTIPLE-VALUE-CALL MULTIPLE-VALUE-PROG1 THE EVAL-WHEN LOAD-TIME-VALUE DESTRUCTURING-BIND DECLARE SPECIAL QUASIQUOTE UNQUOTE UNQUOTE-SPLICING UNQUOTE-NSPLICING WHEN UNLESS AND OR COND RETURN PROG1 PROG2 PROG PROG* DEFVAR DEFPARAMETER DEFCONSTANT DECLAIM SETF INCF DECF PUSH PUSHNEW POP PSETQ PSETF MULTIPLE-VALUE-BIND MULTIPLE-VALUE-LIST MULTIPLE-VALUE-SETQ NTH-VALUE DOLIST DOTIMES DO DO* CASE ECASE TYPECASE ETYPECASE OTHERWISE IGNORE-ERRORS HANDLER-BIND HANDLER-CASE RESTART-BIND RESTART-CASE WITH-CONDITION-RESTARTS IN-PACKAGE DEFPACKAGE DEFTYPE DEFINE-SETF-EXPANDER DEFSETF &OPTIONAL &REST &KEY &ALLOW-OTHER-KEYS &AUX &BODY &WHOLE &ENVIRONMENT DEFCLASS DEFINE-CONDITION DEFGENERIC DEFMETHOD DEFINE-METHOD-COMBINATION DEFSTRUCT WITH-SLOTS WITH-ACCESSORS').split(' '));
const range = (start, end) => ({ start, end });
const list = (...xs) => fromArray(xs);
function array(value, description = 'proper list') {
  const result = [], seen = new Set();
  while (value !== NIL) {
    if (!(value instanceof Cons) || seen.has(value)) throw new CompileError(`expected a ${description}`);
    seen.add(value); result.push(value.car); value = value.cdr;
  }
  return result;
}
function symbol(value, description = 'symbol') {
  if (!(value instanceof LispSymbol) && value !== NIL && value !== T) throw new CompileError(`expected a ${description}`);
  return value;
}
function bindable(value) {
  symbol(value, 'variable');
  if (value === NIL || value === T || value.constant || value.package?.name === 'KEYWORD' || value.package === 'KEYWORD' || value.name?.startsWith('&')) throw new CompileError('invalid variable name');
  return value;
}
function functionName(value, runtime) {
  if (value instanceof LispSymbol) return value;
  const parts = array(value, 'function name');
  if (parts.length !== 2 || parts[0] !== runtime.intern('SETF', 'COMMON-LISP')) throw new CompileError('invalid function name');
  bindable(parts[1]); return value;
}
function blockName(value) { return value instanceof Cons ? value.cdr.car : value; }
export class CompileError extends LispCondition {
  constructor(message) { super('PROGRAM-ERROR', message, { conditionClass: 'PROGRAM-ERROR' }); this.name = 'CompileError'; }
}
function arity(name, args, min, max = min) {
  if (args.length < min || args.length > max) throw new CompileError(`${name} expects ${min === max ? min : `${min} to ${max}`} arguments`);
}

class Scope {
  constructor(parent = null) { this.parent = parent; this.variables = new Map(); this.functions = new Map(); this.setfFunctions = new Map(); this.blocks = new Map(); this.tags = new Map(); this.macros = new Map(); this.symbolMacros = new Map(); this.specials = new Set(); }
  find(namespace, name) { for (let s = this; s; s = s.parent) { if (namespace === 'variables' && s.specials.has(name)) return { kind: 'Special', symbol: name }; if (namespace === 'symbolMacros' && (s.specials.has(name) || s.variables.has(name))) return undefined; if (s[namespace].has(name)) return s[namespace].get(name); } return undefined; }
  isSpecial(name) { for (let s = this; s; s = s.parent) if (s.specials.has(name)) return true; return false; }
  findMacro(name) {
    for (let scope = this; scope; scope = scope.parent) {
      if (scope.functions.has(name)) return NIL;
      if (scope.macros.has(name)) return scope.macros.get(name);
    }
    return undefined;
  }
}

function parameters(runtime, form, macro = false, environmentAllowed = macro) {
  const p = { required: [], optional: [], rest: null, keys: null, allow: false, aux: [], whole: null, environment: null };
  let items = [], tail = form, seenCons = new Set();
  while (tail instanceof Cons) { if (seenCons.has(tail)) throw new CompileError('circular lambda list'); seenCons.add(tail); items.push(tail.car); tail = tail.cdr; }
  if (tail !== NIL && !macro) throw new CompileError('ordinary lambda list must be proper');
  const names = new Set();
  function variable(x, pattern = macro) {
    if (pattern && (x === NIL || x instanceof Cons)) { const nested = parameters(runtime, x, true, false); for (const n of nested.names) { if (names.has(n)) throw new CompileError('duplicate lambda variable'); names.add(n); } return { pattern: nested }; }
    bindable(x); if (names.has(x)) throw new CompileError('duplicate lambda variable'); names.add(x); return x;
  }
  function spec(x, key = false, aux = false) {
    const parts = x instanceof Cons ? array(x) : [x];
    if (parts.length < 1 || parts.length > (aux ? 2 : 3)) throw new CompileError('invalid lambda parameter');
    let v = parts[0], keyword;
    if (key && v instanceof Cons) { const pair = array(v); arity('keyword parameter', pair, 2); keyword = symbol(pair[0]); v = pair[1]; }
    else if (key) keyword = runtime.intern(symbol(v).name, 'KEYWORD');
    const result = { variable: variable(v), init: parts[1] ?? NIL, supplied: parts.length > 2 ? variable(parts[2], false) : null };
    if (key) result.keyword = keyword;
    return result;
  }
  let section = 'required', restSeen = false, auxSeen = false;
  const order = { required: 0, optional: 1, afterRest: 2, key: 3, afterAllow: 4, aux: 5 };
  for (let i = 0; i < items.length; i++) {
    const x = items[i], marker = x instanceof LispSymbol && x.name.startsWith('&') ? x.name : null;
    if (marker) {
      if (marker === '&WHOLE' || marker === '&ENVIRONMENT') {
        if (!macro || marker === '&ENVIRONMENT' && !environmentAllowed || i + 1 >= items.length || (marker === '&WHOLE' && i !== 0)) throw new CompileError(`invalid ${marker}`);
        const key = marker === '&WHOLE' ? 'whole' : 'environment';
        if (p[key]) throw new CompileError(`duplicate ${marker}`); p[key] = variable(items[++i], marker === '&WHOLE'); continue;
      }
      const next = marker === '&OPTIONAL' ? 'optional' : marker === '&KEY' ? 'key' : marker === '&AUX' ? 'aux' : marker === '&ALLOW-OTHER-KEYS' ? 'afterAllow' : null;
      if (marker === '&REST' || (macro && marker === '&BODY')) {
        if (restSeen || order[section] > 1 || i + 1 >= items.length) throw new CompileError(`invalid ${marker}`);
        p.rest = variable(items[++i]); restSeen = true; section = 'afterRest'; continue;
      }
      if (!next || order[next] <= order[section] || (next === 'afterAllow' && section !== 'key')) throw new CompileError(`unexpected lambda list keyword ${marker}`);
      section = next; if (next === 'key') p.keys = []; if (next === 'afterAllow') p.allow = true; if (next === 'aux') auxSeen = true; continue;
    }
    if (section === 'required') p.required.push(variable(x));
    else if (section === 'optional') p.optional.push(spec(x));
    else if (section === 'key') p.keys.push(spec(x, true));
    else if (section === 'aux') p.aux.push(spec(x, false, true));
    else throw new CompileError('unexpected lambda parameter after &REST or &ALLOW-OTHER-KEYS');
  }
  if (tail !== NIL) { if (restSeen || p.keys || auxSeen) throw new CompileError('invalid dotted macro lambda list'); p.rest = variable(tail); }
  p.names = names; return p;
}

export class Compiler {
  constructor(runtime, parent = null, scope = null, name = null, context = parent?.context ?? null) {
    this.runtime = runtime; this.parent = parent; this.scope = scope ?? new Scope(); this.next = 0; this.captureLocations = []; this.captureMap = new Map(); this.blockName = name ? blockName(name) : null; this.context = context;
    this.code = { name: name ? (name instanceof Cons ? `(SETF ${name.cdr.car.name})` : name.name ?? name) : null, source_file: null, debug: { source_expression: null, entry: null, retain_frame: false, variables: [] }, arity: { required: 0, maximum: 0 }, registers: 0, instructions: [], constants: [], lambda_list: null, captures: [], notinline: [], handlers: [], restarts: [], keywords: [], jump_tables: [] };
  }
  cl(name) { return this.runtime.intern(name, 'COMMON-LISP'); }
  form(name, ...args) { return list(this.cl(name), ...args); }
  gensym(name = 'G') { return new LispSymbol(name, null); }
  emit(name, operands) { const pc = this.code.instructions.length; this.code.instructions.push(operands === undefined ? name : { [name]: operands }); return pc; }
  patch(pc, name, operands) { this.code.instructions[pc] = { [name]: operands }; }
  reg(count = 1) { const r = this.next; this.next += count; this.code.registers = Math.max(this.code.registers, this.next); return r; }
  constant(value) { let index = this.code.constants.indexOf(value); if (index < 0) { index = this.code.constants.length; this.code.constants.push(value); } return index; }
  scalar(dest, emit) { const out = dest === ALL ? this.reg() : dest; emit(out); if (dest === ALL) this.emit('SetValues', range(out, out + 1)); }
  literal(value, dest) { this.scalar(dest, out => this.emit('Constant', [out, this.constant(value)])); }
  receive(dest) { if (dest !== ALL) this.emit('ReadValue', [dest, 0]); }
  *body(forms, dest = ALL) { if (!forms.length) this.literal(NIL, dest); else { for (const f of forms.slice(0, -1)) yield* this.expr(f, this.reg()); yield* this.expr(forms.at(-1), dest); } }
  *withScope(fn) { const previous = this.scope; this.scope = new Scope(previous); try { return yield* fn.call(this); } finally { this.scope = previous; } }
  bind(name, reg, namespace = 'variables', kind = 'MutableCell') {
    if (namespace === 'functions') { functionName(name, this.runtime); if (name instanceof Cons) { namespace = 'setfFunctions'; name = name.cdr.car; } } else bindable(name);
    const binding = { owner: this, reg, kind, symbol: name };
    if (kind === 'MutableCell') this.emit('BoxLocal', { reg, at: 0 });
    this.scope[namespace].set(name, binding); return binding;
  }
  location(binding) {
    if (binding.owner === this) return { Local: binding.reg };
    let index = this.captureMap.get(binding);
    if (index === undefined) {
      if (!this.parent) throw new CompileError('invalid lexical capture');
      const parentLocation = this.parent.location(binding);
      index = this.captureLocations.length; this.captureMap.set(binding, index); this.captureLocations.push({ binding, location: parentLocation }); this.code.captures.push(binding.kind);
    }
    return { Capture: index };
  }
  readBinding(binding, out) {
    const location = this.location(binding);
    if (binding.kind === 'MutableCell') this.emit('ReadBox', { out, cell: location });
    else if (location.Local !== undefined) this.emit('Copy', [out, location.Local]);
    else this.emit('Capture', [out, location.Capture]);
  }
  variable(name, dest, functionCell = false) {
    const namespace = functionCell ? name instanceof Cons ? 'setfFunctions' : 'functions' : 'variables';
    const binding = this.scope.find(namespace, functionCell && name instanceof Cons ? name.cdr.car : name);
    this.scalar(dest, out => binding && binding.kind !== 'Special' ? this.readBinding(binding, out) : this.emit('ReadCell', { out, symbol: this.constant(name), function: functionCell, at: 0 }));
  }
  *set(name, value, dest) {
    bindable(name);
    const [place, expanded] = yield* macroexpandSteps(this.runtime, name, this.scope, true);
    if (expanded) return yield* this.expr(this.form('SETF', place, value), dest);
    const reg = this.reg(); yield* this.expr(value, reg);
    const binding = this.scope.find('variables', name);
    if (binding && binding.kind !== 'Special') this.emit('WriteBox', { cell: this.location(binding), src: reg });
    else this.emit('WriteCell', { symbol: this.constant(name), src: reg });
    this.scalar(dest, out => this.emit('Copy', [out, reg]));
  }
  *closure(lambdaList, body, dest, name = null, macro = false) {
    const child = new Compiler(this.runtime, this, new Scope(this.scope), name);
    yield* child.lambda(lambdaList, body, macro);
    const code = this.runtime.makeCode(child.code, { constants: child.code.constants });
    const cells = this.reg(child.captureLocations.length);
    child.captureLocations.forEach(({ binding, location }, i) => {
      if (location.Local !== undefined) this.emit('Copy', [cells + i, location.Local]);
      else this.emit('Capture', [cells + i, location.Capture]);
    });
    this.scalar(dest, out => this.emit('Closure', { out, code: this.constant(code), cells: range(cells, cells + child.captureLocations.length), at: 0 }));
    return child.documentation;
  }
  declarations(forms, doc = false, apply = true) {
    let rest = forms;
    const specials = new Set(), declarations = []; let documentation = null;
    while (rest.length) {
      if (doc && documentation === null && rest.length > 1 && rest[0] instanceof LispString) { documentation = rest[0]; rest = rest.slice(1); continue; }
      if (!(rest[0] instanceof Cons) || rest[0].car !== this.cl('DECLARE')) break;
      declarations.push(rest[0]);
      for (const declaration of array(rest[0].cdr)) {
        const [kind, ...names] = array(declaration);
        if (kind === this.cl('SPECIAL')) for (const name of names) { bindable(name); if (this.scope.symbolMacros.has(name)) throw new CompileError('a symbol macro binding cannot be declared SPECIAL'); specials.add(name); }
      }
      rest = rest.slice(1);
    }
    if (apply) for (const name of specials) this.scope.specials.add(name);
    return { forms: rest, specials, documentation, declarations };
  }
  *bindParameter(pattern, reg, specials) {
    if (pattern?.pattern) return yield* this.destructure(pattern.pattern, reg, NIL, NIL, specials);
    if (specials.has(pattern) || pattern.special || this.runtime.specials?.has(pattern)) {
      this.emit('BindSpecial', { symbol: this.constant(pattern), value: reg }); this.scope.variables.delete(pattern); this.scope.specials.add(pattern); return 1;
    }
    this.bind(pattern, reg); return 0;
  }
  *defaultParameter(spec, reg, specials) {
    const supplied = spec.supplied ? this.reg() : null;
    if (supplied !== null) this.literal(T, supplied);
    const branch = this.emit('IfPresent', [reg, 0]);
    if (supplied !== null) this.literal(NIL, supplied);
    yield* this.expr(spec.init, reg); this.patch(branch, 'IfPresent', [reg, this.code.instructions.length]);
    let dynamic = yield* this.bindParameter(spec.variable, reg, specials);
    if (supplied !== null) dynamic += yield* this.bindParameter(spec.supplied, supplied, specials);
    return dynamic;
  }
  keywordTable(p) {
    const table = this.code.keywords.length;
    this.code.keywords.push({ names: p.keys.map(k => this.constant(k.keyword)), allow_other_keys: p.allow, override_key: this.constant(this.runtime.intern('ALLOW-OTHER-KEYS', 'KEYWORD')) }); return table;
  }
  *lambda(lambdaList, body, macro = false) {
    this.code.lambda_list = this.constant(lambdaList);
    const p = parameters(this.runtime, lambdaList, macro), decl = this.declarations(body, true, false);
    this.documentation = decl.documentation;
    let dynamic = 0;
    if (macro) {
      this.code.arity = { required: 2, maximum: 2 }; this.reg(2);
      const cursor = this.reg();
      const accessor = macro === 'compiler' ? '%%COMPILER-MACRO-ARGUMENTS' : 'CDR';
      this.emit('Call', { callee: { Named: this.constant(this.cl(accessor)) }, out: cursor, args: range(0, 1), at: 0, associate: false, notinline: false });
      dynamic += yield* this.destructure(p, cursor, 0, 1, decl.specials);
    } else {
      this.code.arity = { required: p.required.length, maximum: p.rest || p.keys ? null : p.required.length + p.optional.length };
      const required = this.reg(p.required.length);
      if (p.keys) this.emit('CheckKeywords', { start: p.optional.length, table: this.keywordTable(p) });
      for (const [i, v] of p.required.entries()) dynamic += yield* this.bindParameter(v, required + i, decl.specials);
      for (const [i, spec] of p.optional.entries()) { const reg = this.reg(); this.emit('OptionalArgument', [i, reg]); dynamic += yield* this.defaultParameter(spec, reg, decl.specials); }
      if (p.rest) { const out = this.reg(); this.emit('RestArguments', { start: p.optional.length, out, at: 0 }); dynamic += yield* this.bindParameter(p.rest, out, decl.specials); }
      if (p.keys) for (const [i, spec] of p.keys.entries()) { const reg = this.reg(); this.emit('KeywordArgument', [i, reg]); dynamic += yield* this.defaultParameter(spec, reg, decl.specials); }
      if (p.optional.length || p.rest || p.keys) this.emit('ReleaseArguments');
      for (const spec of p.aux) { const reg = this.reg(); yield* this.expr(spec.init, reg); dynamic += yield* this.bindParameter(spec.variable, reg, decl.specials); }
    }
    for (const name of decl.specials) this.scope.specials.add(name);
    if (this.blockName) yield* this.block(typeof this.blockName === 'string' ? this.runtime.intern(this.blockName, this.runtime.currentPackage) : this.blockName, decl.forms, ALL);
    else yield* this.body(decl.forms);
    if (dynamic) this.emit('Unbind', dynamic);
    return this.code;
  }
  *destructure(p, cursor, whole, environment, specials = new Set()) {
    let dynamic = 0;
    if (p.whole) { const reg = this.reg(); this.emit('Copy', [reg, typeof whole === 'number' ? whole : cursor]); dynamic += yield* this.bindParameter(p.whole, reg, specials); }
    if (p.environment) { const reg = this.reg(); if (typeof environment === 'number') this.emit('Copy', [reg, environment]); else this.literal(environment, reg); dynamic += yield* this.bindParameter(p.environment, reg, specials); }
    for (const v of p.required) { const out = this.reg(); this.emit('DestructureNext', { cursor, out, required: true, macro_call: true, at: 0 }); dynamic += yield* this.bindParameter(v, out, specials); }
    for (const spec of p.optional) { const out = this.reg(); this.emit('DestructureNext', { cursor, out, required: false, macro_call: true, at: 0 }); dynamic += yield* this.defaultParameter(spec, out, specials); }
    if (p.rest) { const out = this.reg(); this.emit('Copy', [out, cursor]); dynamic += yield* this.bindParameter(p.rest, out, specials); }
    if (p.keys) { const start = this.reg(p.keys.length), table = this.keywordTable(p); this.emit('DestructureKeywords', { cursor, table, out: range(start, start + p.keys.length), macro_call: true, at: 0 }); for (const [i, spec] of p.keys.entries()) dynamic += yield* this.defaultParameter(spec, start + i, specials); }
    else if (!p.rest) this.emit('DestructureEnd', { cursor, macro_call: true, at: 0 });
    for (const spec of p.aux) { const out = this.reg(); yield* this.expr(spec.init, out); dynamic += yield* this.bindParameter(spec.variable, out, specials); }
    return dynamic;
  }
  *call(operator, args, dest, associate = false) {
    let callee;
    if (operator instanceof LispSymbol && !this.scope.find('functions', operator)) callee = { Named: this.constant(operator) };
    else { const reg = this.reg(); operator instanceof LispSymbol ? this.variable(operator, reg, true) : yield* this.expr(operator, reg); callee = { Value: reg }; }
    const start = this.reg(args.length);
    for (const [i, arg] of args.entries()) yield* this.expr(arg, start + i);
    this.emit('Call', { callee, out: dest, args: range(start, start + args.length), at: 0, associate: associate && 'Named' in callee, notinline: false });
  }
  *block(name, body, dest) {
    symbol(name);
    yield* this.withScope(function* () {
      const token = this.reg(); this.emit('ExitToken', [token, 0]); this.scope.blocks.set(name, { owner: this, reg: token, kind: 'ExitToken' });
      const entry = this.emit('EnterBlock', { token, resume: 0, clear: this.next });
      yield* this.body(body, ALL); this.emit('LeaveTarget'); this.patch(entry, 'EnterBlock', { token, resume: this.code.instructions.length, clear: token + 1 }); this.receive(dest);
    });
  }
  *let(args, dest, sequential) {
    if (!args.length) throw new CompileError('LET requires bindings');
    const bindings = array(args[0]), parsed = bindings.map(b => b instanceof Cons ? array(b) : [b]);
    for (const b of parsed) { arity('binding', b, 1, 2); bindable(b[0]); }
    if (!sequential && new Set(parsed.map(b => b[0])).size !== parsed.length) throw new CompileError('duplicate LET variable');
    const initial = [];
    if (!sequential) for (const b of parsed) { const reg = this.reg(); yield* this.expr(b[1] ?? NIL, reg); initial.push(reg); }
    yield* this.withScope(function* () {
      const decl = this.declarations(args.slice(1), false, false); let dynamic = 0;
      for (const [i, b] of parsed.entries()) { const reg = sequential ? this.reg() : initial[i]; if (sequential) yield* this.expr(b[1] ?? NIL, reg); dynamic += yield* this.bindParameter(b[0], reg, decl.specials); }
      for (const name of decl.specials) this.scope.specials.add(name);
      yield* this.body(decl.forms, dest); if (dynamic) this.emit('Unbind', dynamic);
    });
  }
  *tagbody(forms, dest) {
    yield* this.withScope(function* () {
      const token = this.reg(); this.emit('ExitToken', [token, 0]);
      const tags = forms.filter(x => x instanceof LispSymbol || x === NIL || typeof x === 'number' || typeof x === 'bigint');
      if (new Set(tags).size !== tags.length) throw new CompileError('duplicate TAGBODY tag');
      tags.forEach((tag, label) => this.scope.tags.set(tag, { owner: this, reg: token, kind: 'ExitToken', label }));
      const table = this.code.jump_tables.length; this.code.jump_tables.push({ targets: [] });
      this.emit('EnterTagbody', { token, jumps: table, clear: this.next });
      for (const form of forms) { if (tags.includes(form)) this.code.jump_tables[table].targets.push(this.code.instructions.length); else yield* this.expr(form, this.reg()); }
      this.emit('LeaveTarget'); this.literal(NIL, dest);
    });
  }
  handlerTest(type) { return type === T ? 'Any' : type === NIL ? 'Never' : { Named: this.constant(type) }; }
  *handlerBind(args, dest) {
    if (!args.length) throw new CompileError('HANDLER-BIND requires bindings');
    const handlers = [];
    for (const spec of array(args[0])) { const pair = array(spec); arity('handler binding', pair, 2); const reg = this.reg(); yield* this.expr(pair[1], reg); handlers.push({ test: this.handlerTest(pair[0]), target: { Function: reg } }); }
    const table = this.code.handlers.length; this.code.handlers.push({ handlers }); this.emit('EnterHandlers', { table }); yield* this.body(args.slice(1), dest); this.emit('LeaveHandlers');
  }
  *handlerCase(args, dest) {
    if (!args.length) throw new CompileError('HANDLER-CASE requires expression');
    let noError = null;
    const clauses = [];
    for (const clause of args.slice(1)) {
      const parts = array(clause); if (parts.length < 2) throw new CompileError('invalid HANDLER-CASE clause');
      if (parts[0] === this.runtime.intern('NO-ERROR', 'KEYWORD')) { if (noError) throw new CompileError('duplicate :NO-ERROR clause'); noError = parts; }
      else { const vars = array(parts[1]); arity('handler lambda list', vars, 0, 1); vars.forEach(bindable); clauses.push({ parts, reg: this.reg(), vars }); }
    }
    const clear = this.next, table = this.code.handlers.length;
    this.code.handlers.push({ handlers: clauses.map(c => ({ test: this.handlerTest(c.parts[0]), target: { Case: { pc: 0, condition: c.reg, clear } } })) });
    this.emit('EnterHandlers', { table }); yield* this.expr(args[0], ALL); this.emit('LeaveHandlers');
    if (noError) yield* this.callWithValues(noError[1], noError.slice(2));
    const joins = [this.emit('Jump', 0)];
    for (const [i, clause] of clauses.entries()) {
      this.code.handlers[table].handlers[i].target.Case.pc = this.code.instructions.length;
      yield* this.withScope(function* () { const decl = this.declarations(clause.parts.slice(2)); let dynamic = 0; if (clause.vars.length) dynamic = yield* this.bindParameter(clause.vars[0], clause.reg, decl.specials); yield* this.body(decl.forms, ALL); if (dynamic) this.emit('Unbind', dynamic); });
      joins.push(this.emit('Jump', 0));
    }
    for (const pc of joins) this.patch(pc, 'Jump', this.code.instructions.length); this.receive(dest);
  }
  *callWithValues(params, body) { const callee = this.reg(); yield* this.closure(params, body, callee); this.emit('CallValues', { callee, at: 0 }); }
  *restartBind(args, dest, caseMode = false) {
    if (!args.length) throw new CompileError(`${caseMode ? 'RESTART-CASE' : 'RESTART-BIND'} requires arguments`);
    const clauses = (caseMode ? args.slice(1) : array(args[0])).map(x => array(x)), arms = [];
    const specs = [];
    for (const parts of clauses) {
      if (parts.length < 2) throw new CompileError('invalid restart clause');
      const spec = { name: this.constant(symbol(parts[0], 'restart name')), object: this.reg(), test: null, interactive: null, report: null, target: null };
      let body;
      if (caseMode) body = parts.slice(2);
      else { const reg = this.reg(); yield* this.expr(parts[1], reg); spec.target = { Function: reg }; body = parts.slice(2); }
      while (body.length && body[0] instanceof LispSymbol && (body[0].package === 'KEYWORD' || body[0].package?.name === 'KEYWORD')) {
        const key = body[0].name, field = ({ TEST: 'test', INTERACTIVE: 'interactive', REPORT: 'report', 'TEST-FUNCTION': 'test', 'INTERACTIVE-FUNCTION': 'interactive', 'REPORT-FUNCTION': 'report' })[key];
        if (!field || (caseMode ? key.endsWith('-FUNCTION') : !key.endsWith('-FUNCTION')) || body.length < 2 || spec[field] !== null) throw new CompileError('unsupported or duplicate restart option');
        const value = body[1], reg = this.reg();
        if (caseMode && value instanceof LispSymbol) this.variable(value, reg, true); else yield* this.expr(value, reg);
        spec[field] = reg; body = body.slice(2);
      }
      if (!caseMode && body.length) throw new CompileError('restart options require keyword/value pairs');
      if (caseMode) { spec.target = { Case: { pc: 0, clear: 0 } }; arms.push({ params: parts[1], body }); }
      specs.push(spec);
    }
    const clear = this.next, table = this.code.restarts.length; this.code.restarts.push({ restarts: specs }); this.emit('EnterRestarts', { table, at: 0 });
    if (!caseMode) { yield* this.body(args.slice(1), dest); this.emit('LeaveRestarts'); return; }
    const [protectedForm] = yield* macroexpandSteps(this.runtime, args[0], this.scope);
    if (protectedForm instanceof Cons && ['SIGNAL', 'ERROR', 'CERROR', 'WARN'].some(name => protectedForm.car === this.cl(name)))
      yield* this.call(protectedForm.car, array(protectedForm.cdr), ALL, true);
    else yield* this.expr(protectedForm, ALL);
    this.emit('LeaveRestarts'); const joins = [this.emit('Jump', 0)];
    for (const [i, arm] of arms.entries()) { specs[i].target.Case = { pc: this.code.instructions.length, clear }; yield* this.callWithValues(arm.params, arm.body); joins.push(this.emit('Jump', 0)); }
    for (const pc of joins) this.patch(pc, 'Jump', this.code.instructions.length); this.receive(dest);
  }
  *bindLocalMacros(forms) {
    const definitions = [];
    for (const form of forms) {
      const [name, params, ...body] = array(form);
      symbol(name);
      if (params === undefined) throw new CompileError('invalid local macro');
      const child = new Compiler(this.runtime, null, new Scope(this.scope.parent), name);
      yield* child.lambda(params, body, true);
      definitions.push([name, this.runtime.makeFunction(child.code, { constants: child.code.constants })]);
    }
    for (const [name, expander] of definitions) this.scope.macros.set(name, expander);
  }
  *expr(form, dest = ALL) {
    [form] = yield* macroexpandSteps(this.runtime, form, this.scope);
    if (form instanceof LispSymbol && form !== NIL && form !== T) {
      if (form.package === 'KEYWORD' || form.package?.name === 'KEYWORD') return this.literal(form, dest);
      return this.variable(form, dest);
    }
    if (!(form instanceof Cons)) return this.literal(form, dest);
    const [operator, ...args] = array(form, 'function form');
    const name = operator instanceof LispSymbol && operator === this.cl(operator.name) ? operator.name : null;
    switch (name) {
      case 'QUOTE': arity(name, args, 1); return this.literal(args[0], dest);
      case 'IF': {
        arity(name, args, 2, 3); const test = this.reg(); yield* this.expr(args[0], test); const branch = this.emit('Unless', [test, 0]);
        yield* this.expr(args[1], dest); const skip = this.emit('Jump', 0); this.patch(branch, 'Unless', [test, this.code.instructions.length]); yield* this.expr(args[2] ?? NIL, dest); this.patch(skip, 'Jump', this.code.instructions.length); return;
      }
      case 'PROGN': return yield* this.body(args, dest);
      case 'LOCALLY': return yield* this.withScope(function* () { yield* this.body(this.declarations(args).forms, dest); });
      case 'SETQ': {
        if (args.length % 2) throw new CompileError('SETQ requires variable/value pairs');
        if (!args.length) return this.literal(NIL, dest);
        for (let i = 0; i < args.length; i += 2) yield* this.set(args[i], args[i + 1], i === args.length - 2 ? dest : this.reg()); return;
      }
      case 'FUNCTION': {
        arity(name, args, 1); const value = args[0];
        if (value instanceof LispSymbol) return this.variable(value, dest, true);
        const parts = array(value); if (parts[0] === this.cl('SETF')) { functionName(value, this.runtime); return this.variable(value, dest, true); } if (parts[0] !== this.cl('LAMBDA') || parts.length < 2) throw new CompileError('invalid FUNCTION argument');
        return yield* this.closure(parts[1], parts.slice(2), dest);
      }
      case 'LAMBDA': if (!args.length) throw new CompileError('LAMBDA requires a lambda list'); return yield* this.closure(args[0], args.slice(1), dest);
      case 'LET': case 'LET*': return yield* this.let(args, dest, name === 'LET*');
      case 'DEFUN': case 'DEFMACRO': case '%%DEFUN': case '%%DEFMACRO': {
        if (args.length < 2) throw new CompileError(`${name} requires a name, lambda list and body`);
        const macro = name.endsWith('DEFMACRO'), fnName = macro ? symbol(args[0]) : functionName(args[0], this.runtime), out = this.reg(); yield* this.closure(args[1], args.slice(2), out, fnName, macro);
        this.emit('DefineFunction', { symbol: this.constant(fnName), definition: out, is_macro: macro, at: 0 }); return this.literal(fnName, dest);
      }
      case '%%DEFINE-COMPILER-MACRO': {
        if (args.length < 2) throw new CompileError('DEFINE-COMPILER-MACRO requires a name and lambda list');
        const [designator, lambdaList, ...body] = args;
        const fnName = functionName(designator, this.runtime);
        const start = this.reg(3);
        this.literal(fnName, start);
        const documentation = yield* this.closure(lambdaList, body, start + 1, fnName, 'compiler');
        this.literal(documentation ?? NIL, start + 2);
        this.emit('Call', { callee: { Named: this.constant(this.cl('%%INSTALL-COMPILER-MACRO')) }, out: dest, args: range(start, start + 3), at: 0, associate: false, notinline: false });
        return;
      }
      case 'DEFINE-SETF-EXPANDER': case '%%DEFINE-SETF-EXPANDER-FORM': {
        if (args.length < 2) throw new CompileError('DEFINE-SETF-EXPANDER requires name and lambda list');
        const name = symbol(args[0]), definition = this.reg(); yield* this.closure(args[1], args.slice(2), definition, name, true);
        const start = this.reg(2); this.literal(name, start); this.emit('Copy', [start + 1, definition]); this.emit('Call', { callee: { Named: this.constant(this.cl('%%DEFINE-SETF-EXPANDER')) }, out: this.reg(), args: range(start, start + 2), at: 0, associate: false, notinline: false }); return this.literal(name, dest);
      }
      case 'DEFINE-SYMBOL-MACRO': case '%%DEFINE-SYMBOL-MACRO-FORM': {
        arity(name, args, 2); const variable = bindable(args[0]);
        return yield* this.expr(this.form('%%DEFINE-SYMBOL-MACRO', this.form('QUOTE', variable), this.form('QUOTE', args[1])), dest);
      }
      case 'FLET': case 'LABELS': {
        if (!args.length) throw new CompileError(`${name} requires definitions`);
        const defs = array(args[0]).map(x => array(x)); defs.forEach(x => { if (x.length < 2) throw new CompileError('invalid local function'); functionName(x[0], this.runtime); });
        return yield* this.withScope(function* () {
          const bindings = defs.map(d => ({ name: d[0], reg: this.reg() }));
          if (name === 'LABELS') bindings.forEach(b => { this.literal(NIL, b.reg); this.bind(b.name, b.reg, 'functions'); });
          for (const [i, d] of defs.entries()) { const value = this.reg(); yield* this.closure(d[1], d.slice(2), value, d[0]); if (name === 'LABELS') this.emit('WriteBox', { cell: { Local: bindings[i].reg }, src: value }); else this.emit('Copy', [bindings[i].reg, value]); }
          if (name === 'FLET') bindings.forEach(b => this.bind(b.name, b.reg, 'functions'));
          yield* this.body(this.declarations(args.slice(1)).forms, dest);
        });
      }
      case 'MACROLET': {
        if (!args.length) throw new CompileError('MACROLET requires definitions');
        const defs = array(args[0]);
        return yield* this.withScope(function* () {
          yield* this.bindLocalMacros(defs);
          yield* this.body(this.declarations(args.slice(1)).forms, dest);
        });
      }
      case 'SYMBOL-MACROLET': {
        if (!args.length) throw new CompileError('SYMBOL-MACROLET requires definitions');
        return yield* this.withScope(function* () { for (const def of array(args[0])) { const pair = array(def); arity('symbol macro binding', pair, 2); this.scope.symbolMacros.set(bindable(pair[0]), pair[1]); } yield* this.body(this.declarations(args.slice(1)).forms, dest); });
      }
      case 'BLOCK': if (!args.length) throw new CompileError('BLOCK requires name'); return yield* this.block(args[0], args.slice(1), dest);
      case 'RETURN-FROM': {
        arity(name, args, 1, 2); const binding = this.scope.find('blocks', symbol(args[0])); if (!binding) throw new CompileError('RETURN-FROM has no lexically enclosing BLOCK');
        const token = this.reg(); this.readBinding(binding, token); yield* this.expr(args[1] ?? NIL, ALL); this.emit('ReturnFrom', { token, at: 0 }); return;
      }
      case 'TAGBODY': return yield* this.tagbody(args, dest);
      case 'GO': {
        arity(name, args, 1); const binding = this.scope.find('tags', args[0]); if (!binding) throw new CompileError('GO has no lexically enclosing tag');
        const token = this.reg(); this.readBinding(binding, token); this.emit('Go', { token, label: binding.label, at: 0 }); return;
      }
      case 'CATCH': {
        if (!args.length) throw new CompileError('CATCH requires tag'); const tag = this.reg(); yield* this.expr(args[0], tag); const pc = this.emit('EnterCatch', { tag, resume: 0, clear: this.next }); yield* this.body(args.slice(1), ALL); this.emit('LeaveTarget'); this.patch(pc, 'EnterCatch', { tag, resume: this.code.instructions.length, clear: tag + 1 }); this.receive(dest); return;
      }
      case 'THROW': {
        arity(name, args, 2); const tag = this.reg(); yield* this.expr(args[0], tag); yield* this.expr(args[1], ALL); this.emit('Throw', { tag, at: 0 }); return;
      }
      case 'UNWIND-PROTECT': {
        if (!args.length) throw new CompileError('UNWIND-PROTECT requires protected form'); const clear = this.next, pc = this.emit('EnterProtect', { cleanup: 0, clear }); yield* this.expr(args[0], ALL); this.emit('BeginCleanup'); this.patch(pc, 'EnterProtect', { cleanup: this.code.instructions.length, clear }); for (const f of args.slice(1)) yield* this.expr(f, this.reg()); this.emit('EndCleanup'); this.receive(dest); return;
      }
      case 'PROGV': {
        if (args.length < 2) throw new CompileError('PROGV requires symbols and values'); const symbols = this.reg(), values = this.reg(); yield* this.expr(args[0], symbols); yield* this.expr(args[1], values); this.emit('BindProgv', { symbols, values, at: 0 }); yield* this.body(args.slice(2), dest); this.emit('Unbind', 1); return;
      }
      case 'MULTIPLE-VALUE-CALL': {
        if (!args.length) throw new CompileError('MULTIPLE-VALUE-CALL requires function'); const callee = this.reg(); yield* this.expr(args[0], callee); this.emit('SetValues', range(0, 0)); this.emit('SaveValues'); for (const arg of args.slice(1)) { yield* this.expr(arg, ALL); this.emit('AppendValues'); } this.emit('RestoreValues'); this.emit('CallValues', { callee, at: 0 }); this.receive(dest); return;
      }
      case 'MULTIPLE-VALUE-PROG1': {
        if (!args.length) throw new CompileError('MULTIPLE-VALUE-PROG1 requires first form'); yield* this.expr(args[0], ALL); this.emit('SaveValues'); for (const arg of args.slice(1)) yield* this.expr(arg, this.reg()); this.emit('RestoreValues'); this.receive(dest); return;
      }
      case 'THE': arity(name, args, 2); return yield* this.expr(args[1], dest);
      case 'EVAL-WHEN': {
        if (!args.length) throw new CompileError('EVAL-WHEN requires situations'); const situations = array(args[0]); for (const s of situations) if (!['COMPILE-TOPLEVEL', 'LOAD-TOPLEVEL', 'EXECUTE', 'COMPILE', 'LOAD', 'EVAL'].includes(symbol(s).name)) throw new CompileError('invalid EVAL-WHEN situation');
        return situations.some(s => ['EXECUTE', 'EVAL'].includes(s.name)) ? yield* this.body(args.slice(1), dest) : this.literal(NIL, dest);
      }
      case 'LOAD-TIME-VALUE': {
        arity(name, args, 1, 2);
        if (!this.context?.file) { const result = yield* this.runtime.evalSteps(args[0]); return this.literal(result.values[0] ?? NIL, dest); }
        const cell = new Cell(UNBOUND), initializer = new Compiler(this.runtime, null, new Scope(), null, this.context); yield* initializer.expr(args[0]); this.context.loadTimeValues.push({ cell, code: initializer.code });
        const local = this.reg(); this.literal(cell, local); this.scalar(dest, out => this.emit('ReadBox', { out, cell: { Local: local } })); return;
      }
      case 'DESTRUCTURING-BIND': case '%%DESTRUCTURING-BIND': {
        if (args.length < 2) throw new CompileError('DESTRUCTURING-BIND requires pattern and expression'); const cursor = this.reg(); yield* this.expr(args[1], cursor); return yield* this.withScope(function* () { const decl = this.declarations(args.slice(2), false, false), dynamic = yield* this.destructure(parameters(this.runtime, args[0], true, false), cursor, cursor, NIL, decl.specials); for (const name of decl.specials) this.scope.specials.add(name); yield* this.body(decl.forms, dest); if (dynamic) this.emit('Unbind', dynamic); });
      }
      case 'HANDLER-BIND': case '%%HANDLER-BIND': return yield* this.handlerBind(args, dest);
      case 'HANDLER-CASE': case '%%HANDLER-CASE': return yield* this.handlerCase(args, dest);
      case 'RESTART-BIND': case '%%RESTART-BIND': return yield* this.restartBind(args, dest);
      case 'RESTART-CASE': case '%%RESTART-CASE': return yield* this.restartBind(args, dest, true);
      case 'WITH-CONDITION-RESTARTS': case '%%WITH-CONDITION-RESTARTS': {
        if (args.length < 2) throw new CompileError('WITH-CONDITION-RESTARTS requires condition and restarts'); const condition = this.reg(), restarts = this.reg(); yield* this.expr(args[0], condition); yield* this.expr(args[1], restarts); this.emit('AssociateRestarts', { condition, list: restarts, at: 0 }); yield* this.body(args.slice(2), dest); this.emit('LeaveRestartAssociation'); return;
      }
      case 'DECLARE': throw new CompileError('DECLARE is only allowed at the beginning of a body');
      default: {
        const expansion = yield* expandStandard(this, operator, args);
        if (expansion !== undefined) return yield* this.expr(expansion, dest);
        if (!(operator instanceof LispSymbol) && !(operator instanceof Cons && operator.car === this.cl('LAMBDA'))) throw new CompileError('invalid function operator');
        return yield* this.call(operator, args, dest);
      }
    }
  }
}

function quasiquote(c, datum, level = 1) {
  if (!(datum instanceof Cons)) return c.form('QUOTE', datum);
  const head = datum.car;
  if (head === c.cl('UNQUOTE') || head === c.cl('UNQUOTE-SPLICING') || head === c.cl('UNQUOTE-NSPLICING')) {
    const args = array(datum.cdr); arity(head.name, args, 1);
    if (level === 1) { if (head !== c.cl('UNQUOTE')) throw new CompileError('unquote splice outside list'); return args[0]; }
    return c.form('LIST', c.form('QUOTE', head), quasiquote(c, args[0], level - 1));
  }
  if (head === c.cl('QUASIQUOTE')) { const args = array(datum.cdr); arity('QUASIQUOTE', args, 1); return c.form('LIST', c.form('QUOTE', head), quasiquote(c, args[0], level + 1)); }
  if (datum.car instanceof Cons && [c.cl('UNQUOTE-SPLICING'), c.cl('UNQUOTE-NSPLICING')].includes(datum.car.car) && level === 1) {
    const args = array(datum.car.cdr); arity('unquote splice', args, 1); return c.form('APPEND', args[0], quasiquote(c, datum.cdr, level));
  }
  return c.form('CONS', quasiquote(c, datum.car, level), quasiquote(c, datum.cdr, level));
}

export function* getSetfExpansionSteps(runtime, place, environment = NIL) {
  const c = new Compiler(runtime), f = (name, ...xs) => c.form(name, ...xs);
  let custom, long, short;
  for (let depth = 0; ; depth++) {
    if (depth >= 10000) throw new CompileError('SETF place expansion did not terminate');
    if (place instanceof Cons) {
      custom = runtime.setfExpanders?.get(place.car);
      long = runtime.longDefsetfs?.get(place.car);
      short = runtime.shortDefsetfs?.get(place.car);
      if (custom || long || short) break;
    }
    const [expansion, expanded] = yield* macroexpandSteps(runtime, place, environment, true);
    if (!expanded) break;
    place = expansion;
  }
  if (place instanceof LispSymbol) {
    bindable(place); const store = c.gensym('STORE'); return [NIL, NIL, list(store), f('SETQ', place, store), place];
  }
  const [accessor, ...args] = array(place, 'place'); symbol(accessor);
  if (custom) { const result = yield new CallRequest(custom, [place, environment]); if (result.values.length < 5) throw new CompileError('SETF expander must return five values'); return result.values.slice(0, 5); }
  if (accessor === c.cl('VALUES')) {
    const temps = [], values = [], stores = [], writers = [], readers = [];
    for (const argument of args) {
      const [bindings, forms, storeVariables, writer, reader] = yield* getSetfExpansionSteps(runtime, argument, environment);
      temps.push(...array(bindings));
      values.push(...array(forms));
      const [primaryStore, ...remainingStores] = array(storeVariables);
      stores.push(primaryStore ?? c.gensym('IGNORED-STORE'));
      writers.push(remainingStores.length ? f('LET', fromArray(remainingStores), writer) : writer);
      readers.push(reader);
    }
    return [fromArray(temps), fromArray(values), fromArray(stores), f('PROGN', ...writers, f('VALUES', ...stores)), f('VALUES', ...readers)];
  }
  if (long) {
    const temps = [], values = [], callArgs = [];
    for (let i = 0; i < args.length; i++) {
      if (long.hasKeys && i >= long.positionalCount && args[i] instanceof LispSymbol && (args[i].package === 'KEYWORD' || args[i].package?.name === 'KEYWORD') && i + 1 < args.length) callArgs.push(args[i++]);
      const temp = c.gensym('PLACE'); temps.push(temp); values.push(args[i]); callArgs.push(temp);
    }
    const stores = Array.from({ length: long.storeCount }, () => c.gensym('STORE'));
    const result = yield new CallRequest(long.fn, [fromArray(callArgs), environment, fromArray(stores)]);
    return [fromArray(temps), fromArray(values), fromArray(stores), result.values[0] ?? NIL, list(accessor, ...callArgs)];
  }
  if (!short && (accessor === c.cl('LDB') || accessor === c.cl('MASK-FIELD'))) {
    arity(`${accessor.name} place`, args, 2);
    const [nestedTemps, nestedValues, nestedStores, writer, reader] = yield* getSetfExpansionSteps(runtime, args[1], environment);
    const stores = array(nestedStores), byte = c.gensym('BYTE'), store = c.gensym('STORE');
    if (stores.length === 0) throw new CompileError(`${accessor.name} requires a place with a store variable`);
    const update = accessor.name === 'LDB' ? 'DPB' : 'DEPOSIT-FIELD';
    const bindings = fromArray([list(stores[0], f(update, store, byte, reader)), ...stores.slice(1).map(name => list(name, NIL))]);
    return [new Cons(byte, nestedTemps), new Cons(args[0], nestedValues), list(store),
      f('LET', bindings, writer, store), list(accessor, byte, reader)];
  }
  const temps = args.map(() => c.gensym('PLACE')), store = c.gensym('STORE');
  let storing;
  if (short) storing = list(short, ...temps, store);
  else if (['CAR', 'CDR', 'FIRST', 'REST'].some(name => accessor === c.cl(name))) storing = f('PROGN', f(accessor.name === 'CAR' || accessor.name === 'FIRST' ? 'RPLACA' : 'RPLACD', ...temps, store), store);
  else if (accessor === c.cl('SYMBOL-VALUE')) storing = f('SET', ...temps, store);
  else if (accessor === c.cl('SLOT-VALUE')) storing = f('%SET-SLOT-VALUE', store, ...temps);
  else if (accessor === c.cl('NTH')) { arity('NTH place', args, 2); storing = f('PROGN', f('RPLACA', f('NTHCDR', ...temps), store), store); }
  else if (/^C[AD]{2,4}R$/.test(accessor.name) && accessor === c.cl(accessor.name)) {
    arity('list accessor place', args, 1); const operations = accessor.name.slice(1, -1), outer = operations[0]; let target = temps[0]; for (const op of [...operations.slice(1)].reverse()) target = f(op === 'A' ? 'CAR' : 'CDR', target); storing = f('PROGN', f(outer === 'A' ? 'RPLACA' : 'RPLACD', target, store), store);
  }
  else {
    storing = f('FUNCALL', f('FUNCTION', list(c.cl('SETF'), accessor)), store, ...temps);
  }
  return [fromArray(temps), fromArray(args), list(store), storing, list(accessor, ...temps)];
}

function* setfForm(c, place, value) {
  const [tempList, valueList, storeList, storing] = yield* getSetfExpansionSteps(c.runtime, place, c.scope), temps = array(tempList), values = array(valueList), stores = array(storeList);
  const bindings = temps.map((temp, i) => list(temp, values[i]));
  if (stores.length === 1) return c.form('LET*', fromArray([...bindings, list(stores[0], value)]), storing);
  return c.form('LET*', fromArray(bindings), c.form('MULTIPLE-VALUE-BIND', storeList, value, storing));
}

function* modifyPlace(c, place, operand, operation, beforePlace = false) {
  const [tempList, valueList, storeList, storing, access] = yield* getSetfExpansionSteps(c.runtime, place, c.scope), temps = array(tempList), values = array(valueList), stores = array(storeList);
  if (stores.length !== 1) throw new CompileError('modify macro requires a single-valued place');
  const argument = c.gensym('ARGUMENT'), bindings = temps.map((temp, i) => list(temp, values[i]));
  if (beforePlace) bindings.unshift(list(argument, operand)); else bindings.push(list(argument, operand));
  bindings.push(list(stores[0], operation(access, argument)));
  return c.form('LET*', fromArray(bindings), storing);
}

function* expandStandard(c, operator, args) {
  if (!(operator instanceof LispSymbol) || operator !== c.cl(operator.name)) return undefined;
  const f = (name, ...xs) => c.form(name, ...xs), q = x => f('QUOTE', x), progn = xs => f('PROGN', ...xs), name = operator.name;
  switch (name) {
    case 'IN-PACKAGE': arity(name, args, 1); return f('%%IN-PACKAGE', q(args[0]));
    case 'DEFPACKAGE': if (!args.length) throw new CompileError('DEFPACKAGE requires name'); return f('%%DEFPACKAGE', q(args[0]), q(fromArray(args.slice(1))));
    case 'DEFTYPE': {
      if (args.length < 2) throw new CompileError('DEFTYPE requires name and lambda list'); symbol(args[0]);
      let defaults = false;
      const lambdaList = fromArray(array(args[1]).map(item => {
        if (item instanceof LispSymbol && item.name.startsWith('&')) { defaults = item.name === '&OPTIONAL' || item.name === '&KEY'; return item; }
        if (!defaults) return item; const parts = item instanceof Cons ? array(item) : [item]; return parts.length === 1 ? list(parts[0], q(c.cl('*'))) : item;
      }));
      const decl = c.declarations(args.slice(2), true, false), prefix = [];
      if (decl.documentation) prefix.push(decl.documentation);
      prefix.push(...decl.declarations);
      return f('%%DEFINE-TYPE', q(args[0]), f('FUNCTION', f('LAMBDA', lambdaList, ...prefix, f('BLOCK', args[0], ...decl.forms))));
    }
    case 'DEFSETF': {
      if (args.length < 2) throw new CompileError('DEFSETF requires accessor and definition'); symbol(args[0]);
      if (args[1] instanceof LispSymbol) { arity(name, args, 2, 3); return f('%%DEFINE-SHORT-DEFSETF', q(args[0]), q(args[1])); }
      if (args.length < 3) throw new CompileError('long DEFSETF requires store variables'); const stores = array(args[2]); stores.forEach(bindable);
      let environment = null, positional = 0, inPositional = true, skipRest = false, hasKeys = false;
      const cleaned = [], items = array(args[1]);
      for (let i = 0; i < items.length; i++) {
        const item = items[i]; if (item === c.cl('&ENVIRONMENT')) { if (environment || i + 1 >= items.length) throw new CompileError('invalid DEFSETF &ENVIRONMENT'); environment = bindable(items[++i]); continue; }
        cleaned.push(item);
        if (item instanceof LispSymbol && item.name.startsWith('&')) { if (item.name === '&KEY') { hasKeys = true; inPositional = false; } else if (item.name === '&REST') { skipRest = true; inPositional = false; } else if (item.name === '&AUX') inPositional = false; continue; }
        if (skipRest) { skipRest = false; continue; } if (inPositional) positional++;
      }
      const argumentsVar = c.gensym('ARGUMENTS'), environmentVar = c.gensym('ENVIRONMENT'), storesVar = c.gensym('STORES');
      const decl = c.declarations(args.slice(3), true, false);
      let inner = f('APPLY', f('FUNCTION', f('LAMBDA', fromArray(cleaned), ...decl.declarations, f('BLOCK', args[0], ...decl.forms))), argumentsVar);
      if (environment) inner = f('LET', list(list(environment, environmentVar)), ...(decl.specials.has(environment) ? [f('DECLARE', list(c.cl('SPECIAL'), environment))] : []), inner);
      const specialStores = stores.filter(x => decl.specials.has(x));
      const wrapper = f('LAMBDA', list(argumentsVar, environmentVar, storesVar), f('DESTRUCTURING-BIND', args[2], storesVar, ...(specialStores.length ? [f('DECLARE', list(c.cl('SPECIAL'), ...specialStores))] : []), inner));
      return f('%%DEFINE-LONG-DEFSETF', q(args[0]), f('FUNCTION', wrapper), stores.length, positional, hasKeys ? T : NIL);
    }
    case 'IGNORE-ERRORS': { const condition = c.gensym('CONDITION'); return f('HANDLER-CASE', progn(args), list(c.cl('ERROR'), list(condition), f('VALUES', NIL, condition))); }
    case 'QUASIQUOTE': arity(name, args, 1); return quasiquote(c, args[0]);
    case 'UNQUOTE': case 'UNQUOTE-SPLICING': case 'UNQUOTE-NSPLICING': throw new CompileError('comma outside quasiquote');
    case 'WHEN': case 'UNLESS': if (!args.length) throw new CompileError(`${name} requires test`); return f('IF', args[0], name === 'WHEN' ? progn(args.slice(1)) : NIL, name === 'UNLESS' ? progn(args.slice(1)) : NIL);
    case 'AND': return !args.length ? T : args.length === 1 ? args[0] : f('IF', args[0], f('AND', ...args.slice(1)), NIL);
    case 'OR': { if (!args.length) return NIL; if (args.length === 1) return args[0]; const temp = c.gensym(); return f('LET', list(list(temp, args[0])), f('IF', temp, temp, f('OR', ...args.slice(1)))); }
    case 'COND': { if (!args.length) return NIL; const [test, ...body] = array(args[0]); if (test === undefined) throw new CompileError('empty COND clause'); const more = f('COND', ...args.slice(1)); if (body.length) return f('IF', test, progn(body), more); const temp = c.gensym(); return f('LET', list(list(temp, test)), f('IF', temp, temp, more)); }
    case 'RETURN': arity(name, args, 0, 1); return f('RETURN-FROM', NIL, args[0] ?? NIL);
    case 'PROG1': case 'PROG2': { if (args.length < (name === 'PROG1' ? 1 : 2)) throw new CompileError(`${name} requires forms`); if (name === 'PROG2') return progn([args[0], f('PROG1', ...args.slice(1))]); const temp = c.gensym(); return f('LET', list(list(temp, args[0])), ...args.slice(1), temp); }
    case 'PROG': case 'PROG*': { if (!args.length) throw new CompileError(`${name} requires bindings`); const decl = c.declarations(args.slice(1), false, false); return f('BLOCK', NIL, f(name === 'PROG' ? 'LET' : 'LET*', args[0], ...decl.declarations, f('TAGBODY', ...decl.forms))); }
    case 'DEFVAR': case 'DEFPARAMETER': case 'DEFCONSTANT': {
      arity(name, args, name === 'DEFVAR' ? 1 : 2, 3); const variable = name === 'DEFCONSTANT' ? symbol(args[0]) : bindable(args[0]);
      if (name === 'DEFCONSTANT') return f('%%DEFINE-CONSTANT', q(variable), args[1], args[2] ?? NIL);
      c.emit('ProclaimSpecial', { symbol: c.constant(variable), at: 0 });
      if (name === 'DEFVAR') { if (args.length > 1) { const jump = c.emit('WhenBound', { symbol: c.constant(variable), jump: 0 }); yield* c.set(variable, args[1], c.reg()); c.patch(jump, 'WhenBound', { symbol: c.constant(variable), jump: c.code.instructions.length }); } }
      else yield* c.set(variable, args[1], c.reg());
      if (args.length > 2) c.emit('SetVariableDocumentation', { symbol: c.constant(variable), doc: c.constant(args[2]) });
      return q(variable);
    }
    case 'DECLAIM': {
      for (const arg of args) { const [kind, ...names] = array(arg); if (kind === c.cl('SPECIAL')) for (const sym of names) c.emit('ProclaimSpecial', { symbol: c.constant(bindable(sym)), at: 0 }); }
      return NIL;
    }
    case '%%DEFVAR': return yield* expandStandard(c, c.cl('DEFVAR'), args);
    case '%%DEFPARAMETER': return yield* expandStandard(c, c.cl('DEFPARAMETER'), args);
    case '%%DEFCONSTANT': return yield* expandStandard(c, c.cl('DEFCONSTANT'), args);
    case '%%DECLAIM': return yield* expandStandard(c, c.cl('DECLAIM'), args);
    case 'SETF': {
      if (args.length % 2) throw new CompileError('SETF requires place/value pairs'); if (!args.length) return NIL;
      const [place, value] = args;
      const form = yield* setfForm(c, place, value);
      return args.length === 2 ? form : progn([form, f('SETF', ...args.slice(2))]);
    }
    case 'INCF': case 'DECF': arity(name, args, 1, 2); return yield* modifyPlace(c, args[0], args[1] ?? 1, (access, operand) => f(name === 'INCF' ? '+' : '-', access, operand));
    case 'PUSH': arity(name, args, 2); return yield* modifyPlace(c, args[1], args[0], (access, operand) => f('CONS', operand, access), true);
    case 'PUSHNEW': {
      if (args.length < 2 || args.length % 2 !== 0) throw new CompileError('PUSHNEW expects an item, a place, and keyword/value pairs');
      const [item, place, ...options] = args;
      const [temps, values, stores, storing, access] = yield* getSetfExpansionSteps(c.runtime, place, c.scope);
      const [store = c.gensym('STORE'), ...additional] = array(stores);
      const element = c.gensym('ITEM'), placeValues = array(values);
      const bindings = [list(element, item), ...array(temps).map((temp, i) => list(temp, placeValues[i]))];
      const keywords = options.map((form, i) => {
        if (i % 2 === 0 && form instanceof LispSymbol && form.package?.name === 'KEYWORD') return form;
        const temp = c.gensym('KEY ARG');
        bindings.push(list(temp, form));
        return temp;
      });
      bindings.push(list(store, f('ADJOIN', element, access, ...keywords)), ...additional.map(name => list(name, NIL)));
      return f('LET*', fromArray(bindings), storing, store);
    }
    case 'POP': { arity(name, args, 1); const [temps, vals, stores, storing, access] = yield* getSetfExpansionSteps(c.runtime, args[0], c.scope), store = array(stores); if (store.length !== 1) throw new CompileError('POP requires single-valued place'); const old = c.gensym(); return f('LET*', fromArray([...array(temps).map((temp, i) => list(temp, array(vals)[i])), list(old, access), list(store[0], f('CDR', old))]), storing, f('CAR', old)); }
    case 'PSETQ': case 'PSETF': {
      if (args.length % 2) throw new CompileError(`${name} requires pairs`); const bindings = [], stores = [];
      for (let i = 0; i < args.length; i += 2) {
        if (name === 'PSETQ') { bindable(args[i]); const temp = c.gensym(); bindings.push(list(temp, args[i + 1])); stores.push(f('SETQ', args[i], temp)); }
        else { const [temps, values, storeVars, storing] = yield* getSetfExpansionSteps(c.runtime, args[i], c.scope), names = array(storeVars); bindings.push(...array(temps).map((temp, n) => list(temp, array(values)[n]))); if (names.length === 1) bindings.push(list(names[0], args[i + 1])); else { const temp = c.gensym(); bindings.push(list(temp, f('MULTIPLE-VALUE-LIST', args[i + 1])), ...names.map((name, n) => list(name, f('NTH', n, temp)))); } stores.push(storing); }
      }
      return f('LET*', fromArray(bindings), ...stores, NIL);
    }
    case 'MULTIPLE-VALUE-BIND': {
      if (args.length < 2) throw new CompileError('MULTIPLE-VALUE-BIND requires variables and value form'); const vars = array(args[0]); vars.forEach(bindable); const rest = c.gensym(); return f('MULTIPLE-VALUE-CALL', f('FUNCTION', f('LAMBDA', fromArray([c.cl('&OPTIONAL'), ...vars, c.cl('&REST'), rest]), ...args.slice(2))), args[1]);
    }
    case 'MULTIPLE-VALUE-LIST': arity(name, args, 1); return f('MULTIPLE-VALUE-CALL', f('FUNCTION', c.cl('LIST')), args[0]);
    case 'MULTIPLE-VALUE-SETQ': { arity(name, args, 2); const vars = array(args[0]); vars.forEach(bindable); const temps = vars.map(() => c.gensym()); if (!temps.length) return f('NTH-VALUE', 0, args[1]); return f('MULTIPLE-VALUE-BIND', fromArray(temps), args[1], f('SETQ', ...vars.flatMap((v, i) => [v, temps[i]])), temps[0]); }
    case 'NTH-VALUE': arity(name, args, 2); return f('NTH', args[0], f('MULTIPLE-VALUE-LIST', args[1]));
    case 'CASE': case 'ECASE': case 'TYPECASE': case 'ETYPECASE': {
      if (!args.length) throw new CompileError(`${name} requires key`); const temp = c.gensym(); const clauses = args.slice(1).map(clause => { const [key, ...body] = array(clause); if (key === undefined) throw new CompileError('empty case clause'); const otherwise = key === T || key === c.cl('OTHERWISE'); const test = otherwise ? T : name.includes('TYPECASE') ? f('TYPEP', temp, q(key)) : f('MEMBER', temp, q(key instanceof Cons || key === NIL ? key : list(key))); return list(test, ...body); }); if (name[0] === 'E') clauses.push(list(T, f('ERROR', new LispString('No matching case')))); return f('LET', list(list(temp, args[0])), f('COND', ...clauses));
    }
    default: return c.runtime.expandClosForm?.(list(operator, ...args));
  }
}

export function* macroexpandSteps(runtime, form, environment = NIL, once = false) {
  let expanded = false;
  const hookSymbol = runtime.intern('*MACROEXPAND-HOOK*', 'COMMON-LISP');
  const env = environment instanceof Scope ? environment : NIL;
  for (let depth = 0; depth < 10000; depth++) {
    let expander;
    if (form instanceof LispSymbol) {
      const replacement = environment instanceof Scope ? environment.find('symbolMacros', form) ?? (!environment.find('variables', form) ? runtime.symbolMacros?.get(form) : undefined) : runtime.symbolMacros?.get(form);
      if (replacement !== undefined) expander = new NativeFunction('SYMBOL-MACRO-EXPANDER', args => {
        arity('symbol macro function', args, 2);
        return replacement;
      });
    } else if (form instanceof Cons && form.car instanceof LispSymbol) expander = runtime.macroFunction(form.car, env);
    if (!expander || expander === NIL) return [form, expanded];
    const hook = runtime.readVariable(hookSymbol, runtime.activeTask);
    const result = yield new CallRequest(hook, [expander, form, env]);
    form = result.values[0] ?? NIL;
    expanded = true;
    if (once) return [form, true];
  }
  throw new CompileError('macro expansion did not terminate');
}

function packageDesignator(value) {
  if (value instanceof LispString) return value.toString();
  if (value instanceof LispSymbol) return value.name;
  if (typeof value === 'string') return value;
  throw new CompileError('expected a package or symbol name designator');
}
function definePackage(runtime, name, options) {
  const spelling = packageDesignator(name), clauses = array(options).map(x => array(x)), parsed = new Map();
  const repeatable = new Set(['SHADOW', 'SHADOWING-IMPORT-FROM', 'IMPORT-FROM', 'EXPORT', 'INTERN']);
  for (const [key, ...values] of clauses) {
    const option = symbol(key).name;
    if (!['NICKNAMES', 'USE', 'SHADOW', 'SHADOWING-IMPORT-FROM', 'IMPORT-FROM', 'EXPORT', 'INTERN', 'SIZE', 'DOCUMENTATION'].includes(option)) throw new CompileError(`unsupported DEFPACKAGE option ${option}`);
    if (!repeatable.has(option) && parsed.has(option)) throw new CompileError(`duplicate DEFPACKAGE option ${option}`);
    if (option === 'SIZE') { arity(':SIZE', values, 1); if (!Number.isSafeInteger(values[0]) || values[0] < 0) throw new CompileError('invalid package size'); }
    else if (option === 'DOCUMENTATION') { arity(':DOCUMENTATION', values, 1); if (!(values[0] instanceof LispString)) throw new CompileError('package documentation must be a string'); }
    else for (const value of values) packageDesignator(value);
    if (repeatable.has(option)) { const existing = parsed.get(option) ?? []; existing.push(values); parsed.set(option, existing); } else parsed.set(option, values);
  }
  const use = (parsed.get('USE') ?? ['COMMON-LISP']).map(x => { const pkg = runtime.findPackage(packageDesignator(x)); if (!pkg) throw new CompileError('DEFPACKAGE refers to a missing used package'); return pkg; });
  const imports = [];
  for (const kind of ['SHADOWING-IMPORT-FROM', 'IMPORT-FROM']) for (const [sourceName, ...names] of parsed.get(kind) ?? []) {
    if (sourceName === undefined) throw new CompileError(`${kind} requires a package`);
    const source = runtime.findPackage(packageDesignator(sourceName)); if (!source) throw new CompileError('import source package does not exist');
    for (const item of names) { const sym = runtime.findSymbol(packageDesignator(item), source); if (!sym) throw new CompileError('imported symbol does not exist'); imports.push({ kind, sym }); }
  }
  const nicknames = (parsed.get('NICKNAMES') ?? []).map(packageDesignator);
  let pkg = runtime.findPackage(spelling);
  for (const nickname of nicknames) { const owner = runtime.findPackage(nickname); if (owner && owner !== pkg) throw new CompileError('package nickname is already in use'); }
  if (!pkg) pkg = runtime.makePackage(spelling, { use: [], nicknames });
  for (const nickname of nicknames) { runtime.packages.set(nickname, pkg); pkg.nicknames.add(nickname); }
  for (const names of parsed.get('SHADOW') ?? []) for (const item of names) { const name = packageDesignator(item); if (!pkg.symbols.has(name)) pkg.symbols.set(name, new LispSymbol(name, pkg)); (pkg.shadows ??= new Set()).add(name); }
  for (const { kind, sym } of imports) {
    const existing = runtime.findSymbol(sym.name, pkg);
    if (kind === 'IMPORT-FROM' && existing && existing !== sym) throw new CompileError('conflicting imported symbol');
    pkg.symbols.set(sym.name, sym); if (kind === 'SHADOWING-IMPORT-FROM') (pkg.shadows ??= new Set()).add(sym.name);
  }
  const inherited = new Map();
  for (const used of use) for (const name of used.exports) {
    if (pkg.shadows?.has(name)) continue; const sym = used.symbols.get(name), local = pkg.symbols.get(name), previous = inherited.get(name);
    if (local && local !== sym || previous && previous !== sym) throw new CompileError('conflicting symbols in used packages'); inherited.set(name, sym);
  }
  pkg.uses = use;
  for (const names of parsed.get('INTERN') ?? []) for (const item of names) runtime.intern(packageDesignator(item), pkg);
  for (const names of parsed.get('EXPORT') ?? []) for (const item of names) runtime.exportSymbol(runtime.intern(packageDesignator(item), pkg), pkg);
  if (parsed.has('DOCUMENTATION')) pkg.documentation = parsed.get('DOCUMENTATION')[0];
  return pkg;
}

export function* compileFormSteps(runtime, form) {
  const compiler = new Compiler(runtime); yield* compiler.expr(form); return compiler.code;
}
export function* compileLambdaSteps(runtime, form) {
  const parts = array(form); if (parts[0] !== runtime.intern('LAMBDA', 'COMMON-LISP') || parts.length < 2) throw new CompileError('expected lambda expression');
  const compiler = new Compiler(runtime); return yield* compiler.lambda(parts[1], parts.slice(2));
}
export function* compileStringSteps(runtime, source, { portable = false, onForm } = {}) {
  const compiler = new Compiler(runtime), reader = new Reader(runtime, source);
  const entries = [], loadTimeCells = new Set();
  let emitted = false;
  const process = function* (original, emit = true, evaluate = false) {
    if (!emit && !evaluate) return;
    const form = (yield* macroexpandSteps(runtime, original, compiler.scope, false))[0];
    if (form instanceof Cons) {
      const [head, ...args] = array(form);
      if (head === compiler.cl('PROGN')) { for (const child of args) yield* process(child, emit, evaluate); return; }
      if (head === compiler.cl('LOCALLY')) { yield* compiler.withScope(function* () { const body = compiler.declarations(args); for (const child of body.forms) yield* process(child, emit, evaluate); }); return; }
      if (head === compiler.cl('MACROLET')) {
        if (!args.length) throw new CompileError('MACROLET requires definitions');
        yield* compiler.withScope(function* () {
          yield* compiler.bindLocalMacros(array(args[0]));
          for (const child of compiler.declarations(args.slice(1)).forms) yield* process(child, emit, evaluate);
        }); return;
      }
      if (head === compiler.cl('SYMBOL-MACROLET')) {
        if (!args.length) throw new CompileError('SYMBOL-MACROLET requires definitions');
        yield* compiler.withScope(function* () { for (const spec of array(args[0])) { const pair = array(spec); arity('symbol macro binding', pair, 2); compiler.scope.symbolMacros.set(bindable(pair[0]), pair[1]); } for (const child of compiler.declarations(args.slice(1)).forms) yield* process(child, emit, evaluate); }); return;
      }
      if (head === compiler.cl('EVAL-WHEN')) {
        if (!args.length) throw new CompileError('EVAL-WHEN requires situations');
        const situations = array(args[0]).map(x => symbol(x).name);
        if (situations.some(x => !['COMPILE-TOPLEVEL', 'LOAD-TOPLEVEL', 'EXECUTE', 'COMPILE', 'LOAD', 'EVAL'].includes(x))) throw new CompileError('invalid EVAL-WHEN situation');
        const executes = situations.includes('EXECUTE') || situations.includes('EVAL');
        const compileTime = situations.includes('COMPILE-TOPLEVEL') || situations.includes('COMPILE') || evaluate && executes;
        const loadTime = situations.includes('LOAD-TOPLEVEL') || situations.includes('LOAD');
        for (const child of args.slice(1)) yield* process(child, emit && loadTime, compileTime); return;
      }
      if (['DEFMACRO', '%%DEFMACRO', '%%DEFINE-COMPILER-MACRO', 'DEFTYPE', 'DEFINE-SETF-EXPANDER', '%%DEFINE-SETF-EXPANDER-FORM', 'DEFINE-SYMBOL-MACRO', '%%DEFINE-SYMBOL-MACRO-FORM', 'DEFSETF', 'DECLAIM', '%%DECLAIM', 'DEFPACKAGE', 'IN-PACKAGE', 'DEFCONSTANT', '%%DEFCONSTANT', '%%DEFPACKAGE', '%%IN-PACKAGE', '%%DEFINE-TYPE', '%%DEFINE-SHORT-DEFSETF', '%%DEFINE-LONG-DEFSETF'].some(name => head === compiler.cl(name))) evaluate = true;
      if (['DEFVAR', 'DEFPARAMETER', '%%DEFVAR', '%%DEFPARAMETER'].some(name => head === compiler.cl(name))) { const sym = symbol(args[0]); sym.special = true; runtime.specials?.add(sym); }
    }
    if (!emit && !evaluate) return;
    const context = { file: true, loadTimeValues: [] }, unit = new Compiler(runtime, null, new Scope(compiler.scope), null, context); yield* unit.expr(form); const code = unit.code, fn = runtime.makeFunction(code, { constants: code.constants });
    // A portable entry contains one top-level action and its load-time
    // initializers. Its constants can link after earlier package definitions.
    const output = portable ? new Compiler(runtime) : compiler;
    for (const { cell, code: initializer } of context.loadTimeValues) {
      const initialize = runtime.makeFunction(initializer, { constants: initializer.constants });
      if (evaluate) cell.value = (yield new CallRequest(initialize)).values[0] ?? NIL;
      if (emit) {
        const callee = output.reg(), cellReg = output.reg(), result = output.reg();
        output.literal(initialize, callee);
        output.literal(cell, cellReg);
        output.emit('Call', { callee: { Value: callee }, out: result, args: range(0, 0), at: 0, associate: false, notinline: false });
        output.emit('WriteBox', { cell: { Local: cellReg }, src: result });
        if (portable) loadTimeCells.add(cell);
      }
    }
    if (evaluate) yield new CallRequest(fn);
    if (emit) {
      const callee = output.reg();
      output.literal(fn, callee);
      output.emit('Call', { callee: { Value: callee }, out: ALL, args: range(0, 0), at: 0, associate: false, notinline: false });
      if (portable) entries.push(output.code);
      emitted = true;
    }
  };
  const task = runtime.activeTask;
  task.bind(task.currentFrame, frontendBindings(runtime, task));
  for (;;) {
    const form = yield* reader.readSteps(false, EOF);
    if (form === EOF) break;
    if (onForm) yield* onForm(form);
    yield* process(form);
  }
  task.popScope('special');
  if (portable) return { entries, loadTimeCells };
  if (!emitted) compiler.literal(NIL, ALL);
  return compiler.code;
}

function frontendOperation(name, steps) { return new NativeFunction(name, () => steps); }
function runSteps(runtime, name, steps, task = runtime.activeTask) { return runtime.call(frontendOperation(name, steps), [], task); }
function runStepsAsync(runtime, name, steps, options) { return runtime.callAsync(frontendOperation(name, steps), [], options); }

export function compileForm(runtime, form) { return runSteps(runtime, 'COMPILE-FORM', compileFormSteps(runtime, form)).values[0]; }
export function compileLambda(runtime, form) { return runSteps(runtime, 'COMPILE-LAMBDA', compileLambdaSteps(runtime, form)).values[0]; }
export function compileString(runtime, source) { return runSteps(runtime, 'COMPILE-STRING', compileStringSteps(runtime, source)).values[0]; }
export function macroexpand(runtime, form, environment = NIL, once = false) { return runSteps(runtime, 'MACROEXPAND', macroexpandSteps(runtime, form, environment, once)).values[0]; }
export function getSetfExpansion(runtime, place, environment = NIL) { return runSteps(runtime, 'GET-SETF-EXPANSION', getSetfExpansionSteps(runtime, place, environment)).values[0]; }

function frontendBindings(runtime, task) {
  return ['*PACKAGE*', '*READTABLE*'].map(name => {
    const symbol = runtime.intern(name, 'COMMON-LISP');
    return [symbol, runtime.variableValue(symbol, task)];
  });
}

function* evalReader(runtime, reader, afterForm) {
  let result = NIL;
  for (;;) {
    const form = yield* reader.readSteps(false, EOF);
    if (form === EOF) return result;
    result = yield* runtime.evalSteps(form);
    if (afterForm) yield* afterForm(result);
  }
}

function* openPathForLoad(runtime, pathname, task, externalFormat) {
  const type = pathname.components[4];
  const candidates = [pathname];
  if (type === NIL) {
    const suffixes = ['lisp', 'lsp'];
    if (typeof runtime.pathnameHost?.loadModule === 'function') suffixes.push('mjs');
    for (const suffix of suffixes) {
      const components = pathname.components.slice();
      components[4] = new LispString(pathname.logical ? suffix.toUpperCase() : suffix);
      candidates.push(new LispPathname(components, { logical: pathname.logical }));
    }
  }
  for (const candidate of candidates) {
    const compiled = candidate.components[4] instanceof LispString && candidate.components[4].toString().toLowerCase() === 'mjs';
    const operation = compiled ? 'loadModule' : 'readFile';
    const file = yield* runtime.callPathnameHost(operation, candidate, task, { args: compiled ? [] : [externalFormat] });
    if (file == null || file === NIL) continue;
    if (!compiled && typeof file.contents === 'string' && file.contents.startsWith(compiledModuleHeader)) {
      const module = yield* runtime.callPathnameHost('loadModule', candidate, task, { args: [file.contents] });
      if (module == null || module === NIL) continue;
      return { compiled: true, file: { ...module, truename: module.truename ?? file.truename } };
    }
    return { file, compiled };
  }
  return null;
}

function fileError(pathname, message) { throw new LispCondition('FILE-ERROR', message, { pathname }); }

function installCompilerMacros(runtime) {
  const globals = new Map(), setters = new Map();
  const globalDocs = new Map(), setterDocs = new Map();
  const cl = name => runtime.intern(name, 'COMMON-LISP');
  const funcall = cl('FUNCALL');
  runtime.exportSymbol(cl('COMPILER-MACRO'), 'COMMON-LISP');
  const keyFor = name => {
    try { return functionName(name, runtime); }
    catch (error) {
      if (!(error instanceof CompileError)) throw error;
      throw new LispCondition('TYPE-ERROR', 'Expected a function name', { datum: name });
    }
  };
  const location = name => {
    const key = keyFor(name);
    return key instanceof Cons ? [key.cdr.car, setters, setterDocs] : [key, globals, globalDocs];
  };
  const set = (name, value) => {
    const [key, definitions] = location(name);
    if (value === NIL) definitions.delete(key);
    else {
      if (!(value instanceof Closure) && !(value instanceof LispCode) && typeof value?.invoke !== 'function') {
        throw new LispCondition('TYPE-ERROR', 'Expected a compiler macro function or NIL', { datum: value });
      }
      definitions.set(key, value);
    }
    return value;
  };
  runtime.compilerMacroDocumentation = name => {
    const [key, , docs] = location(name);
    return docs.get(key) ?? NIL;
  };
  runtime.setCompilerMacroDocumentation = (name, documentation) => {
    const [key, , docs] = location(name);
    if (documentation === NIL) docs.delete(key);
    else if (documentation instanceof LispString) docs.set(key, documentation);
    else throw new LispCondition('TYPE-ERROR', 'Expected a documentation string or NIL', { datum: documentation });
    return documentation;
  };
  runtime.defineBuiltin('COMPILER-MACRO-FUNCTION', args => {
    arity('COMPILER-MACRO-FUNCTION', args, 1, 2);
    const [name, environment = NIL] = args, [key, definitions] = location(name);
    if (environment instanceof Scope) {
      const shadowed = definitions === setters ? environment.find('setfFunctions', key) : environment.findMacro(key);
      if (shadowed !== undefined) return NIL;
    } else if (environment !== NIL) throw new LispCondition('TYPE-ERROR', 'Expected a compiler environment or NIL', { datum: environment });
    return definitions.get(key) ?? NIL;
  }, { primitive: 'COMPILER-MACRO-FUNCTION' });
  runtime.defineBuiltin('(SETF COMPILER-MACRO-FUNCTION)', args => {
    arity('(SETF COMPILER-MACRO-FUNCTION)', args, 2, 3);
    if (args[2] !== undefined && args[2] !== NIL) throw new CompileError('Setting a compiler macro requires the null environment');
    return set(args[1], args[0]);
  }, { primitive: '(SETF COMPILER-MACRO-FUNCTION)' });
  runtime.defineBuiltin('%%INSTALL-COMPILER-MACRO', args => {
    arity('%%INSTALL-COMPILER-MACRO', args, 3);
    const [name, expander, documentation] = args;
    expander.documentation = documentation;
    set(name, expander);
    runtime.setCompilerMacroDocumentation(name, documentation);
    return name;
  });
  runtime.defineBuiltin('%%COMPILER-MACRO-ARGUMENTS', args => {
    arity('%%COMPILER-MACRO-ARGUMENTS', args, 1);
    const [form] = args;
    if (!(form instanceof Cons)) throw new CompileError('A compiler macro requires a call form');
    if (form.car !== funcall) return form.cdr;
    if (!(form.cdr instanceof Cons)) throw new CompileError('A FUNCALL compiler macro form requires a function operand');
    return form.cdr.cdr;
  });
}

function installFileOperations(runtime) {
  const specials = new Map();
  for (const name of ['LOAD-PATHNAME', 'LOAD-TRUENAME', 'LOAD-PRINT', 'LOAD-VERBOSE', 'COMPILE-FILE-PATHNAME', 'COMPILE-FILE-TRUENAME', 'COMPILE-PRINT', 'COMPILE-VERBOSE']) {
    const symbol = runtime.intern(`*${name}*`, 'COMMON-LISP');
    symbol.special = true;
    symbol.value = NIL;
    specials.set(name, runtime.exportSymbol(symbol, 'COMMON-LISP'));
  }
  const option = (opts, name, operation, task) => opts.get(name) ?? runtime.readVariable(specials.get(`${operation}-${name}`), task);
  const call = (name, args) => new CallRequest(runtime.intern(name, 'COMMON-LISP'), args);
  const writeLine = text => call('WRITE-LINE', [new LispString(text)]);
  const report = function* (result) {
    for (const value of result.values) yield call('PRINT', [value]);
  };

  runtime.defineBuiltin('LOAD', function* (args, _, task) {
    if (!args.length) throw new CompileError('LOAD requires a file or input stream');
    const [input, ...options] = args;
    const opts = keys(options, ['VERBOSE', 'PRINT', 'IF-DOES-NOT-EXIST', 'EXTERNAL-FORMAT']);
    const stream = runtime.streamClassName(input) !== undefined;
    const verbose = option(opts, 'VERBOSE', 'LOAD', task) !== NIL;
    const print = option(opts, 'PRINT', 'LOAD', task) !== NIL;
    let pathname = NIL, truename = NIL, opened;
    if (stream) {
      const source = resolveSynonymStream(runtime, input, task);
      if (!streamDirection(runtime, source, 'input', task)) throw new LispCondition('STREAM-ERROR', 'LOAD requires an open input stream', { stream: input });
      if (source instanceof LispFileStream) {
        source.checkInput(true);
        pathname = source.pathname;
        truename = (yield call('TRUENAME', [input])).values[0];
      }
    } else {
      pathname = runtime.mergePathnames(input, task);
      const format = opts.get('EXTERNAL-FORMAT') ?? runtime.intern('DEFAULT', 'KEYWORD');
      opened = yield* openPathForLoad(runtime, pathname, task, format);
      if (!opened) {
        if (opts.get('IF-DOES-NOT-EXIST') === NIL) return NIL;
        fileError(pathname, 'File does not exist');
      }
      truename = runtime.hostPathname(opened.file.truename, pathname);
    }
    task.bind(task.currentFrame, [...frontendBindings(runtime, task), [specials.get('LOAD-PATHNAME'), pathname], [specials.get('LOAD-TRUENAME'), truename]]);
    if (verbose) yield writeLine(`; Loading ${stream ? 'input stream' : runtime.pathnameNamestring(truename)}`);
    if (opened?.compiled) {
      const { file } = opened;
      const programs = file.programs ?? (file.program ? [file.program] : null);
      if (!Array.isArray(programs)) fileError(pathname, 'Expected a JavaScript compiled module');
      for (const program of programs) {
        const result = yield new CallRequest(runtime.loadFunction(program));
        if (print) yield* report(result);
      }
    } else if (stream) {
      yield* withStreamReader(runtime, input, task, reader => evalReader(runtime, reader, print ? report : null));
    } else {
      const source = opened.file.contents;
      if (typeof source !== 'string') fileError(pathname, 'Filesystem host must return decoded Lisp source');
      yield* evalReader(runtime, new Reader(runtime, source), print ? report : null);
    }
    return T;
  });

  runtime.defineBuiltin('COMPILE-FILE', function* (args, _, task) {
    if (!args.length) throw new CompileError('COMPILE-FILE requires an input pathname');
    const [input, ...options] = args;
    const opts = keys(options, ['OUTPUT-FILE', 'VERBOSE', 'PRINT', 'EXTERNAL-FORMAT']);
    const pathname = runtime.mergePathnames(input, task);
    const format = opts.get('EXTERNAL-FORMAT') ?? runtime.intern('DEFAULT', 'KEYWORD');
    const opened = yield* runtime.callPathnameHost('readFile', pathname, task, { args: [format] });
    if (opened == null || opened === NIL) fileError(pathname, 'File does not exist');
    if (typeof opened.contents !== 'string') fileError(pathname, 'Filesystem host must return decoded Lisp source');
    const truename = runtime.hostPathname(opened.truename, pathname);
    const output = (yield call('COMPILE-FILE-PATHNAME', [pathname, runtime.intern('OUTPUT-FILE', 'KEYWORD'), opts.get('OUTPUT-FILE') ?? NIL])).values[0];
    const verbose = option(opts, 'VERBOSE', 'COMPILE', task) !== NIL;
    const print = option(opts, 'PRINT', 'COMPILE', task) !== NIL;
    task.bind(task.currentFrame, [[specials.get('COMPILE-FILE-PATHNAME'), pathname], [specials.get('COMPILE-FILE-TRUENAME'), truename]]);
    const sourcePath = runtime.pathnameNamestring(truename);
    if (verbose) yield writeLine(`; Compiling ${sourcePath}`);
    const onForm = print ? function* (form) { yield call('PRINT', [form]); } : null;
    const program = yield* compileStringSteps(runtime, opened.contents, { portable: true, onForm });
    const source = emitResidentModule(runtime, program, sourcePath);
    const written = yield* runtime.callPathnameHost('writeFile', output, task, { args: [source] });
    return runtime.multipleValues(runtime.hostPathname(written, output), NIL, NIL);
  });
}

export function installFrontend(runtime) {
  installReadtable(runtime);
  installMacros(runtime);
  installCompilerMacros(runtime);
  for (const name of FRONTEND_SYMBOLS) { const sym = runtime.intern(name, 'COMMON-LISP'); runtime.exportSymbol?.(sym, 'COMMON-LISP'); }
  const hook = runtime.intern('*MACROEXPAND-HOOK*', 'COMMON-LISP');
  hook.special = true;
  hook.value = runtime.readFunction('FUNCALL');
  runtime.exportSymbol(hook, 'COMMON-LISP');
  runtime.exportSymbol(runtime.intern('DEFINE-SYMBOL-MACRO', 'COMMON-LISP'), 'COMMON-LISP');
  const residentMacros = 'WHEN UNLESS AND OR COND RETURN PROG1 PROG2 PROG PROG* SETF INCF DECF PUSH PUSHNEW POP PSETQ PSETF MULTIPLE-VALUE-BIND MULTIPLE-VALUE-LIST MULTIPLE-VALUE-SETQ NTH-VALUE CASE ECASE TYPECASE ETYPECASE IGNORE-ERRORS IN-PACKAGE DEFPACKAGE DEFTYPE DEFSETF QUASIQUOTE DEFCLASS DEFINE-CONDITION DEFGENERIC DEFMETHOD DEFINE-METHOD-COMBINATION DEFSTRUCT WITH-SLOTS WITH-ACCESSORS'.split(' ');
  const nativeMacro = (name, expand) => ({ name, invoke(args) { arity(`${name} macro function`, args, 2); if (!(args[0] instanceof Cons)) throw new CompileError('macro function requires a whole form'); return expand(...args); } });
  for (const name of residentMacros) {
    const sym = runtime.intern(name, 'COMMON-LISP');
    sym.macro = nativeMacro(name, function* (whole, environment) { const compiler = new Compiler(runtime, null, environment instanceof Scope ? environment : new Scope()); const expanded = yield* expandStandard(compiler, sym, array(whole.cdr)); if (expanded === undefined) throw new CompileError(`no expansion for ${name}`); return expanded; });
  }
  for (const name of ['DEFUN', 'DEFMACRO', 'DEFVAR', 'DEFPARAMETER', 'DEFCONSTANT', 'DECLAIM']) runtime.intern(name, 'COMMON-LISP').macro = nativeMacro(name, whole => list(runtime.intern(`%%${name}`, 'COMMON-LISP'), ...array(whole.cdr)));
  runtime.intern('DEFINE-COMPILER-MACRO', 'COMMON-LISP').macro = nativeMacro('DEFINE-COMPILER-MACRO', whole => list(runtime.intern('%%DEFINE-COMPILER-MACRO', 'COMMON-LISP'), ...array(whole.cdr)));
  for (const name of ['DESTRUCTURING-BIND', 'HANDLER-BIND', 'HANDLER-CASE', 'RESTART-BIND', 'RESTART-CASE', 'WITH-CONDITION-RESTARTS', 'DEFINE-SETF-EXPANDER', 'DEFINE-SYMBOL-MACRO']) runtime.intern(name, 'COMMON-LISP').macro = nativeMacro(name, whole => list(runtime.intern(`%%${name}${name.startsWith('DEFINE-') ? '-FORM' : ''}`, 'COMMON-LISP'), ...array(whole.cdr)));
  runtime.intern('LAMBDA', 'COMMON-LISP').macro = nativeMacro('LAMBDA', whole => list(runtime.intern('FUNCTION', 'COMMON-LISP'), whole));
  runtime.read = (source, options) => read(runtime, source, options);
  runtime.readAll = (source, options) => readAll(runtime, source, options);
  runtime.readFromString = (source, options) => readFromString(runtime, source, options);
  runtime.readAsync = async (source, options) => (await runStepsAsync(runtime, 'READ', readSteps(runtime, source, options), options)).values[0];
  runtime.readAllAsync = async (source, options) => (await runStepsAsync(runtime, 'READ-ALL', readAllSteps(runtime, source, options), options)).values[0];
  runtime.readFromStringAsync = async (source, options) => (await runStepsAsync(runtime, 'READ-FROM-STRING', readFromStringSteps(runtime, source, options), options)).values;
  const existingMacroFunction = runtime.macroFunction?.bind(runtime);
  runtime.macroFunction = (name, env = NIL) => (env instanceof Scope ? env.findMacro(name) : undefined) ?? existingMacroFunction?.(name, env) ?? runtime.macros?.get(name) ?? NIL;
  runtime.macroexpandSteps = (form, environment, once) => macroexpandSteps(runtime, form, environment, once);
  runtime.macroexpand = (form, environment, once) => macroexpand(runtime, form, environment, once);
  runtime.getSetfExpansionSteps = (place, environment) => getSetfExpansionSteps(runtime, place, environment);
  runtime.getSetfExpansion = (place, environment) => getSetfExpansion(runtime, place, environment);
  runtime.setfExpanders ??= new Map(); runtime.shortDefsetfs ??= new Map();
  runtime.defineSetfExpander = (name, expander) => { runtime.setfExpanders.set(symbol(name), expander); return name; };
  runtime.defineShortDefsetf = (name, update) => { runtime.shortDefsetfs.set(symbol(name), symbol(update)); return name; };
  runtime.longDefsetfs ??= new Map(); runtime.typeExpanders ??= new Map();
  runtime.symbolMacros ??= new Map();
  runtime.defineSymbolMacro = (name, expansion) => { bindable(name); if (name.special) throw new CompileError('a special variable cannot be a symbol macro'); runtime.symbolMacros.set(name, expansion); return name; };
  runtime.defineLongDefsetf = (name, fn, storeCount, positionalCount, hasKeys) => { if (!Number.isSafeInteger(storeCount) || storeCount < 0 || !Number.isSafeInteger(positionalCount) || positionalCount < 0) throw new CompileError('invalid DEFSETF descriptor'); runtime.longDefsetfs.set(symbol(name), { fn, storeCount, positionalCount, hasKeys: hasKeys !== NIL && hasKeys !== false }); return name; };
  runtime.defineType = (name, expander) => { runtime.typeExpanders.set(symbol(name), expander); return name; };
  runtime.expandTypeSteps = function* (original, environment = NIL) {
    let type = original, changed = false;
    for (let i = 0; i < 1000; i++) {
      const name = type instanceof Cons ? type.car : type, expander = runtime.typeExpanders.get(name);
      if (!expander) return [type, changed];
      const result = yield new CallRequest(expander, type instanceof Cons ? array(type.cdr) : []); type = result.values[0] ?? NIL; changed = true;
    }
    throw new CompileError('recursive type expansion did not terminate');
  };
  runtime.expandType = (type, environment) => runSteps(runtime, 'EXPAND-TYPE', runtime.expandTypeSteps(type, environment)).values[0];
  runtime.defineBuiltin('%%DEFPACKAGE', args => { arity('%%DEFPACKAGE', args, 2); return definePackage(runtime, args[0], args[1]); });
  runtime.defineBuiltin('%%DEFINE-TYPE', args => { arity('%%DEFINE-TYPE', args, 2); return runtime.defineType(...args); });
  runtime.defineBuiltin('%%DEFINE-SYMBOL-MACRO', args => { arity('%%DEFINE-SYMBOL-MACRO', args, 2); return runtime.defineSymbolMacro(...args); });
  runtime.defineBuiltin('%%DEFINE-SETF-EXPANDER', args => { arity('%%DEFINE-SETF-EXPANDER', args, 2); return runtime.defineSetfExpander(...args); });
  runtime.defineBuiltin('%%DEFINE-SHORT-DEFSETF', args => { arity('%%DEFINE-SHORT-DEFSETF', args, 2); return runtime.defineShortDefsetf(...args); });
  runtime.defineBuiltin('%%DEFINE-LONG-DEFSETF', args => { arity('%%DEFINE-LONG-DEFSETF', args, 5); return runtime.defineLongDefsetf(...args); });
  runtime.defineBuiltin('%%DEFINE-CONSTANT', args => { arity('%%DEFINE-CONSTANT', args, 3); const name = symbol(args[0]); if (name.constant) { if (!eql(name.value, args[1])) throw new CompileError('cannot redefine a constant with a different value'); return name; } runtime.checkMutableSymbol(name); name.value = args[1]; name.constant = true; name.documentation = args[2]; return name; });
  installFileOperations(runtime);
  runtime.compileLambdaSteps = form => compileLambdaSteps(runtime, form);
  const lower = code => { if (runtime.executionStrategy !== 'bytecode') compileJavaScript(code); return code; };
  runtime.compileSteps = function* (form) {
    if (form instanceof Closure) { lower(form.code.entry); return form; }
    if (form instanceof LispCode) { lower(form.entry); return runtime.makeFunction(form, { constants: form.constants }); }
    if (typeof form?.invoke === 'function') return form;
    const code = lower(yield* runtime.compileLambdaSteps(form)); return runtime.makeFunction(code, { constants: code.constants, captures: [] });
  };
  runtime.compileStringSteps = function* (source) {
    const code = lower(yield* compileStringSteps(runtime, source));
    return runtime.makeFunction(code, { constants: code.constants, captures: [] });
  };
  runtime.compile = form => runSteps(runtime, 'COMPILE', runtime.compileSteps(form)).values[0];
  runtime.compileAsync = async (form, options) => (await runStepsAsync(runtime, 'COMPILE', runtime.compileSteps(form), options)).values[0];
  runtime.compileString = source => runSteps(runtime, 'COMPILE-STRING', runtime.compileStringSteps(source)).values[0];
  runtime.compileStringAsync = async (source, options) => (await runStepsAsync(runtime, 'COMPILE-STRING', runtime.compileStringSteps(source), options)).values[0];
  runtime.evalSteps = function* (form) {
    if (form instanceof Cons && form.car === runtime.intern('PROGN', 'COMMON-LISP')) {
      let result = runtime.multipleValues(NIL);
      for (const child of array(form.cdr)) result = yield* runtime.evalSteps(child);
      return result;
    }
    const code = yield* compileFormSteps(runtime, form);
    return yield new CallRequest(runtime.makeFunction(code, { constants: code.constants }));
  };
  runtime.eval = (form, task) => runSteps(runtime, 'EVAL', runtime.evalSteps(form), task);
  runtime.evalAsync = (form, options) => runStepsAsync(runtime, 'EVAL', runtime.evalSteps(form), options);
  runtime.evalStringSteps = function* (source) { return yield* evalReader(runtime, new Reader(runtime, source)); };
  const evalStringOperation = (source, outcome) => frontendOperation('EVAL-STRING', (function* () {
    const result = yield* runtime.evalStringSteps(source);
    outcome.empty = result === NIL;
    return result;
  })());
  runtime.evalString = source => {
    const outcome = {}, result = runtime.call(evalStringOperation(source, outcome));
    return outcome.empty ? NIL : result;
  };
  runtime.evalStringAsync = async (source, options) => {
    const outcome = {}, result = await runtime.callAsync(evalStringOperation(source, outcome), [], options);
    return outcome.empty ? NIL : result;
  };
  return runtime;
}
