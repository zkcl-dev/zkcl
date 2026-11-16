import { NIL, T, UNBOUND, LispSymbol, Cons, LispString, LispArray, LispCharacter, LispPathname, fromArray, toArray, eql, equal } from './objects.mjs';
import { Ratio, Complex, SingleFloat, DoubleFloat, RandomState } from './numbers.mjs';
import { LispCondition, LispRestart, MultipleValues } from './execution.mjs';
import { LispHashTable } from './hash_tables.mjs';
import { installMethodCombinations, expandMethodCombination } from './method_combination.mjs';

// Lisp classes are runtime identities. JavaScript prototypes only implement the
// storage machinery; changing a Lisp class never changes a JS prototype.
export class LispClass {
  constructor(name, { builtin = false } = {}) {
    this.name = name; this.builtin = builtin; this.directSuperclasses = [];
    this.directSlots = []; this.cpl = [this]; this.slots = new Map();
    this.defaultInitargs = new Map(); this.directDefaultInitargs = new Map();
    this.subclasses = new Set(); this.version = 0;
    this.accessorMethods = [];
    this.forwardReferenced = false; this.finalized = true;
    this.metaclassName = builtin ? 'BUILT-IN-CLASS' : 'STANDARD-CLASS';
  }
}
export class StandardInstance {
  constructor(classObject) { initializeStorage(this, classObject); }
}
function initializeStorage(object, cls) {
  object.classObject = cls; object.version = cls.version;
  object.layout = new Map(cls.slots); object.slots = new Map();
  for (const [name, slot] of object.layout) if (slot.allocation === 'INSTANCE') object.slots.set(name, UNBOUND);
}
export class EqlSpecializer { constructor(object) { this.object = object; } }
export class StructureInstance {
  constructor(classObject, values) { this.kind = 'structure-instance'; this.classObject = classObject; this.values = [...values]; }
}
export class Method {
  constructor({ specializers = [], qualifiers = [], fn, lambdaList = NIL, publicEntry = NIL, documentation = NIL, inline = false }) {
    if (qualifiers.some(q => q === NIL || q instanceof Cons)) fail('Method qualifiers must be non-NIL atoms');
    this.specializers = specializers; this.qualifiers = [...qualifiers];
    this.fn = fn; this.lambdaList = lambdaList; this.genericFunction = null;
    this.publicEntry = publicEntry; this.documentation = documentation; this.inline = inline;
  }
}
const spelling = x => x instanceof LispSymbol ? x.name : String(x).replace(/^:/, '').toUpperCase();
const array = x => Array.isArray(x) ? x : toArray(x);
const primary = x => x instanceof MultipleValues ? (x.values[0] ?? NIL) : x;
// Public JS entry points are synchronous reentry boundaries. Lisp invocation
// uses the generators directly so each outstanding call lives in Task frames.
function driveSynchronous(iterator, rt, task) {
  try {
    let step = iterator.next();
    while (!step.done) step = iterator.next(rt.call(step.value.callee, step.value.args, task));
    return step.value;
  } catch (error) { iterator.return(); throw error; }
}
function fail(message) { throw new LispCondition('PROGRAM-ERROR', message); }
function requiredParameters(lambdaList) {
  const names = [];
  for (const item of array(lambdaList)) { if (spelling(item).startsWith('&')) break; names.push(item); }
  return names;
}
function signature(lambdaList) {
  const result = { required: 0, optional: 0, rest: false, key: false, keys: [], allowOtherKeys: false };
  let mode = 'required';
  for (const item of array(lambdaList)) {
    const name = spelling(item);
    if (name.startsWith('&')) {
      if (name === '&REST' || name === '&BODY') { result.rest = true; mode = 'rest'; }
      else if (name === '&KEY') { result.key = true; mode = 'key'; }
      else if (name === '&OPTIONAL') mode = 'optional';
      else if (name === '&ALLOW-OTHER-KEYS') result.allowOtherKeys = true;
      else if (name === '&AUX') mode = 'aux';
      continue;
    }
    if (mode === 'required') result.required++;
    else if (mode === 'optional') result.optional++;
    else if (mode === 'key') {
      const variable = item instanceof Cons ? item.car : item;
      result.keys.push(variable instanceof Cons ? array(variable)[0] : spelling(variable));
    }
  }
  return result;
}
function keywordMatches(key, actual) {
  return key instanceof LispSymbol ? key === actual : actual instanceof LispSymbol && key === actual.name && spelling(actual.package?.name ?? actual.package) === 'KEYWORD';
}
function congruent(method, generic) {
  if (method.required !== generic.required || method.optional !== generic.optional || (method.rest || method.key) !== (generic.rest || generic.key)) return false;
  if (generic.key && method.key && !method.allowOtherKeys)
    return generic.keys.every(k => method.keys.some(m => typeof k === 'string' && typeof m === 'string' ? k === m : typeof k === 'string' ? keywordMatches(k, m) : typeof m === 'string' ? keywordMatches(m, k) : k === m));
  return true;
}
function implicitGenericLambdaList(methodLambdaList) {
  const parameters = []; let mode = 'required';
  for (const item of array(methodLambdaList)) {
    if (item instanceof LispSymbol && spelling(item).startsWith('&')) {
      const marker = spelling(item);
      if (marker === '&AUX' || marker === '&ALLOW-OTHER-KEYS') { mode = 'ignore'; continue; }
      parameters.push(item); mode = marker; continue;
    }
    if (mode === 'required' || mode === '&OPTIONAL' || mode === '&REST' || mode === '&BODY')
      parameters.push(item instanceof Cons ? item.car : item);
  }
  return fromArray(parameters);
}
function precedence(cls) {
  const classes = [], edges = new Map();
  const visit = c => {
    if (classes.includes(c)) return;
    classes.push(c); edges.set(c, new Set());
    for (const s of c.directSuperclasses) visit(s);
  };
  visit(cls);
  for (const c of classes) {
    const local = [c, ...c.directSuperclasses];
    for (let i = 1; i < local.length; i++) edges.get(local[i]).add(local[i - 1]);
  }
  const result = [];
  while (result.length < classes.length) {
    const candidates = classes.filter(c => !result.includes(c) && [...edges.get(c)].every(p => result.includes(p)));
    if (!candidates.length) fail('Inconsistent class precedence constraints');
    let chosen;
    for (let i = result.length - 1; i >= 0 && !chosen; i--)
      chosen = result[i].directSuperclasses.find(c => candidates.includes(c));
    result.push(chosen ?? candidates[0]);
  }
  return result;
}
function effectiveSlots(cls) {
  const result = new Map();
  for (const c of cls.cpl) for (const slot of c.directSlots) {
    let effective = result.get(slot.name);
    if (!effective) {
      effective = { ...slot, owner: c, initargs: [...slot.initargs] };
      if (slot.allocation === 'CLASS') effective.cell = slot.cell ?? (slot.cell = { value: UNBOUND });
      result.set(slot.name, effective);
    } else {
      for (const key of slot.initargs) if (!effective.initargs.includes(key)) effective.initargs.push(key);
      if (!effective.hasInitform && slot.hasInitform) {
        effective.hasInitform = true; effective.initfunction = slot.initfunction;
      }
    }
  }
  return result;
}
export class GenericFunction {
  constructor(name, { lambdaList = NIL, argumentPrecedenceOrder = null, documentation = NIL, methodCombination = null } = {}) {
    this.name = name; this.lambdaList = lambdaList; this.required = requiredParameters(lambdaList);
    this.argumentPrecedenceOrder = validateArgumentOrder(argumentPrecedenceOrder, this.required.length);
    this.methods = []; this.documentation = documentation; this.methodCombination = methodCombination;
  }
  applicable(args, rt) {
    if (args.length < this.required.length) fail('Too few arguments to generic function');
    const classes = args.map(x => rt.classOf(x));
    return this.methods.filter(m => m.specializers.every((s, i) => s instanceof EqlSpecializer ? eql(args[i], s.object) : classes[i]?.cpl.includes(s)))
      .sort((a, b) => {
        for (const i of this.argumentPrecedenceOrder) {
          const x = a.specializers[i], y = b.specializers[i];
          if (sameSpecializer(x, y)) continue;
          if (x instanceof EqlSpecializer) return -1;
          if (y instanceof EqlSpecializer) return 1;
          const delta = classes[i].cpl.indexOf(x) - classes[i].cpl.indexOf(y);
          if (delta) return delta;
        }
        return 0;
      });
  }
  *invoke(args, rt, task) {
    const applicable = this.applicable(args, rt);
    if (!applicable.length) return yield rt.requestCall(rt.intern('NO-APPLICABLE-METHOD', 'COMMON-LISP'), [this, ...args]);
    this.checkArguments(args, applicable);
    const validateNextArgs = actual => {
      const changed = this.applicable(actual, rt);
      if (changed.length !== applicable.length || changed.some((m, i) => m !== applicable[i]))
        fail('CALL-NEXT-METHOD arguments change the ordered applicable methods');
      this.checkArguments(actual, changed);
      return actual;
    };
    if (this.methodCombination !== null) return yield* rt.methodCombinations.invoke(this.methodCombination, this, applicable, args, validateNextArgs);

    const around = [], before = [], after = [], primaries = [];
    const groups = new Map([['AROUND', around], ['BEFORE', before], ['AFTER', after]].map(([name, methods]) => [rt.intern(name, 'KEYWORD'), methods]));
    for (const method of applicable) {
      const qualifiers = method.qualifiers;
      const group = qualifiers.length === 0 ? primaries : qualifiers.length === 1 && groups.get(qualifiers[0]);
      if (!group) fail('Invalid qualifier for STANDARD method combination');
      group.push(method);
    }
    if (!primaries.length) fail('No primary method');
    after.reverse();
    const generic = this;
    const core = function* (actual, _rt, activeTask) {
      for (const method of before) yield* invokeMethod(rt, activeTask, method, actual, null, undefined, generic);
      const saved = yield* invokeMethodChain(rt, activeTask, generic, primaries, actual, validateNextArgs);
      for (const method of after) yield* invokeMethod(rt, activeTask, method, actual, null, undefined, generic);
      return saved;
    };
    if (!around.length) return yield* core(args, rt, task);
    return yield* invokeMethodChain(rt, task, generic, [...around, new Method({ fn: { invoke: core } })], args, validateNextArgs);
  }
  checkArguments(args, applicable) {
    const sig = signature(this.lambdaList);
    if (!sig.rest && !sig.key && args.length > sig.required + sig.optional) fail('Too many arguments to generic function');
    if (sig.key) {
      const start = sig.required + Math.min(sig.optional, args.length - sig.required);
      if ((args.length - start) % 2) fail('Odd number of generic keyword arguments');
      const methods = applicable.map(m => signature(m.lambdaList));
      const accepted = [...sig.keys, ...methods.flatMap(s => s.keys)];
      let allow = sig.allowOtherKeys || methods.some(s => s.allowOtherKeys), foundOverride = false;
      for (let i = start; i < args.length; i += 2) if (spelling(args[i]) === 'ALLOW-OTHER-KEYS' && !foundOverride) { allow ||= args[i + 1] !== NIL; foundOverride = true; }
      if (!allow) for (let i = start; i < args.length; i += 2)
        if (spelling(args[i]) !== 'ALLOW-OTHER-KEYS' && !accepted.some(k => keywordMatches(k, args[i]))) fail('Unrecognized generic keyword argument');
    }
  }
}
function* invokeMethodChain(rt, task, generic, methods, args, validateNextArgs, explicitWithoutNext = false) {
  const chain = function* (index, actual, activeTask) {
    const more = index + 1 < methods.length;
    const next = more ? (changed, nextTask) => chain(index + 1, changed === null ? actual : validateNextArgs(changed), nextTask) : null;
    return yield* invokeMethod(rt, activeTask, methods[index], actual, next, () => methods.slice(index + 1), generic, explicitWithoutNext && index === 0);
  };
  return yield* chain(0, args, task);
}
function* invokeMethod(rt, task, method, args, next, nextMethods = () => [], generic = method.genericFunction, explicitWithoutNext = false) {
  const stack = task.closMethodContexts ?? (task.closMethodContexts = []);
  stack.push({ next, nextMethods, args, generic, method, explicitWithoutNext });
  try { return yield rt.requestCall(method.fn, args); }
  finally { stack.pop(); }
}
function validateArgumentOrder(order, count) {
  if (order == null) return Array.from({ length: count }, (_, i) => i);
  if (order.length !== count || new Set(order).size !== count || order.some(i => !Number.isInteger(i) || i < 0 || i >= count)) fail('Invalid argument precedence order');
  return [...order];
}
function sameSpecializer(a, b) { return a === b || (a instanceof EqlSpecializer && b instanceof EqlSpecializer && eql(a.object, b.object)); }
function sameSpecializers(a, b) { return a.length === b.length && a.every((s, i) => sameSpecializer(s, b[i])); }
function sameQualifiers(a, b) { return a.length === b.length && a.every((q, i) => equal(q, b[i])); }

export function installClos(rt) {
  const classes = new Map(), generics = new Map();
  const structures = new Map();
  const lookupFunction = name => {
    try { return rt.readFunction(name); } catch (error) { if (error.className !== 'UNDEFINED-FUNCTION') throw error; return null; }
  };
  const setfNames = new Map();
  const genericKey = name => {
    if (!(name instanceof Cons)) return name;
    const parts = array(name);
    if (parts.length !== 2 || spelling(parts[0]) !== 'SETF' || !(parts[1] instanceof LispSymbol)) fail('Invalid function name');
    if (!setfNames.has(parts[1])) setfNames.set(parts[1], {});
    return setfNames.get(parts[1]);
  };
  const intern = name => rt.intern(name, 'COMMON-LISP');
  const mop = rt.findPackage('ZK-MOP') ?? rt.makePackage('ZK-MOP', { use: ['COMMON-LISP'] });
  const methodCombinations = installMethodCombinations(rt, mop, {
    signature, invokeMethodChain, sameSpecializers, isMethod: value => value instanceof Method,
    makeMethod: (entry, dispatch, sampler) => new Method({ fn: { invoke(args, _rt, task) {
      const context = task.closMethodContexts.at(-1);
      const next = { invoke: (changed, _rt, nextTask) => callNext(context, changed, nextTask) };
      return rt.requestCall(entry, [next, context.next ? T : NIL, fromArray(args), dispatch, sampler]);
    } } }),
  });
  rt.methodCombinations = methodCombinations;
  const normalizeQualifiers = qualifiers => array(qualifiers).map(q => typeof q === 'string' ? rt.intern(spelling(q), 'KEYWORD') : q);
  for (const name of ['WITH-SLOTS', 'WITH-ACCESSORS']) rt.exportSymbol(intern(name), 'COMMON-LISP');
  const builtin = (name, supers = []) => {
    const cls = new LispClass(intern(name), { builtin: true });
    cls.directSuperclasses = supers.map(s => classes.get(s)); cls.cpl = precedence(cls);
    for (const parent of cls.directSuperclasses) parent.subclasses.add(cls);
    classes.set(name, cls); classes.set(cls.name, cls); return cls;
  };
  builtin('T'); builtin('STANDARD-OBJECT', ['T']); builtin('CLASS', ['STANDARD-OBJECT']);
  builtin('STANDARD-CLASS', ['CLASS']); builtin('BUILT-IN-CLASS', ['CLASS']);
  builtin('FUNCALLABLE-STANDARD-CLASS', ['CLASS']);
  builtin('FORWARD-REFERENCED-CLASS', ['CLASS']);
  builtin('STRUCTURE-CLASS', ['CLASS']); builtin('STRUCTURE-OBJECT', ['T']);
  builtin('FUNCTION', ['T']); builtin('GENERIC-FUNCTION', ['FUNCTION', 'STANDARD-OBJECT']);
  builtin('STANDARD-GENERIC-FUNCTION', ['GENERIC-FUNCTION']); builtin('METHOD', ['STANDARD-OBJECT']);
  builtin('STANDARD-METHOD', ['METHOD']); builtin('SYMBOL', ['T']); builtin('SEQUENCE', ['T']); builtin('LIST', ['SEQUENCE']);
  builtin('NULL', ['SYMBOL', 'LIST']); builtin('CONS', ['LIST']); builtin('NUMBER', ['T']);
  builtin('REAL', ['NUMBER']); builtin('RATIONAL', ['REAL']); builtin('INTEGER', ['RATIONAL']);
  builtin('RATIO', ['RATIONAL']); builtin('FLOAT', ['REAL']); builtin('DOUBLE-FLOAT', ['FLOAT']); builtin('SINGLE-FLOAT', ['FLOAT']);
  builtin('COMPLEX', ['NUMBER']); builtin('ARRAY', ['T']); builtin('VECTOR', ['ARRAY', 'SEQUENCE']);
  builtin('STRING', ['VECTOR']); builtin('CHARACTER', ['T']); builtin('HASH-TABLE', ['T']);
  builtin('RESTART', ['T']);
  builtin('STREAM', ['T']); builtin('STRING-STREAM', ['STREAM']);
  builtin('TWO-WAY-STREAM', ['STREAM']); builtin('FILE-STREAM', ['STREAM']);
  for (const name of ['BROADCAST-STREAM', 'CONCATENATED-STREAM', 'ECHO-STREAM', 'SYNONYM-STREAM']) builtin(name, ['STREAM']);
  builtin('RANDOM-STATE', ['T']);
  for (const [name, ...parents] of [
    ['CONDITION', 'T'], ['SERIOUS-CONDITION', 'CONDITION'], ['WARNING', 'CONDITION'], ['ERROR', 'SERIOUS-CONDITION'],
    ['SIMPLE-CONDITION', 'CONDITION'], ['SIMPLE-WARNING', 'SIMPLE-CONDITION', 'WARNING'], ['SIMPLE-ERROR', 'SIMPLE-CONDITION', 'ERROR'],
    ['PROGRAM-ERROR', 'ERROR'], ['CONTROL-ERROR', 'ERROR'], ['TYPE-ERROR', 'ERROR'], ['CELL-ERROR', 'ERROR'],
    ['UNBOUND-VARIABLE', 'CELL-ERROR'], ['UNDEFINED-FUNCTION', 'CELL-ERROR'], ['ARITHMETIC-ERROR', 'ERROR'],
    ['PARSE-ERROR', 'ERROR'], ['STREAM-ERROR', 'ERROR'], ['READER-ERROR', 'PARSE-ERROR', 'STREAM-ERROR'], ['END-OF-FILE', 'STREAM-ERROR'],
    ['PACKAGE-ERROR', 'ERROR'], ['FILE-ERROR', 'ERROR'], ['PRINT-NOT-READABLE', 'ERROR'], ['STYLE-WARNING', 'WARNING'],
    ['DIVISION-BY-ZERO', 'ARITHMETIC-ERROR'], ['FLOATING-POINT-OVERFLOW', 'ARITHMETIC-ERROR'], ['FLOATING-POINT-UNDERFLOW', 'ARITHMETIC-ERROR'],
    ['FLOATING-POINT-INVALID-OPERATION', 'ARITHMETIC-ERROR'], ['FLOATING-POINT-INEXACT', 'ARITHMETIC-ERROR'],
    ['UNBOUND-SLOT', 'CELL-ERROR'], ['SIMPLE-TYPE-ERROR', 'SIMPLE-CONDITION', 'TYPE-ERROR'], ['STORAGE-CONDITION', 'SERIOUS-CONDITION'],
    ['FORMAT-ERROR', 'PROGRAM-ERROR'],
  ]) {
    const cls = builtin(name, parents); cls.condition = true; cls.metaclassName = 'STANDARD-CLASS';
  }
  for (const [className, ...fields] of [
    ['SIMPLE-CONDITION', ['FORMAT-CONTROL', 'formatControl'], ['FORMAT-ARGUMENTS', 'formatArguments']],
    ['TYPE-ERROR', ['DATUM', 'datum'], ['EXPECTED-TYPE', 'expectedType']], ['CELL-ERROR', ['NAME', 'cellName']],
    ['UNBOUND-SLOT', ['INSTANCE', 'instance']], ['ARITHMETIC-ERROR', ['OPERATION', 'operation'], ['OPERANDS', 'operands']],
    ['STREAM-ERROR', ['STREAM', 'stream']], ['PACKAGE-ERROR', ['PACKAGE', 'package']], ['FILE-ERROR', ['PATHNAME', 'pathname']],
    ['PRINT-NOT-READABLE', ['OBJECT', 'object']],
  ]) classes.get(className).directSlots = fields.map(([name, conditionField]) => ({
    name: intern(name), initargs: [rt.intern(name, 'KEYWORD')], allocation: 'INSTANCE', conditionField,
    hasInitform: conditionField === 'formatArguments', initfunction: { invoke: () => NIL },
  }));
  for (const [name, cls] of classes) if (typeof name === 'string' && cls.condition) cls.slots = effectiveSlots(cls);
  builtin('PATHNAME', ['T']); builtin('LOGICAL-PATHNAME', ['PATHNAME']);
  for (const name of ['STANDARD-OBJECT', 'CLASS', 'STANDARD-CLASS', 'BUILT-IN-CLASS', 'FUNCALLABLE-STANDARD-CLASS', 'FORWARD-REFERENCED-CLASS', 'STRUCTURE-CLASS', 'METHOD', 'STANDARD-METHOD']) classes.get(name).metaclassName = 'STANDARD-CLASS';
  for (const name of ['GENERIC-FUNCTION', 'STANDARD-GENERIC-FUNCTION']) classes.get(name).metaclassName = 'FUNCALLABLE-STANDARD-CLASS';
  classes.get('STRUCTURE-OBJECT').metaclassName = 'STRUCTURE-CLASS';
  for (const [name, cls] of classes) if (typeof name === 'string') rt.exportSymbol(cls.name, 'COMMON-LISP');
  const findClass = (name, errorp = true) => {
    if (name instanceof LispClass) return name;
    const cls = classes.get(name) ?? (name instanceof LispSymbol && name.package && ['COMMON-LISP', 'CL'].includes(spelling(name.package.name ?? name.package)) ? classes.get(name.name) : undefined);
    if (!cls && errorp) fail(`Unknown class ${spelling(name)}`);
    return cls ?? null;
  };
  const classOf = object => {
    if (object instanceof StandardInstance || object instanceof StructureInstance) return object.classObject;
    if (object instanceof LispCondition) return object.classObject ?? classes.get(object.className) ?? classes.get('ERROR');
    if (object instanceof LispClass) return classes.get(object.structure ? 'STRUCTURE-CLASS' : object.forwardReferenced ? 'FORWARD-REFERENCED-CLASS' : object.metaclassName);
    if (object instanceof GenericFunction) return classes.get('STANDARD-GENERIC-FUNCTION');
    if (object instanceof Method) return classes.get('STANDARD-METHOD');
    if (object instanceof LispRestart) return classes.get('RESTART');
    const streamClass = rt.streamClassName?.(object);
    if (streamClass !== undefined) return classes.get(streamClass);
    if (object === NIL) return classes.get('NULL');
    if (object instanceof LispSymbol) return classes.get('SYMBOL');
    if (object instanceof Cons) return classes.get('CONS');
    if (typeof object === 'bigint') return classes.get('INTEGER');
    if (typeof object === 'number') return classes.get(Number.isInteger(object) ? 'INTEGER' : 'DOUBLE-FLOAT');
    if (typeof object === 'string' || object instanceof LispString) return classes.get('STRING');
    if (object instanceof LispArray) return classes.get(object.dimensions.length === 1 ? 'VECTOR' : 'ARRAY');
    if (Array.isArray(object) || ArrayBuffer.isView(object)) return classes.get('VECTOR');
    if (object instanceof LispHashTable) return classes.get('HASH-TABLE');
    if (object instanceof LispCharacter) return classes.get('CHARACTER');
    if (object instanceof LispPathname) return classes.get(object.logical ? 'LOGICAL-PATHNAME' : 'PATHNAME');
    if (object instanceof Ratio) return classes.get('RATIO');
    if (object instanceof Complex) return classes.get('COMPLEX');
    if (object instanceof SingleFloat) return classes.get('SINGLE-FLOAT');
    if (object instanceof DoubleFloat) return classes.get('DOUBLE-FLOAT');
    if (object instanceof RandomState) return classes.get('RANDOM-STATE');
    if (typeof object === 'function' || typeof object?.invoke === 'function') return classes.get('FUNCTION');
    return classes.get('T');
  };
  const normalizeSlot = spec => {
    if (spec instanceof LispSymbol) spec = { name: spec };
    if (!spec || !(spec.name instanceof LispSymbol)) fail('Slot name must be a symbol');
    const allocation = spelling(spec.allocation ?? 'INSTANCE');
    if (!['CLASS', 'INSTANCE'].includes(allocation)) fail('Invalid slot allocation');
    return { ...spec, allocation, initargs: [...(spec.initargs ?? [])], hasInitform: spec.hasInitform ?? spec.initfunction !== undefined };
  };
  const refinalize = (cls, visited = new Set()) => {
    if (visited.has(cls)) return; visited.add(cls);
    cls.cpl = precedence(cls); cls.slots = effectiveSlots(cls); cls.defaultInitargs = new Map();
    cls.finalized = cls.cpl.every(c => !c.forwardReferenced);
    for (const c of [...cls.cpl].reverse()) for (const entry of c.directDefaultInitargs) cls.defaultInitargs.set(...entry);
    cls.version++;
    for (const child of cls.subclasses) refinalize(child, visited);
  };
  const ensureClassSteps = function* (name, supers = [], slots = [], options = {}, task) {
    if (!(name instanceof LispSymbol) || name === NIL) fail('Class name must be a non-NIL symbol');
    const condition = options.condition === true;
    if (options.metaclass && findClass(options.metaclass) !== classes.get('STANDARD-CLASS')) fail('Custom metaclasses are not implemented by this backend');
    let cls = classes.get(name);
    if (cls?.builtin || cls?.structure) fail('Cannot redefine this class');
    if (cls && !cls.forwardReferenced && !!cls.condition !== condition) fail('Cannot change between a condition class and a standard class');
    if (options.initialize === false && cls?.declarationActive) return cls;
    const parents = array(supers).map(s => {
      const existing = findClass(s, false);
      if (existing) {
        if (condition !== !!existing.condition) fail('Condition classes must inherit from condition classes');
        return existing;
      }
      if (!(s instanceof LispSymbol)) fail('Superclass name must be a symbol');
      if (condition) fail(`Unknown condition superclass ${spelling(s)}`);
      const forward = new LispClass(s); forward.forwardReferenced = true; forward.finalized = false;
      classes.set(s, forward); return forward;
    });
    if (!parents.length) parents.push(classes.get(condition ? 'CONDITION' : 'STANDARD-OBJECT'));
    if (new Set(parents).size !== parents.length) fail('Duplicate direct superclass');
    const directSlots = array(slots).map(normalizeSlot);
    if (new Set(directSlots.map(s => s.name)).size !== directSlots.length) fail('Duplicate direct slot');
    cls ??= new LispClass(name);
    if (parents.some(p => p === cls || p.cpl.includes(cls))) fail('Circular class inheritance');
    const previous = { directSuperclasses: cls.directSuperclasses, directSlots: cls.directSlots, directDefaultInitargs: cls.directDefaultInitargs, forwardReferenced: cls.forwardReferenced, condition: cls.condition };
    for (const slot of directSlots) {
      const old = cls.directSlots.find(s => s.name === slot.name && s.allocation === 'CLASS');
      if (old && slot.allocation === 'CLASS') slot.cell = old.cell;
    }
    cls.directSuperclasses = parents; cls.directSlots = directSlots;
    cls.forwardReferenced = false; cls.condition = condition;
    cls.directDefaultInitargs = new Map(options.defaultInitargs ?? []);
    const validateDescendants = (current, seen = new Set()) => {
      if (seen.has(current)) return; seen.add(current); precedence(current);
      for (const child of current.subclasses) validateDescendants(child, seen);
    };
    try { validateDescendants(cls); } catch (error) { Object.assign(cls, previous); throw error; }
    for (const parent of previous.directSuperclasses) parent.subclasses.delete(cls);
    for (const parent of parents) parent.subclasses.add(cls);
    refinalize(cls);
    classes.set(name, cls);
    if (options.initialize === false) return cls;
    cls.declarationActive = true;
    for (const method of cls.accessorMethods) if (method.genericFunction) {
      const gf = method.genericFunction, index = gf.methods.indexOf(method);
      if (index >= 0) gf.methods.splice(index, 1); method.genericFunction = null;
    }
    cls.accessorMethods = [];
    for (const slot of directSlots) {
      for (const reader of slot.readers ?? []) cls.accessorMethods.push(ensureMethod(reader, {
        lambdaList: fromArray([rt.intern('OBJECT')]), specializers: [cls],
      }, { invoke: (a, _rt, currentTask) => slotOperation(a[0], slot.name, 'get', undefined, currentTask) }));
      for (const writer of slot.writers ?? []) cls.accessorMethods.push(ensureMethod(writer, {
        lambdaList: fromArray([rt.intern('VALUE'), rt.intern('OBJECT')]), specializers: [classes.get('T'), cls],
      }, { invoke: (a, _rt, currentTask) => slotOperation(a[1], slot.name, 'set', a[0], currentTask) }));
    }
    const initializeShared = function* (current, seen = new Set()) {
      if (seen.has(current)) return; seen.add(current);
      for (const slot of current.directSlots) if (options.initialize !== false && current.finalized && slot.allocation === 'CLASS' && slot.hasInitform && slot.cell.value === UNBOUND)
        slot.cell.value = primary(yield rt.requestCall(slot.initfunction, []));
      for (const child of current.subclasses) yield* initializeShared(child, seen);
    };
    yield* initializeShared(cls);
    return cls;
  };
  const ensureClass = (...args) => driveSynchronous(ensureClassSteps(...args), rt, args[4]);
  const instance = object => {
    if (object instanceof LispCondition) {
      if (!object.classObject) initializeStorage(object, classOf(object));
      return object;
    }
    if (!(object instanceof StandardInstance)) throw new LispCondition('TYPE-ERROR', 'Expected a standard instance', { datum: object, expectedType: classes.get('STANDARD-OBJECT') });
    return object;
  };
  const initializationValues = (object, slotNames, initargs = [], validate = null, methodArgs = [object]) => {
    instance(object); const args = array(initargs);
    if (args.length % 2 || args.some((arg, i) => i % 2 === 0 && !(arg instanceof LispSymbol))) fail('Initialization arguments must be symbol/value pairs');
    const values = new Map();
    for (let i = 0; i < args.length; i += 2) if (!values.has(args[i])) values.set(args[i], args[i + 1]);
    const override = rt.intern('ALLOW-OTHER-KEYS', 'KEYWORD');
    const allow = values.has(override) && values.get(override) !== NIL;
    const initializationMethods = [validate, 'SHARED-INITIALIZE'].filter(Boolean).flatMap(name => {
      const gf = generics.get(intern(name)); return gf ? gf.applicable(name === 'SHARED-INITIALIZE' ? [object, slotNames] : methodArgs, rt) : [];
    });
    const methodSignatures = initializationMethods.map(m => signature(m.lambdaList));
    const methodKeys = methodSignatures.flatMap(s => s.keys);
    if (validate && !allow && !methodSignatures.some(s => s.allowOtherKeys)) for (const key of values.keys())
      if (key !== override && ![...object.classObject.slots.values()].some(s => s.initargs.includes(key)) && !methodKeys.some(k => keywordMatches(k, key))) fail(`Invalid initialization argument ${spelling(key)}`);
    return values;
  };
  const sharedInitialize = function* (object, slotNames, initargs = [], task) {
    const values = initializationValues(object, slotNames, initargs);
    for (const [name, slot] of object.classObject.slots) {
      const key = [...values.keys()].find(k => slot.initargs.includes(k));
      if (key !== undefined) { rawSet(object, slot, values.get(key)); continue; }
      if ((slotNames === T || array(slotNames).includes(name)) && rawGet(object, slot) === UNBOUND && slot.hasInitform)
        rawSet(object, slot, primary(yield rt.requestCall(slot.initfunction, [])));
    }
    return object;
  };
  const rawGet = (object, slot) => {
    if (slot.allocation === 'CLASS') return slot.cell.value;
    if (slot.conditionField) {
      const value = object[slot.conditionField];
      return Array.isArray(value) ? fromArray(value) : value ?? UNBOUND;
    }
    return object.slots.get(slot.name) ?? UNBOUND;
  };
  const rawSet = (object, slot, value) => {
    if (slot.allocation === 'CLASS') slot.cell.value = value;
    else if (slot.conditionField) object[slot.conditionField] = value;
    else object.slots.set(slot.name, value);
    return value;
  };
  const withDefaultInitargs = function* (cls, args) {
    const supplied = [...args], keys = new Set(supplied.filter((_, i) => !(i % 2)));
    for (const [key, fn] of cls.defaultInitargs) if (!keys.has(key))
      supplied.push(key, primary(yield rt.requestCall(fn, [])));
    return supplied;
  };
  const makeConditionSteps = function* (type, args) {
    const cls = findClass(type, false);
    if (!cls?.condition) throw new LispCondition('TYPE-ERROR', 'Expected a condition type', { datum: type });
    if (args.length % 2 || args.some((arg, i) => i % 2 === 0 && !(arg instanceof LispSymbol))) fail('Condition initargs must be symbol/value pairs');
    const object = new LispCondition(cls.name.name, cls.name.name);
    initializeStorage(object, cls);
    const supplied = yield* withDefaultInitargs(cls, args);
    initializationValues(object, T, supplied, 'MAKE-CONDITION');
    yield* sharedInitialize(object, T, supplied);
    return object;
  };
  const migrate = function* (object, task) {
    instance(object);
    if (object.version === object.classObject.version) return object;
    const added = [], discarded = [], discardedValues = [];
    const current = object.classObject.slots, old = object.layout, next = new Map();
    for (const [name, slot] of current) if (slot.allocation === 'INSTANCE') {
      const previous = old.get(name);
      if (previous) next.set(name, rawGet(object, previous));
      else { next.set(name, UNBOUND); added.push(name); }
    }
    for (const [name, slot] of old) if (slot.allocation === 'INSTANCE' && (!current.has(name) || current.get(name).allocation !== 'INSTANCE')) {
      discarded.push(name); const value = object.slots.get(name); if (value !== UNBOUND) discardedValues.push(name, value);
    }
    object.slots = next; object.layout = new Map(current); object.version = object.classObject.version;
    const hook = generics.get(intern('UPDATE-INSTANCE-FOR-REDEFINED-CLASS'));
    if (object instanceof LispCondition) yield* sharedInitialize(object, fromArray(added));
    else yield rt.requestCall(hook, [object, fromArray(added), fromArray(discarded), fromArray(discardedValues)]);
    return object;
  };
  const missingSlotSteps = function* (object, name, operation, value) {
    const operationName = { get: 'SLOT-VALUE', bound: 'SLOT-BOUNDP', unbind: 'SLOT-MAKUNBOUND', set: 'SETF' }[operation];
    const more = operation === 'set' ? [value] : [];
    const result = yield rt.requestCall(generics.get(intern('SLOT-MISSING')), [object.classObject, object, name, intern(operationName), ...more]);
    if (operation === 'set') return value;
    if (operation === 'unbind') return object;
    const answer = primary(result);
    return operation === 'bound' ? (answer === NIL ? NIL : T) : answer;
  };
  const slotOperation = function* (object, name, operation, value, task) {
    if (!(name instanceof LispSymbol)) throw new LispCondition('TYPE-ERROR', 'Slot name must be a symbol', { datum: name, expectedType: intern('SYMBOL') });
    if (object instanceof StructureInstance) {
      const definition = object.classObject.structureDefinition, index = definition.slots.findIndex(s => s.name === name);
      if (index < 0) return yield* missingSlotSteps(object, name, operation, value);
      if (operation === 'bound') return T;
      if (operation === 'unbind') fail('Structure slots cannot be made unbound');
      return operation === 'set' ? writeStructureSlot(object, definition, index, value) : object.values[index];
    }
    yield* migrate(object, task);
    const slot = object.classObject.slots.get(name);
    if (!slot) return yield* missingSlotSteps(object, name, operation, value);
    if (operation === 'set') return rawSet(object, slot, value);
    if (operation === 'unbind') { rawSet(object, slot, UNBOUND); return object; }
    const result = rawGet(object, slot);
    if (operation === 'bound') return result === UNBOUND ? NIL : T;
    if (result === UNBOUND) return primary(yield rt.requestCall(generics.get(intern('SLOT-UNBOUND')), [object.classObject, object, name]));
    return result;
  };
  const ensureGenericFunction = (name, options = {}, replaceOrdinary = false, defining = false) => {
    const key = genericKey(name);
    const existing = lookupFunction(name);
    if (existing && !(existing instanceof GenericFunction) && !replaceOrdinary) fail('Generic function name already denotes an ordinary function');
    const methodCombination = options.methodCombination === undefined ? undefined : methodCombinations.parse(options.methodCombination);
    let gf = existing instanceof GenericFunction ? existing : null;
    if (!gf) {
      gf = new GenericFunction(name, { ...options, methodCombination }); generics.set(key, gf); rt.defineFunction(name, gf);
    }
    else {
      const methods = defining ? gf.methods.filter(m => !m.inline) : gf.methods;
      const lambdaList = options.lambdaList ?? gf.lambdaList;
      const required = requiredParameters(lambdaList);
      if (methods.some(m => !congruent(signature(m.lambdaList), signature(lambdaList)))) fail('Incongruent generic lambda list');
      const previousOrder = gf.argumentPrecedenceOrder.map(i => required.indexOf(gf.required[i]));
      const order = validateArgumentOrder(options.argumentPrecedenceOrder ?? (required.length === previousOrder.length && previousOrder.every(i => i >= 0) ? previousOrder : null), required.length);
      if (defining) for (const method of gf.methods) if (method.inline) method.genericFunction = null;
      gf.methods = methods; gf.lambdaList = lambdaList; gf.required = required;
      gf.argumentPrecedenceOrder = order;
      if (options.documentation !== undefined) gf.documentation = options.documentation;
      if (methodCombination !== undefined || defining) gf.methodCombination = methodCombination ?? null;
    }
    return gf;
  };
  const ensureMethod = (generic, options, fn) => {
    const specializers = options.specializers.map(s => s instanceof EqlSpecializer ? s : findClass(s));
    const existing = generic instanceof GenericFunction ? generic : lookupFunction(generic);
    if (existing && !(existing instanceof GenericFunction)) fail('Method name already denotes an ordinary function');
    const gf = existing ?? ensureGenericFunction(generic, { lambdaList: implicitGenericLambdaList(options.lambdaList) });
    if (specializers.length !== gf.required.length) fail('Method and generic function required arguments are incongruent');
    const methodSignature = signature(options.lambdaList), genericSignature = signature(gf.lambdaList);
    if (!congruent(methodSignature, genericSignature))
      fail('Method and generic function lambda lists are incongruent');
    const qualifiers = normalizeQualifiers(options.qualifiers ?? []);
    const method = new Method({ ...options, specializers, qualifiers, fn: fn ?? options.fn });
    const old = gf.methods.findIndex(m => sameQualifiers(m.qualifiers, qualifiers) && sameSpecializers(m.specializers, specializers));
    if (old >= 0) { gf.methods[old].genericFunction = null; gf.methods.splice(old, 1); }
    method.genericFunction = gf; gf.methods.push(method); return method;
  };
  Object.assign(rt, { findClass, classOf, ensureClass, ensureGenericFunction, ensureMethod, makeConditionSteps, structures, structureSourceDefinitions: new Map(),
    isInstanceOf: (object, cls) => classOf(object).cpl.includes(findClass(cls)),
    slotValueSteps: (object, name, task) => slotOperation(object, name, 'get', undefined, task),
    isStandardInstance: value => value instanceof StandardInstance,
    slotValue: (o, n, task) => driveSynchronous(slotOperation(o, n, 'get', undefined, task), rt, task),
    setSlotValue: (o, n, v, task) => driveSynchronous(slotOperation(o, n, 'set', v, task), rt, task),
    expandClosForm: form => expandClosForm(rt, form) });
  const handlers = new Map();
  const arities = {
    'FIND-CLASS': [1, 3], 'CLASS-OF': [1, 1], 'CLASS-NAME': [1, 1], 'CLASS-PRECEDENCE-LIST': [1, 1],
    'CLASS-DIRECT-SUPERCLASSES': [1, 1], 'CLASS-DIRECT-SUBCLASSES': [1, 1], 'CLASS-FINALIZED-P': [1, 1], 'FINALIZE-INHERITANCE': [1, 1],
    'MAKE-INSTANCE': [1], 'ALLOCATE-INSTANCE': [1], 'INITIALIZE-INSTANCE': [1], 'REINITIALIZE-INSTANCE': [1], 'SHARED-INITIALIZE': [2],
    'UPDATE-INSTANCE-FOR-REDEFINED-CLASS': [4], 'UPDATE-INSTANCE-FOR-DIFFERENT-CLASS': [2], 'CHANGE-CLASS': [2],
    'SLOT-VALUE': [2, 2], 'SLOT-BOUNDP': [2, 2], 'SLOT-MAKUNBOUND': [2, 2], 'SLOT-EXISTS-P': [2, 2], '%SET-SLOT-VALUE': [3, 3],
    'MAKE-INSTANCES-OBSOLETE': [1, 1], 'CALL-NEXT-METHOD': [0], 'NEXT-METHOD-P': [0, 0], 'COMPUTE-APPLICABLE-METHODS': [2, 2],
    'GENERIC-FUNCTION-METHODS': [1, 1], 'METHOD-SPECIALIZERS': [1, 1], 'METHOD-QUALIFIERS': [1, 1], 'ADD-METHOD': [2, 2], 'REMOVE-METHOD': [2, 2], 'FIND-METHOD': [3, 4],
    'SLOT-MISSING': [4, 5], 'SLOT-UNBOUND': [3, 3], 'NO-APPLICABLE-METHOD': [1], 'NO-NEXT-METHOD': [2], 'PRINT-OBJECT': [2, 2],
    'COPY-STRUCTURE': [1, 1], 'ENSURE-CLASS': [1], 'ENSURE-GENERIC-FUNCTION': [1], 'MAKE-LOAD-FORM': [1, 2], 'MAKE-LOAD-FORM-SAVING-SLOTS': [1],
    '(SETF FIND-CLASS)': [2, 3],
    '%%REGISTER-STRUCTURE': [5, 5], '%%STRUCTURE-DEFAULT': [2, 2], '%%MAKE-STRUCTURE': [1], '%%READ-STRUCTURE-SLOT': [3, 3], '%%WRITE-STRUCTURE-SLOT': [4, 4], '%%STRUCTURE-PREDICATE': [2, 2], '%%COPY-STRUCTURE': [2, 2],
    '%%DEFINE-CLASS': [6, 6], '%%DEFINE-CONDITION': [6, 6], '%%DEFINE-GENERIC': [5, 5], '%%DEFINE-METHOD': [6, 6],
  };
  const define = (name, fn) => {
    const [minimum, maximum = Infinity] = arities[name];
    const checked = (args, runtime, task) => {
      if (args.length < minimum || args.length > maximum) fail(`Wrong number of arguments to ${name}`);
      return fn(args, runtime, task);
    };
    handlers.set(name, checked);
    if (['ENSURE-CLASS', 'CLASS-PRECEDENCE-LIST', 'CLASS-DIRECT-SUPERCLASSES', 'CLASS-DIRECT-SUBCLASSES', 'CLASS-FINALIZED-P', 'FINALIZE-INHERITANCE', 'GENERIC-FUNCTION-METHODS', 'METHOD-SPECIALIZERS'].includes(name)) {
      const symbol = rt.intern(name, mop); rt.exportSymbol(symbol, mop);
      const callable = { invoke: checked }; rt.defineFunction(symbol, callable); return callable;
    }
    return rt.defineBuiltin(name, checked);
  };
  define('FIND-CLASS', args => {
    if (!(args[0] instanceof LispSymbol)) throw new LispCondition('TYPE-ERROR', 'Class name must be a symbol', { datum: args[0] });
    return findClass(args[0], args[1] !== NIL) ?? NIL;
  });
  define('(SETF FIND-CLASS)', args => {
    const [value, name] = args;
    if (!(name instanceof LispSymbol) || (value !== NIL && !(value instanceof LispClass))) throw new LispCondition('TYPE-ERROR', 'Expected a class binding and a symbol name', { datum: value });
    if (classes.get(name)?.builtin) fail('Cannot change a built-in class binding');
    if (value === NIL) classes.delete(name); else classes.set(name, value);
    return value;
  });
  define('CLASS-OF', args => classOf(args[0]));
  define('CLASS-NAME', args => findClass(args[0]).name);
  define('CLASS-PRECEDENCE-LIST', args => { const cls = findClass(args[0]); if (!cls.finalized) fail('Class is not finalized'); return fromArray(cls.cpl); });
  define('CLASS-FINALIZED-P', args => findClass(args[0]).finalized ? T : NIL);
  define('FINALIZE-INHERITANCE', args => { const cls = findClass(args[0]); if (!cls.finalized) fail('Class has forward-referenced superclasses'); return T; });
  define('CLASS-DIRECT-SUPERCLASSES', args => fromArray(findClass(args[0]).directSuperclasses));
  define('CLASS-DIRECT-SUBCLASSES', args => fromArray([...findClass(args[0]).subclasses]));
  define('ALLOCATE-INSTANCE', args => {
    const cls = findClass(args[0]);
    if (!cls.finalized) fail('Cannot allocate a class with forward references');
    if (cls.structure) return new StructureInstance(cls, Array(cls.structureDefinition.slots.length).fill(NIL));
    if (cls.condition) {
      const condition = new LispCondition(cls.name.name, cls.name.name);
      initializeStorage(condition, cls);
      return condition;
    }
    if (cls.builtin && cls !== classes.get('STANDARD-OBJECT')) fail('Cannot allocate this class using ALLOCATE-INSTANCE');
    return new StandardInstance(cls);
  });
  define('MAKE-INSTANCE', function* (args, _rt, task) {
    const cls = findClass(args[0]);
    if (args[0] instanceof LispSymbol) return yield rt.requestCall(generics.get(intern('MAKE-INSTANCE')), [cls, ...args.slice(1)]);
    if (!cls.finalized) fail('Cannot allocate a class with forward references');
    if (cls.condition) return yield* makeConditionSteps(cls, args.slice(1));
    const supplied = yield* withDefaultInitargs(cls, args.slice(1));
    const object = primary(yield rt.requestCall(generics.get(intern('ALLOCATE-INSTANCE')), [cls, ...supplied]));
    const init = generics.get(intern('INITIALIZE-INSTANCE'));
    initializationValues(object, T, supplied, 'INITIALIZE-INSTANCE');
    yield rt.requestCall(init, [object, ...supplied]);
    return object;
  });
  define('SHARED-INITIALIZE', function* (args, _rt, task) { yield* migrate(args[0], task); return yield* sharedInitialize(args[0], args[1], args.slice(2), task); });
  define('INITIALIZE-INSTANCE', args => rt.requestCall(generics.get(intern('SHARED-INITIALIZE')), [args[0], T, ...args.slice(1)]));
  define('REINITIALIZE-INSTANCE', function* (args, _rt, task) {
    yield* migrate(args[0], task);
    initializationValues(args[0], NIL, args.slice(1), 'REINITIALIZE-INSTANCE');
    return yield rt.requestCall(generics.get(intern('SHARED-INITIALIZE')), [args[0], NIL, ...args.slice(1)]);
  });
  define('UPDATE-INSTANCE-FOR-REDEFINED-CLASS', args => rt.requestCall(generics.get(intern('SHARED-INITIALIZE')), [args[0], args[1], ...args.slice(4)]));
  define('UPDATE-INSTANCE-FOR-DIFFERENT-CLASS', (args, _rt, task) => {
    const previous = instance(args[0]), current = instance(args[1]);
    const added = fromArray([...current.classObject.slots].filter(([name, slot]) => slot.allocation === 'INSTANCE' && !previous.layout.has(name)).map(([name]) => name));
    initializationValues(current, added, args.slice(2), 'UPDATE-INSTANCE-FOR-DIFFERENT-CLASS', [previous, current]);
    return rt.requestCall(generics.get(intern('SHARED-INITIALIZE')), [current, added, ...args.slice(2)]);
  });
  define('CHANGE-CLASS', function* (args, _rt, task) {
    const cls = findClass(args[1]);
    if (args[1] instanceof LispSymbol) return yield rt.requestCall(generics.get(intern('CHANGE-CLASS')), [args[0], cls, ...args.slice(2)]);
    if (!cls.finalized) fail('Cannot change an instance to a class with forward references');
    yield* migrate(args[0], task);
    const object = instance(args[0]);
    if ((cls.builtin && cls !== classes.get('STANDARD-OBJECT')) || cls.structure || cls.condition || object instanceof LispCondition) fail('Cannot change an instance to this class');
    const previous = new StandardInstance(object.classObject);
    previous.layout = object.layout; previous.slots = object.slots; previous.version = object.version;
    const next = new Map();
    for (const [name, slot] of cls.slots) if (slot.allocation === 'INSTANCE') {
      const previousSlot = previous.layout.get(name);
      next.set(name, previousSlot ? rawGet(previous, previousSlot) : UNBOUND);
    }
    object.classObject = cls; object.version = cls.version; object.layout = new Map(cls.slots); object.slots = next;
    yield rt.requestCall(generics.get(intern('UPDATE-INSTANCE-FOR-DIFFERENT-CLASS')), [previous, object, ...args.slice(2)]);
    return object;
  });
  define('SLOT-VALUE', (args, _rt, task) => slotOperation(args[0], args[1], 'get', undefined, task));
  define('%SET-SLOT-VALUE', (args, _rt, task) => slotOperation(args[1], args[2], 'set', args[0], task));
  define('SLOT-BOUNDP', (args, _rt, task) => slotOperation(args[0], args[1], 'bound', undefined, task));
  define('SLOT-MAKUNBOUND', (args, _rt, task) => slotOperation(args[0], args[1], 'unbind', undefined, task));
  define('SLOT-EXISTS-P', args => {
    if (!(args[1] instanceof LispSymbol)) throw new LispCondition('TYPE-ERROR', 'Slot name must be a symbol', { datum: args[1] });
    const object = args[0];
    return (object instanceof StandardInstance || object instanceof StructureInstance || object instanceof LispCondition) && classOf(object).slots.has(args[1]) ? T : NIL;
  });
  define('MAKE-INSTANCES-OBSOLETE', args => { const cls = findClass(args[0]); refinalize(cls); return cls; });
  const callNext = (context, args, task) => {
    if (context.next) return context.next(args.length ? args : null, task);
    if (context.explicitWithoutNext) throw new LispCondition('CONTROL-ERROR', 'CALL-METHOD supplied no next methods');
    return rt.requestCall(generics.get(intern('NO-NEXT-METHOD')), [context.generic, context.method, ...(args.length ? args : context.args)]);
  };
  define('CALL-NEXT-METHOD', (args, _rt, task) => {
    const context = task?.closMethodContexts?.at(-1);
    if (!context) fail('No next method outside a method invocation');
    return callNext(context, args, task);
  });
  define('NEXT-METHOD-P', (_args, _rt, task) => task?.closMethodContexts?.at(-1)?.next ? T : NIL);
  define('COMPUTE-APPLICABLE-METHODS', args => fromArray(args[0].applicable(array(args[1]), rt)));
  define('GENERIC-FUNCTION-METHODS', args => fromArray(args[0].methods));
  define('METHOD-SPECIALIZERS', args => fromArray(args[0].specializers));
  define('METHOD-QUALIFIERS', args => fromArray(args[0].qualifiers));
  define('NO-APPLICABLE-METHOD', args => fail(`No applicable method for ${spelling(args[0].name)}`));
  define('NO-NEXT-METHOD', () => fail('No next method'));
  define('SLOT-MISSING', args => { throw new LispCondition('ERROR', `Missing slot ${spelling(args[2])}`, { instance: args[1], slotName: args[2] }); });
  define('SLOT-UNBOUND', args => { throw new LispCondition('UNBOUND-SLOT', `Unbound slot ${spelling(args[2])}`, { instance: args[1], slotName: args[2] }); });
  define('PRINT-OBJECT', (args, _rt, task) => rt.printObjectFallbackSteps(args[0], args[1], task));
  define('REMOVE-METHOD', args => { const [gf, method] = args; const i = gf.methods.indexOf(method); if (i >= 0) { gf.methods.splice(i, 1); method.genericFunction = null; } return gf; });
  define('ADD-METHOD', args => {
    const [gf, method] = args;
    if (!(gf instanceof GenericFunction) || !(method instanceof Method)) fail('ADD-METHOD requires a generic function and method');
    if (method.genericFunction && method.genericFunction !== gf) fail('Method already belongs to another generic function');
    const installed = ensureMethod(gf, method, method.fn);
    const index = gf.methods.indexOf(installed);
    gf.methods[index] = method;
    installed.genericFunction = null;
    Object.assign(method, { qualifiers: installed.qualifiers, specializers: installed.specializers, genericFunction: gf });
    return gf;
  });
  define('FIND-METHOD', args => {
    const [gf, qualifiers, specs, errorp = T] = args, q = normalizeQualifiers(qualifiers), s = array(specs);
    if (s.length !== gf.required.length) fail('Wrong number of method specializers');
    const result = gf.methods.find(m => sameQualifiers(m.qualifiers, q) && sameSpecializers(m.specializers, s));
    if (!result && errorp !== NIL) fail('Method not found'); return result ?? NIL;
  });
  const defineSourceClass = (args, task, condition) => {
    const specs = array(args[2]).map(s => { const a = array(s); return { name: a[0], initargs: array(a[1]), hasInitform: a[2] !== NIL, initfunction: a[3], allocation: a[4] }; });
    for (const entry of array(args[5])) {
      const [name, reader, writer] = array(entry), slot = specs.find(s => s.name === name);
      if (reader !== NIL) (slot.readers ??= []).push(reader);
      if (writer !== NIL) (slot.writers ??= []).push(writer);
    }
    const defaults = array(args[3]); const pairs = []; for (let i = 0; i < defaults.length; i += 2) pairs.push([defaults[i], defaults[i + 1]]);
    return ensureClassSteps(args[0], args[1], specs, { condition, defaultInitargs: pairs,
      ...(condition ? { initialize: args[4] !== NIL } : { metaclass: args[4] === NIL ? undefined : args[4] }) }, task);
  };
  define('%%DEFINE-CLASS', (args, _rt, task) => defineSourceClass(args, task, false));
  define('%%DEFINE-CONDITION', (args, _rt, task) => defineSourceClass(args, task, true));
  const argumentOrder = (names, lambdaList) => names === NIL ? undefined : array(names).map(n => requiredParameters(lambdaList).indexOf(n));
  const defineGeneric = ([name, lambdaList, order, methodCombination, documentation]) => ensureGenericFunction(name, {
    lambdaList, argumentPrecedenceOrder: argumentOrder(order, lambdaList), methodCombination, documentation,
  }, false, true);
  define('%%DEFINE-GENERIC', defineGeneric);
  const eqlSymbol = intern('EQL');
  const decodeSpecializers = specs => array(specs).map(s => s instanceof Cons && s.car === eqlSymbol ? new EqlSpecializer(s.cdr) : s);
  const methodCall = rt.intern('*METHOD-CALL*', mop);
  methodCall.value = NIL; rt.specials?.add(methodCall);
  const compiledMethod = (name, qualifiers, specializers, lambdaList, fast, publicEntry = NIL, documentation = NIL, inline = NIL) => {
    if (fast === NIL && publicEntry === NIL) fail('Compiled method has no entry');
    return ensureMethod(name, { qualifiers: array(qualifiers), specializers: decodeSpecializers(specializers), lambdaList, publicEntry, documentation, inline: inline !== NIL }, {
      invoke: function* (actual, _rt, task) {
        const context = task.closMethodContexts.at(-1);
        const next = { invoke: (forwarded, _rt, nextTask) => callNext(context, forwarded, nextTask) };
        if (publicEntry !== NIL) {
          const args = fromArray(actual);
          task.bind(task.currentFrame, [[methodCall, new Cons(args, next)]]);
          return yield rt.requestCall(publicEntry, [args, fromArray(context.nextMethods())]);
        }
        return yield rt.requestCall(fast, [next, context.next ? T : NIL, ...actual]);
      },
    });
  };
  define('%%DEFINE-METHOD', args => compiledMethod(args[0], args[1], args[2], args[3], args[4], NIL, NIL, args[5]));
  const methodFunction = method => {
    if (!(method instanceof Method)) fail('METHOD-FUNCTION expects a method');
    if (method.publicEntry !== NIL) return method.publicEntry;
    return method.methodFunction ??= { invoke: ([argumentsList, nextList], _rt, task) => {
      const next = nextList === NIL ? null : (changed, _task) => rt.requestCall(methodFunction(nextList.car), [changed === null ? argumentsList : fromArray(changed), nextList.cdr]);
      return invokeMethod(rt, task, method, array(argumentsList), next, () => array(nextList));
    } };
  };
  const plist = value => {
    const a = array(value); if (a.length % 2) fail('Expected a property list');
    const result = new Map();
    for (let i = 0; i < a.length; i += 2) if (!result.has(spelling(a[i]))) result.set(spelling(a[i]), a[i + 1]);
    return result;
  };
  const canonicalSlots = value => array(value).map(spec => {
    const p = plist(spec), fn = p.get('INITFUNCTION') ?? NIL;
    return { name: p.get('NAME'), allocation: p.get('ALLOCATION') ?? 'INSTANCE', initargs: array(p.get('INITARGS') ?? NIL),
      readers: array(p.get('READERS') ?? NIL), writers: array(p.get('WRITERS') ?? NIL),
      initfunction: fn, hasInitform: fn !== NIL, initform: p.get('INITFORM') ?? NIL, type: p.get('TYPE') ?? T };
  });
  const canonicalDefaults = value => array(value).map(entry => {
    const fields = array(entry); if (fields.length !== 3) fail('Expected canonical default initarg'); return [fields[0], fields[2]];
  });
  const ensureClassFunction = (args, _rt, task) => {
    const options = plist(args.slice(1));
    return ensureClassSteps(args[0], options.get('DIRECT-SUPERCLASSES') ?? [], canonicalSlots(options.get('DIRECT-SLOTS') ?? NIL), {
      defaultInitargs: canonicalDefaults(options.get('DIRECT-DEFAULT-INITARGS') ?? NIL),
      metaclass: options.get('METACLASS'),
    }, task);
  };
  define('ENSURE-CLASS', ensureClassFunction);
  const genericOptions = args => {
    const p = plist(args.slice(1)), lambdaList = p.get('LAMBDA-LIST');
    const order = p.get('ARGUMENT-PRECEDENCE-ORDER');
    for (const [key, standard] of [['GENERIC-FUNCTION-CLASS', 'STANDARD-GENERIC-FUNCTION'], ['METHOD-CLASS', 'STANDARD-METHOD']])
      if (p.has(key) && findClass(p.get(key)) !== classes.get(standard)) fail(`Custom :${key} is not implemented`);
    return { lambdaList, documentation: p.get('DOCUMENTATION'), methodCombination: p.get('METHOD-COMBINATION'),
      argumentPrecedenceOrder: order === undefined ? undefined : argumentOrder(order, lambdaList ?? lookupFunction(args[0])?.lambdaList ?? NIL) };
  };
  define('ENSURE-GENERIC-FUNCTION', args => ensureGenericFunction(args[0], genericOptions(args)));
  const installers = rt.installers ?? (rt.installers = new Map());
  const registerStructure = (name, parentName, slots, representation = null, keywordConstructor = NIL) => {
    if (!(name instanceof LispSymbol)) fail('Structure name must be a symbol');
    const parent = parentName === NIL || parentName == null ? null : structures.get(parentName);
    if (parentName !== NIL && parentName != null && !parent) fail('Unknown included structure');
    if (parent && !!parent.representation !== !!representation) fail('Incompatible included structure representation');
    if (new Set(slots.map(s => s.name.name)).size !== slots.length) fail('Duplicate structure slot');
    const definition = { name, parent, slots, representation, keywordConstructor, classObject: null };
    if (!representation) {
      let cls = classes.get(name);
      if (cls && !cls.structure && !cls.forwardReferenced) fail('Structure name already denotes another class');
      cls ??= new LispClass(name); cls.structure = true; cls.forwardReferenced = false;
      cls.directSuperclasses = [parent?.classObject ?? classes.get('STRUCTURE-OBJECT')];
      cls.directSuperclasses[0].subclasses.add(cls); cls.cpl = precedence(cls);
      cls.directSlots = slots.filter(s => s.direct !== false).map(s => ({ ...s, allocation: 'INSTANCE', initargs: [rt.intern(s.name.name, 'KEYWORD')] }));
      cls.slots = new Map(slots.map(s => [s.name, s])); cls.structureDefinition = definition;
      classes.set(name, cls); definition.classObject = cls;
    }
    structures.set(name, definition); return definition;
  };
  const definitionFor = name => structures.get(name) ?? fail(`Unknown structure ${spelling(name)}`);
  const structureMatches = (object, definition) => {
    if (!definition.representation) return object instanceof StructureInstance && object.classObject.cpl.includes(definition.classObject);
    const r = definition.representation;
    if (r.kind === 'LIST') {
      let values; try { values = array(object); } catch { return false; }
      return values.length >= r.totalLength && r.prefixNames.every(([index, name]) => values[index] === name);
    }
    return object instanceof LispArray && object.dimensions.length === 1 && object.totalSize >= r.totalLength && r.prefixNames.every(([index, name]) => object.get(index) === name);
  };
  const expectStructure = (object, definition) => {
    if (!structureMatches(object, definition)) throw new LispCondition('TYPE-ERROR', `Expected structure ${spelling(definition.name)}`, { datum: object, expectedType: definition.name });
    return object;
  };
  const makeStructure = (definition, values) => {
    if (values.length !== definition.slots.length) fail('Wrong number of structure slot values');
    if (!definition.representation) return new StructureInstance(definition.classObject, values);
    const r = definition.representation;
    if (r.kind === 'VECTOR') {
      const object = new LispArray([r.totalLength], { elementType: r.elementType ?? T });
      for (const [index, name] of r.prefixNames) object.set(index, name);
      definition.slots.forEach((slot, i) => object.set(slot.offset, values[i]));
      return object;
    }
    const elements = Array(r.totalLength).fill(NIL);
    for (const [index, name] of r.prefixNames) elements[index] = name;
    definition.slots.forEach((slot, i) => { elements[slot.offset] = values[i]; });
    return fromArray(elements);
  };
  const readStructureSlot = (object, definition, index) => {
    expectStructure(object, definition); const slot = definition.slots[index]; if (!slot) fail('Structure slot index is out of bounds');
    if (!definition.representation) return object.values[index];
    if (definition.representation.kind === 'VECTOR') return object.get(slot.offset);
    let tail = object; for (let i = 0; i < slot.offset; i++) tail = tail.cdr; return tail.car;
  };
  const writeStructureSlot = (object, definition, index, value) => {
    expectStructure(object, definition); const slot = definition.slots[index]; if (!slot) fail('Structure slot index is out of bounds');
    const actualSlot = object instanceof StructureInstance ? object.classObject.structureDefinition.slots[index] : slot;
    if (actualSlot.readOnly) fail('Cannot change a read-only structure slot');
    if (!definition.representation) object.values[index] = value;
    else if (definition.representation.kind === 'VECTOR') object.set(slot.offset, value);
    else { let tail = object; for (let i = 0; i < slot.offset; i++) tail = tail.cdr; tail.car = value; }
    return value;
  };
  const copyStructure = (object, definition = null) => {
    if (definition) expectStructure(object, definition);
    if (object instanceof StructureInstance) return new StructureInstance(object.classObject, object.values);
    if (definition?.representation?.kind === 'LIST') return fromArray(array(object));
    if (definition?.representation?.kind === 'VECTOR') return new LispArray([object.totalSize], { initialContents: Array.from({ length: object.totalSize }, (_, i) => object.get(i)), elementType: object.elementType });
    throw new LispCondition('TYPE-ERROR', 'Expected a structure object', { datum: object });
  };
  Object.assign(rt, {
    structureDefinition: name => structures.get(name) ?? null,
    registerStructure,
    constructStructureSteps: function* (name, initargs) {
      const definition = definitionFor(name);
      if (definition.keywordConstructor === NIL) fail('Structure has no keyword constructor');
      return primary(yield rt.requestCall(definition.keywordConstructor, array(initargs)));
    },
  });
  define('%%REGISTER-STRUCTURE', args => {
    const slots = array(args[2]).map(raw => {
      const a = array(raw); return { name: a[0], initfunction: a[1], initform: a[2], type: a[3], readOnly: a[4] !== NIL, direct: a[5] !== NIL, offset: a[6] };
    });
    const r = args[3] === NIL ? null : array(args[3]);
    registerStructure(args[0], args[1], slots, r ? { kind: spelling(r[0]), elementType: r[1], totalLength: r[2], prefixNames: array(r[3]).map(array) } : null, args[4]);
    return args[0];
  });
  define('%%STRUCTURE-DEFAULT', (args, _rt, task) => {
    const slot = definitionFor(args[0]).slots[args[1]]; if (!slot) fail('Structure slot index is out of bounds');
    return rt.requestCall(slot.initfunction, []);
  });
  define('%%MAKE-STRUCTURE', args => makeStructure(definitionFor(args[0]), args.slice(1)));
  define('%%READ-STRUCTURE-SLOT', args => readStructureSlot(args[0], definitionFor(args[1]), args[2]));
  define('%%WRITE-STRUCTURE-SLOT', args => writeStructureSlot(args[0], definitionFor(args[1]), args[2], args[3]));
  define('%%STRUCTURE-PREDICATE', args => structureMatches(args[1], definitionFor(args[0])) ? T : NIL);
  define('%%COPY-STRUCTURE', args => copyStructure(args[1], definitionFor(args[0])));
  define('COPY-STRUCTURE', args => copyStructure(args[0]));
  installers.set(3, args => {
    if (args.length !== 5 && args.length !== 6) fail('Unsupported compiled structure definition');
    const slots = array(args[2]).map(raw => {
      const a = array(raw); if (a.length !== 6) fail('Invalid compiled structure slot record');
      return { name: a[0], initform: a[1], type: a[2], reader: a[3], readOnly: a[4] !== NIL, direct: a[5] !== NIL, offset: typeof a[5] === 'number' ? a[5] : undefined };
    });
    const r = args.length === 6 ? array(args[5]) : null;
    const definition = registerStructure(args[0], args[1], slots, r ? { kind: spelling(r[0]), elementType: r[1], totalLength: r[2], prefixNames: array(r[3]).map(array) } : null, args[4]);
    for (const [index, slot] of slots.entries()) if (slot.reader !== NIL && !slot.readOnly && !r) rt.defineFunction(fromArray([intern('SETF'), slot.reader]), {
      invoke: a => writeStructureSlot(a[1], definition, index, a[0]),
    });
    return args[0];
  });
  installers.set(4, handlers.get('%%MAKE-STRUCTURE'));
  installers.set(5, handlers.get('%%STRUCTURE-PREDICATE'));
  installers.set(6, handlers.get('%%COPY-STRUCTURE'));
  installers.set(7, handlers.get('%%READ-STRUCTURE-SLOT'));
  installers.set(8, handlers.get('%%WRITE-STRUCTURE-SLOT'));
  installers.set(9, args => {
    if (args.length !== 5) fail('Invalid compiled generic definition');
    return defineGeneric(args);
  });
  installers.set(10, args => {
    if (args.length !== 7) fail('Invalid compiled method definition');
    return compiledMethod(args[0], args[1], args[2], args[3], args[4], NIL, args[5], args[6]);
  });
  installers.set(11, function* (args, _rt, task) {
    if (args.length !== 9) fail('Invalid compiled class declaration');
    yield* ensureClassSteps(args[2], args[3], canonicalSlots(args[4]), { condition: args[0] !== NIL, defaultInitargs: canonicalDefaults(args[5]), metaclass: args[8] === NIL ? undefined : args[8], initialize: args[1] !== NIL }, task);
    return rt.multipleValues(NIL, NIL);
  });
  installers.set(13, args => {
    if (args.length !== 4) fail('Invalid shared slot initializer');
    const slot = findClass(args[0]).slots.get(args[1]);
    if (!slot || slot.allocation !== 'CLASS' || slot.cell !== args[2]) fail('Shared slot initializer does not belong to the class');
    if (slot.cell.value === UNBOUND) slot.cell.value = args[3];
    return rt.multipleValues();
  });
  const loadForm = (name, ...args) => fromArray([intern(name), ...args]);
  const quoteLoadForm = value => loadForm('QUOTE', value);
  define('MAKE-LOAD-FORM', args => {
    throw new LispCondition('ERROR', `No MAKE-LOAD-FORM method for instances of ${classOf(args[0]).name.name}`);
  });
  const structureSlotInitializer = rt.intern('%INITIALIZE-STRUCTURE-SLOT', mop);
  rt.defineFunction(structureSlotInitializer, { invoke: ([object, name, value]) => {
    if (!(object instanceof StructureInstance)) fail('Expected a structure instance');
    const index = object.classObject.structureDefinition.slots.findIndex(slot => slot.name === name);
    if (index < 0) fail('Unknown structure slot');
    object.values[index] = value;
    return value;
  } });
  define('MAKE-LOAD-FORM-SAVING-SLOTS', function* ([object, ...args], _rt, task) {
    const options = new Map(), slotNames = rt.intern('SLOT-NAMES', 'KEYWORD'), environment = rt.intern('ENVIRONMENT', 'KEYWORD');
    const allowOtherKeys = rt.intern('ALLOW-OTHER-KEYS', 'KEYWORD');
    if (args.length % 2) fail('Odd number of MAKE-LOAD-FORM-SAVING-SLOTS keyword arguments');
    for (let i = 0; i < args.length; i += 2) if (!options.has(args[i])) options.set(args[i], args[i + 1]);
    if ((options.get(allowOtherKeys) ?? NIL) === NIL)
      for (const key of options.keys()) if (key !== slotNames && key !== environment && key !== allowOtherKeys) fail('Unrecognized MAKE-LOAD-FORM-SAVING-SLOTS keyword');
    const cls = classOf(object), structure = object instanceof StructureInstance;
    if (!structure) yield* migrate(object, task);
    const names = options.has(slotNames) ? array(options.get(slotNames)) : structure
      ? cls.structureDefinition.slots.map(slot => slot.name)
      : [...cls.slots.values()].filter(slot => slot.allocation === 'INSTANCE').map(slot => slot.name);
    const target = quoteLoadForm(object), restore = [];
    for (const name of names) {
      const quoted = quoteLoadForm(name);
      const bound = primary(yield* slotOperation(object, name, 'bound', undefined, task));
      if (bound === NIL) restore.push(loadForm('SLOT-MAKUNBOUND', target, quoted));
      else {
        const value = quoteLoadForm(primary(yield* slotOperation(object, name, 'get', undefined, task)));
        restore.push(structure ? fromArray([structureSlotInitializer, target, quoted, value])
          : loadForm('SETF', loadForm('SLOT-VALUE', target, quoted), value));
      }
    }
    return rt.multipleValues(loadForm('ALLOCATE-INSTANCE', quoteLoadForm(cls)), restore.length ? loadForm('PROGN', ...restore) : NIL);
  });
  for (const name of ['ENSURE-CLASS', 'ENSURE-GENERIC-FUNCTION', 'CLASS-PRECEDENCE-LIST', 'CLASS-DIRECT-SUPERCLASSES', 'CLASS-DIRECT-SUBCLASSES', 'CLASS-FINALIZED-P', 'FINALIZE-INHERITANCE', 'GENERIC-FUNCTION-METHODS', 'METHOD-SPECIALIZERS']) {
    const symbol = rt.intern(name, mop); rt.exportSymbol(symbol, mop);
    rt.defineFunction(symbol, { invoke: handlers.get(name) });
  }
  const finishClassChange = args => args[0] ?? NIL;
  for (const [name, fn] of [['%FINISH-CLASS-CHANGE', finishClassChange], ['%INSTALL-ACCESSORS', args => findClass(args[0])]]) {
    rt.defineFunction(rt.intern(name, mop), { invoke: fn });
  }
  const loadMethod = args => {
    const [name, qualifiers, lambdaList, specializers, fast, publicEntry, documentation, inline, initargs] = args;
    if (initargs !== NIL) fail('Custom compiled method initialization is not implemented');
    return compiledMethod(name, qualifiers, specializers, lambdaList, fast, publicEntry, documentation, inline);
  };
  for (const [name, min, max, fn] of [
    ['%DEFGENERIC', 1, Infinity, args => ensureGenericFunction(args[0], genericOptions(args), false, true)],
    ['%LOAD-METHOD', 9, 9, loadMethod],
    ['METHOD-FUNCTION', 1, 1, args => methodFunction(args[0])],
  ]) {
    const symbol = rt.intern(name, mop);
    if (name === 'METHOD-FUNCTION') rt.exportSymbol(symbol, mop);
    rt.defineFunction(symbol, { invoke: args => {
      if (args.length < min || args.length > max) fail(`Wrong number of arguments to ${name}`);
      return fn(args);
    } });
  }
  // These standard generic functions retain their primary method when a user
  // adds before/after/around methods or invokes CALL-NEXT-METHOD.
  for (const [name, parameters, specs] of [
    ['MAKE-INSTANCE', ['CLASS', '&REST', 'INITARGS'], ['CLASS']],
    ['CHANGE-CLASS', ['INSTANCE', 'NEW-CLASS', '&REST', 'INITARGS'], ['STANDARD-OBJECT', 'CLASS']],
    ['ALLOCATE-INSTANCE', ['CLASS', '&REST', 'INITARGS'], ['CLASS']],
    ['INITIALIZE-INSTANCE', ['OBJECT', '&REST', 'INITARGS'], ['STANDARD-OBJECT']],
    ['REINITIALIZE-INSTANCE', ['OBJECT', '&REST', 'INITARGS'], ['STANDARD-OBJECT']],
    ['SHARED-INITIALIZE', ['OBJECT', 'SLOTS', '&REST', 'INITARGS'], ['STANDARD-OBJECT', 'T']],
    ['UPDATE-INSTANCE-FOR-REDEFINED-CLASS', ['OBJECT', 'ADDED', 'DISCARDED', 'VALUES', '&REST', 'INITARGS'], ['STANDARD-OBJECT', 'T', 'T', 'T']],
    ['UPDATE-INSTANCE-FOR-DIFFERENT-CLASS', ['OLD', 'NEW', '&REST', 'INITARGS'], ['STANDARD-OBJECT', 'STANDARD-OBJECT']],
    ['NO-APPLICABLE-METHOD', ['GENERIC', '&REST', 'ARGS'], ['T']],
    ['NO-NEXT-METHOD', ['GENERIC', 'METHOD', '&REST', 'ARGS'], ['T', 'T']],
    ['SLOT-MISSING', ['CLASS', 'OBJECT', 'NAME', 'OPERATION', '&OPTIONAL', 'VALUE'], ['T', 'T', 'T', 'T']],
    ['SLOT-UNBOUND', ['CLASS', 'OBJECT', 'NAME'], ['T', 'T', 'T']],
    ['PRINT-OBJECT', ['OBJECT', 'STREAM'], ['T', 'T']],
  ]) {
    const lambdaList = fromArray(parameters.map(intern));
    const gf = ensureGenericFunction(intern(name), { lambdaList }, true);
    ensureMethod(gf, { lambdaList, specializers: specs.map(s => classes.get(s)) }, { invoke: handlers.get(name) });
  }
  const makeInstance = generics.get(intern('MAKE-INSTANCE'));
  ensureMethod(makeInstance, { lambdaList: makeInstance.lambdaList, specializers: [classes.get('SYMBOL')] }, { invoke: handlers.get('MAKE-INSTANCE') });
  const changeClass = generics.get(intern('CHANGE-CLASS'));
  ensureMethod(changeClass, { lambdaList: changeClass.lambdaList, specializers: [classes.get('T'), classes.get('SYMBOL')] }, { invoke: handlers.get('CHANGE-CLASS') });
  const loadFormLambdaList = fromArray([rt.intern('OBJECT'), intern('&OPTIONAL'), rt.intern('ENVIRONMENT')]);
  const loadFormGeneric = ensureGenericFunction(intern('MAKE-LOAD-FORM'), { lambdaList: loadFormLambdaList }, true);
  for (const name of ['STANDARD-OBJECT', 'STRUCTURE-OBJECT', 'CONDITION'])
    ensureMethod(loadFormGeneric, { lambdaList: loadFormLambdaList, specializers: [classes.get(name)] }, { invoke: handlers.get('MAKE-LOAD-FORM') });
  ensureMethod(loadFormGeneric, { lambdaList: loadFormLambdaList, specializers: [classes.get('CLASS')] }, { invoke([cls]) {
    if (findClass(cls.name, false) !== cls) throw new LispCondition('ERROR', `Class ${cls.name.name} has no proper name`);
    return loadForm('FIND-CLASS', quoteLoadForm(cls.name));
  } });
  const printObject = generics.get(intern('PRINT-OBJECT'));
  rt.usesPrintObject = value => value instanceof LispCondition || value instanceof StandardInstance || value instanceof LispRestart ||
    value instanceof StructureInstance && printObject.methods.length > 1;
  return rt;
}

export function expandClosForm(rt, form, inline = false) {
  if (!(form instanceof Cons)) return undefined;
  const forms = array(form), name = spelling(forms[0]);
  if (name === 'DEFINE-METHOD-COMBINATION') return expandMethodCombination(rt, forms);
  if (!['DEFCLASS', 'DEFINE-CONDITION', 'DEFGENERIC', 'DEFMETHOD', 'DEFSTRUCT', 'WITH-SLOTS', 'WITH-ACCESSORS'].includes(name)) return undefined;
  const sym = name => rt.intern(name, 'COMMON-LISP'), quote = x => fromArray([sym('QUOTE'), x]);
  const invoke = (name, ...args) => fromArray([sym(name), ...args]);
  const thunk = body => invoke('FUNCTION', invoke('LAMBDA', NIL, body));
  if (name === 'DEFSTRUCT') {
    const header = forms[1] instanceof LispSymbol ? [forms[1]] : array(forms[1]), structureName = header[0];
    if (!(structureName instanceof LispSymbol)) fail('DEFSTRUCT requires a symbol name');
    let prefix = `${structureName.name}-`, copier = rt.intern(`COPY-${structureName.name}`), predicate = rt.intern(`${structureName.name}-P`);
    let included = null, includeOverrides = [], kind = null, elementType = T, named = false, initialOffset = 0;
    const constructors = [], seen = new Set(); let constructorsSpecified = false;
    for (const option of header.slice(1)) {
      const a = option instanceof LispSymbol ? [option] : array(option), key = spelling(a[0]), args = a.slice(1);
      if (key !== 'CONSTRUCTOR' && seen.has(key)) fail('Duplicate DEFSTRUCT option'); seen.add(key);
      if (key === 'CONC-NAME') { if (args.length !== 1) fail('Malformed :CONC-NAME'); prefix = args[0] === NIL ? '' : args[0] instanceof LispString ? args[0].toString() : spelling(args[0]); }
      else if (key === 'COPIER') { if (args.length !== 1) fail('Malformed :COPIER'); copier = args[0]; }
      else if (key === 'PREDICATE') { if (args.length !== 1) fail('Malformed :PREDICATE'); predicate = args[0]; }
      else if (key === 'CONSTRUCTOR') {
        constructorsSpecified = true;
        if (args.length > 2) fail('Malformed :CONSTRUCTOR');
        if (args[0] !== NIL) constructors.push({ name: args[0] ?? rt.intern(`MAKE-${structureName.name}`), parameters: args.length === 2 ? args[1] : null });
      } else if (key === 'INCLUDE') { if (!args.length) fail('Malformed :INCLUDE'); included = args[0]; includeOverrides = args.slice(1); }
      else if (key === 'TYPE') {
        if (args.length !== 1) fail('Malformed :TYPE');
        const r = args[0] instanceof Cons ? array(args[0]) : [args[0]]; kind = spelling(r[0]);
        if (!['LIST', 'VECTOR'].includes(kind) || r.length > (kind === 'LIST' ? 1 : 2)) fail('DEFSTRUCT :TYPE requires LIST or VECTOR');
        elementType = r[1] ?? T;
      } else if (key === 'NAMED') { if (args.length) fail('Malformed :NAMED'); named = true; }
      else if (key === 'INITIAL-OFFSET') { if (args.length !== 1 || !Number.isInteger(args[0]) || args[0] < 0) fail('Invalid :INITIAL-OFFSET'); initialOffset = args[0]; }
      else if (key === 'PRINT-FUNCTION' || key === 'PRINT-OBJECT') { if (args.length) fail('Custom structure printing is not implemented'); }
      else fail(`Unsupported DEFSTRUCT option ${key}`);
    }
    if (!constructorsSpecified) constructors.push({ name: rt.intern(`MAKE-${structureName.name}`), parameters: null });
    if (initialOffset && !kind) fail(':INITIAL-OFFSET requires :TYPE');
    if (kind && !named) { if (seen.has('PREDICATE') && predicate !== NIL) fail('Unnamed typed structures cannot have predicates'); predicate = NIL; }
    const parseSlot = raw => {
      const a = raw instanceof LispSymbol ? [raw] : array(raw);
      if (!(a[0] instanceof LispSymbol)) fail('Structure slot name must be a symbol');
      const slot = { name: a[0], initform: a[1] ?? NIL, type: T, readOnly: false, direct: true };
      const options = new Set();
      if (a.length > 2 && a.length % 2) fail('Malformed structure slot options');
      for (let i = 2; i < a.length; i += 2) {
        const key = spelling(a[i]); if (options.has(key)) fail('Duplicate structure slot option'); options.add(key);
        if (key === 'TYPE') slot.type = a[i + 1];
        else if (key === 'READ-ONLY') slot.readOnly = a[i + 1] !== NIL;
        else fail(`Unknown structure slot option ${key}`);
      }
      slot.specified = options; return slot;
    };
    let slots = [], prefixNames = [], offset = initialOffset;
    if (included) {
      const parent = rt.structureSourceDefinitions.get(included) ?? rt.structureDefinition(included);
      if (!parent) fail('Unknown included structure');
      const r = parent.representation;
      if ((r?.kind ?? null) !== kind) fail('Included structure has a different representation');
      slots = parent.slots.map(s => ({ ...s, direct: false }));
      if (r) { prefixNames = [...r.prefixNames]; offset += r.totalLength; }
      for (const raw of includeOverrides) {
        const replacement = parseSlot(raw), index = slots.findIndex(s => s.name.name === replacement.name.name);
        if (index < 0) fail('Unknown included slot override');
        const old = slots[index];
        if (old.readOnly && replacement.specified.has('READ-ONLY') && !replacement.readOnly) fail('Cannot make an inherited read-only slot writable');
        slots[index] = { ...old, initform: replacement.initform,
          type: replacement.specified.has('TYPE') ? replacement.type : old.type,
          readOnly: replacement.specified.has('READ-ONLY') ? replacement.readOnly : old.readOnly };
      }
    }
    if (kind && named) prefixNames.push([offset++, structureName]);
    let body = forms.slice(2); if (body[0] instanceof LispString) body = body.slice(1);
    for (const raw of body) {
      const slot = parseSlot(raw);
      if (slots.some(s => s.name.name === slot.name.name)) fail('Duplicate structure slot');
      slot.offset = kind ? offset++ : slots.length; slots.push(slot);
    }
    const representation = kind ? { kind, elementType, prefixNames, totalLength: offset } : null;
    rt.structureSourceDefinitions.set(structureName, { slots, representation });
    const keywordConstructor = constructors.find(c => c.parameters === null)?.name ?? NIL;
    const records = slots.map(s => invoke('LIST', quote(s.name), thunk(s.initform), quote(s.initform), quote(s.type), quote(s.readOnly ? T : NIL), quote(s.direct ? T : NIL), s.offset));
    const rep = representation ? fromArray([sym(kind), elementType, offset, fromArray(prefixNames.map(pair => fromArray(pair)))]) : NIL;
    const definitions = [invoke('%%REGISTER-STRUCTURE', quote(structureName), quote(included ?? NIL), invoke('LIST', ...records), quote(rep), quote(keywordConstructor))];
    const defun = (name, parameters, expression) => invoke('DEFUN', name, fromArray(parameters), expression);
    const defaultFor = index => invoke('%%STRUCTURE-DEFAULT', quote(structureName), index);
    for (const constructor of constructors) {
      let parameters = [], values = [];
      if (constructor.parameters === null) {
        parameters = [sym('&KEY'), ...slots.map((s, i) => fromArray([s.name, defaultFor(i)]))];
        values = slots.map(s => s.name);
      } else {
        const variables = new Map(); let mode = 'required';
        for (const parameter of array(constructor.parameters)) {
          if (parameter instanceof LispSymbol && spelling(parameter).startsWith('&')) { mode = spelling(parameter); parameters.push(parameter); continue; }
          const p = parameter instanceof Cons ? array(parameter) : [parameter];
          const variable = mode === '&KEY' && p[0] instanceof Cons ? array(p[0])[1] : p[0];
          if (!(variable instanceof LispSymbol)) fail('Invalid BOA parameter');
          variables.set(variable.name, variable); if (p[2] instanceof LispSymbol) variables.set(p[2].name, p[2]);
          const index = slots.findIndex(s => s.name.name === variable.name);
          parameters.push(p.length === 1 && index >= 0 && ['&OPTIONAL', '&KEY'].includes(mode) ? fromArray([p[0], defaultFor(index)]) : parameter);
        }
        values = slots.map((s, i) => variables.get(s.name.name) ?? defaultFor(i));
      }
      definitions.push(defun(constructor.name, parameters, invoke('%%MAKE-STRUCTURE', quote(structureName), ...values)));
    }
    for (const [index, slot] of slots.entries()) {
      const reader = rt.intern(`${prefix}${slot.name.name}`), object = new LispSymbol('STRUCTURE'), value = new LispSymbol('VALUE');
      definitions.push(defun(reader, [object], invoke('%%READ-STRUCTURE-SLOT', object, quote(structureName), index)));
      if (!slot.readOnly) definitions.push(defun(fromArray([sym('SETF'), reader]), [value, object], invoke('%%WRITE-STRUCTURE-SLOT', object, quote(structureName), index, value)));
    }
    const object = new LispSymbol('STRUCTURE');
    if (predicate !== NIL) definitions.push(defun(predicate, [object], invoke('%%STRUCTURE-PREDICATE', quote(structureName), object)));
    if (copier !== NIL) definitions.push(defun(copier, [object], invoke('%%COPY-STRUCTURE', quote(structureName), object)));
    return invoke('PROGN', ...definitions, quote(structureName));
  }
  if (name === 'WITH-SLOTS' || name === 'WITH-ACCESSORS') {
    if (forms.length < 3) fail(`${name} requires bindings and an instance form`);
    const object = new LispSymbol('INSTANCE'), bindings = array(forms[1]).map(binding => {
      if (name === 'WITH-ACCESSORS' && !(binding instanceof Cons)) fail('Invalid WITH-ACCESSORS binding');
      const fields = binding instanceof LispSymbol ? [binding, binding] : array(binding);
      if (fields.length !== 2) fail(`Invalid ${name} binding`);
      const [variable, slot] = fields;
      if (!(variable instanceof LispSymbol) || !(slot instanceof LispSymbol)) fail(`Invalid ${name} binding`);
      return fromArray([variable, name === 'WITH-SLOTS' ? invoke('SLOT-VALUE', object, quote(slot)) : fromArray([slot, object])]);
    });
    return invoke('LET', fromArray([fromArray([object, forms[2]])]), invoke('SYMBOL-MACROLET', fromArray(bindings), ...forms.slice(3)));
  }
  if (name === 'DEFCLASS' || name === 'DEFINE-CONDITION') {
    if (forms.length < 4) fail(`${name} requires a name, superclasses, and slots`);
    const condition = name === 'DEFINE-CONDITION';
    const slots = [], accessors = [], defaults = []; let metaclass = NIL, report = NIL;
    for (const raw of array(forms[3])) {
      const spec = raw instanceof LispSymbol ? [raw] : array(raw); const slot = spec[0];
      const initargs = []; let init = NIL, hasInit = NIL, allocation = rt.intern('INSTANCE', 'KEYWORD');
      for (let i = 1; i < spec.length; i += 2) {
        const option = spelling(spec[i]), value = spec[i + 1]; if (value === undefined) fail('Missing slot option value');
        if (option === 'INITARG') initargs.push(value);
        else if (option === 'INITFORM') { hasInit = T; init = thunk(value); }
        else if (option === 'ALLOCATION') allocation = value;
        else if (option === 'READER' || option === 'WRITER' || option === 'ACCESSOR') {
          accessors.push(fromArray([slot, option === 'WRITER' ? NIL : value, option === 'READER' ? NIL : option === 'ACCESSOR' ? fromArray([sym('SETF'), value]) : value]));
        } else if (!['TYPE', 'DOCUMENTATION'].includes(option)) fail(`Unknown slot option ${option}`);
      }
      slots.push(invoke('LIST', quote(slot), quote(fromArray(initargs)), quote(hasInit), init, quote(allocation)));
    }
    const seenOptions = new Set();
    for (const option of forms.slice(4)) {
      const a = array(option), key = spelling(a[0]);
      if (seenOptions.has(key)) fail(`Duplicate class option :${key}`);
      seenOptions.add(key);
      if (key === 'DEFAULT-INITARGS') { if (a.length % 2 !== 1) fail('Malformed default initargs'); for (let i = 1; i < a.length; i += 2) defaults.push(quote(a[i]), thunk(a[i + 1])); }
      else if (key === 'METACLASS' && !condition && a.length === 2) metaclass = a[1];
      else if (key === 'REPORT' && condition && a.length === 2) report = a[1];
      else if (key !== 'DOCUMENTATION') fail(`Unsupported class option ${key}`);
    }
    const define = flag => invoke(condition ? '%%DEFINE-CONDITION' : '%%DEFINE-CLASS', quote(forms[1]), quote(forms[2]), invoke('LIST', ...slots), invoke('LIST', ...defaults), quote(flag), quote(fromArray(accessors)));
    if (!condition) return define(metaclass);
    const situation = (...flags) => fromArray(flags.map(key => rt.intern(key, 'KEYWORD')));
    const install = [define(T)];
    if (report !== NIL) {
      const object = new LispSymbol('CONDITION'), stream = new LispSymbol('STREAM'), fn = new LispSymbol('REPORT');
      const parameters = fromArray([fromArray([object, forms[1]]), stream]);
      const emit = report instanceof LispString ? invoke('WRITE-STRING', report, stream) : invoke('FUNCALL', fn, object, stream);
      const method = invoke('DEFMETHOD', sym('PRINT-OBJECT'), parameters,
        invoke('IF', invoke('OR', sym('*PRINT-ESCAPE*'), sym('*PRINT-READABLY*')), invoke('CALL-NEXT-METHOD'), emit));
      install.push(report instanceof LispString ? method : invoke('LET', fromArray([fromArray([fn, invoke('FUNCTION', report)])]), method));
    }
    return invoke('PROGN', invoke('EVAL-WHEN', situation('COMPILE-TOPLEVEL'), define(NIL)),
      invoke('EVAL-WHEN', situation('LOAD-TOPLEVEL', 'EXECUTE'), ...install, quote(forms[1])));
  }
  if (name === 'DEFGENERIC') {
    let order = NIL, combination = fromArray([sym('STANDARD')]), documentation = NIL;
    const methods = [], seen = new Set();
    for (const option of forms.slice(3)) { const a = array(option), key = spelling(a[0]);
      if (key !== 'METHOD' && key !== 'DECLARE' && seen.has(key)) fail(`Duplicate generic option ${key}`);
      seen.add(key);
      if (key === 'ARGUMENT-PRECEDENCE-ORDER') order = fromArray(a.slice(1));
      else if (key === 'METHOD') methods.push(expandClosForm(rt, fromArray([sym('DEFMETHOD'), forms[1], ...a.slice(1)]), true));
      else if (key === 'METHOD-COMBINATION') combination = fromArray(a.slice(1));
      else if (key === 'DOCUMENTATION') {
        if (a.length !== 2 || !(a[1] instanceof LispString)) fail('DEFGENERIC :DOCUMENTATION requires a string');
        documentation = a[1];
      }
      else if (key === 'GENERIC-FUNCTION-CLASS' && spelling(a[1]) !== 'STANDARD-GENERIC-FUNCTION') fail('Custom generic function classes are not implemented');
      else if (key === 'METHOD-CLASS' && spelling(a[1]) !== 'STANDARD-METHOD') fail('Custom method classes are not implemented');
      else if (!['DOCUMENTATION', 'DECLARE', 'METHOD-COMBINATION', 'GENERIC-FUNCTION-CLASS', 'METHOD-CLASS'].includes(key)) fail(`Unsupported generic option ${key}`);
    }
    const definition = invoke('%%DEFINE-GENERIC', quote(forms[1]), quote(forms[2]), quote(order), quote(combination), quote(documentation));
    return methods.length ? invoke('PROGN', definition, ...methods, invoke('FUNCTION', forms[1])) : definition;
  }
  const qualifiers = []; let cursor = 2;
  while (forms[cursor] !== undefined && forms[cursor] !== NIL && !(forms[cursor] instanceof Cons)) qualifiers.push(forms[cursor++]);
  if (forms[cursor] === undefined) fail('DEFMETHOD requires a specialized lambda list');
  const params = array(forms[cursor++]), lambda = [], specializers = []; let required = true;
  for (const param of params) {
    if (param instanceof LispSymbol && spelling(param).startsWith('&')) required = false;
    if (required) {
      if (param instanceof Cons) {
        const [variable, spec] = array(param); lambda.push(variable);
        specializers.push(spec instanceof Cons && spelling(spec.car) === 'EQL' ? invoke('CONS', quote(sym('EQL')), array(spec)[1]) : quote(spec));
      } else { lambda.push(param); specializers.push(quote(sym('T'))); }
    } else lambda.push(param);
  }
  const implementationLambda = [...lambda];
  if (lambda.some(p => spelling(p) === '&KEY') && !lambda.some(p => spelling(p) === '&ALLOW-OTHER-KEYS')) {
    const aux = implementationLambda.findIndex(p => spelling(p) === '&AUX');
    implementationLambda.splice(aux < 0 ? implementationLambda.length : aux, 0, sym('&ALLOW-OTHER-KEYS'));
  }
  const next = new LispSymbol('NEXT-METHOD'), nextp = new LispSymbol('NEXT-METHOD-P'), forwarded = new LispSymbol('FORWARDED');
  const localFunctions = fromArray([
    fromArray([sym('CALL-NEXT-METHOD'), fromArray([sym('&REST'), forwarded]), invoke('APPLY', next, forwarded)]),
    fromArray([sym('NEXT-METHOD-P'), NIL, nextp]),
  ]);
  const body = forms.slice(cursor);
  // Method declarations have lambda scope; the implicit block encloses only
  // executable forms, so RETURN-FROM and lexical next-method closures agree.
  if (body[0] instanceof LispString) body.shift();
  const declarations = [];
  while (body[0] instanceof Cons && spelling(body[0].car) === 'DECLARE') declarations.push(body.shift());
  const block = forms[1] instanceof Cons ? array(forms[1])[1] : forms[1];
  const implementation = fromArray([sym('LAMBDA'), fromArray([next, nextp, ...implementationLambda]), ...declarations,
    invoke('BLOCK', block, invoke('FLET', localFunctions, ...body))]);
  return invoke('%%DEFINE-METHOD', quote(forms[1]), quote(fromArray(qualifiers)), invoke('LIST', ...specializers), quote(fromArray(lambda)), invoke('FUNCTION', implementation), inline ? T : NIL);
}
