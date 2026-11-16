import { NIL, T, UNBOUND, LispSymbol, Cons, LispString, LispArray, LispCharacter, LispPathname, Closure, LispCode, fromArray, toArray, eql, equal } from './objects.mjs';
import { Ratio, Complex, SingleFloat, DoubleFloat, RandomState, isNumber, makeComplex } from './numbers.mjs';
import { LispCondition, LispRestart, MultipleValues } from './execution.mjs';
import { LispHashTable } from './hash_tables.mjs';
import { installMethodCombinations } from './method_combination.mjs';
import { expandClosForm, splitSpecializedLambdaList } from './clos_macros.mjs';
import { conditionClassResolver } from './condition_types.mjs';
import { LispClass, StandardInstance, SlotDefinition, ClassPrecedenceError, initializeStorage, attachStorage, hasStandardStorage, classPrecedence, collectDefaultInitargs, groupDirectSlots, effectiveSlotFields, assignSlotLocations, slotLayout, rawSlotValue, writeRawSlot } from './clos_layout.mjs';

export { LispClass, StandardInstance };
class Specializer {
  constructor() { this.directMethods = new Set(); }
}
export class EqlSpecializer extends Specializer {
  constructor(object = UNBOUND) { super(); this.object = object; }
}
export class StructureInstance {
  constructor(classObject, values) { this.kind = 'structure-instance'; this.classObject = classObject; this.values = [...values]; }
}
class FuncallableInstance extends StandardInstance {
  invoke(args, rt) {
    if (!this.target) throw new LispCondition('UNDEFINED-FUNCTION', 'Funcallable instance has no function', { datum: this });
    return rt.requestCall(this.target, args);
  }
}
class MethodCombination {
  constructor(spec, classObject) {
    this.spec = spec;
    initializeStorage(this, classObject);
  }
}
export class Method {
  constructor({ specializers = [], qualifiers = [], fn, lambdaList = NIL, publicEntry = NIL, documentation = NIL, inline = false, slotDefinition = NIL } = {}) {
    if (qualifiers.some(q => q === NIL || q instanceof Cons)) fail('Method qualifiers must be non-NIL atoms');
    this.specializers = specializers; this.qualifiers = [...qualifiers];
    this.fn = fn; this.lambdaList = lambdaList; this.callSignature = signature(lambdaList); this.genericFunction = null;
    this.publicEntry = publicEntry; this.documentation = documentation; this.inline = inline;
    this.slotDefinition = slotDefinition;
  }
}
const spelling = x => x instanceof LispSymbol ? x.name : String(x).replace(/^:/, '').toUpperCase();
const array = x => Array.isArray(x) ? x : toArray(x);
const primary = x => x instanceof MultipleValues ? (x.values[0] ?? NIL) : x;
const callable = value => value instanceof Closure || value instanceof LispCode || typeof value?.invoke === 'function';
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
function standardMethodGroups(rt, applicable) {
  const around = [], before = [], after = [], primary = [];
  const modes = new Map([['AROUND', around], ['BEFORE', before], ['AFTER', after]].map(([name, methods]) => [rt.intern(name, 'KEYWORD'), methods]));
  for (const method of applicable) {
    const qualifiers = method.qualifiers;
    const group = qualifiers.length === 0 ? primary : qualifiers.length === 1 && modes.get(qualifiers[0]);
    if (!group) fail('Invalid qualifier for STANDARD method combination');
    group.push(method);
  }
  after.reverse();
  return { around, before, after, primary };
}
const standardSpecializer = spec => spec instanceof LispClass || spec instanceof EqlSpecializer;
export class GenericFunction {
  constructor(name = NIL, { lambdaList = NIL, argumentPrecedenceOrder = null, documentation = NIL, methodCombination = null, declarations = NIL } = {}) {
    this.name = name; this.lambdaList = lambdaList; this.required = requiredParameters(lambdaList);
    this.callSignature = signature(lambdaList);
    this.argumentPrecedenceOrder = validateArgumentOrder(argumentPrecedenceOrder, this.required.length);
    this.methods = []; this.documentation = documentation; this.methodCombination = methodCombination;
    this.declarations = declarations;
    this.dependents = new Set();
    this.revision = 0;
  }
  checkArity(args) {
    const sig = this.callSignature;
    if (args.length < sig.required || (!sig.rest && !sig.key && args.length > sig.required + sig.optional))
      fail('Wrong number of arguments to generic function');
  }
  applicable(args, rt, task) {
    this.checkArity(args);
    return this.orderedMethods(args, rt, task);
  }
  orderedMethods(args, rt, task) {
    if (this.required.length === 1) {
      if (this.classDispatch === undefined) this.classDispatch = this.methods.every(method => method.specializers[0] instanceof LispClass) ? new WeakMap() : null;
      if (this.classDispatch) {
        const cls = rt.classOf(args[0], task), cpl = rt.closPrecedence(cls, task);
        let methods = this.classDispatch.get(cpl);
        if (!methods) {
          methods = this.rankMethods([cpl], () => false, false);
          this.classDispatch.set(cpl, methods);
        }
        return methods;
      }
    }
    const classes = this.required.map((_, i) => rt.classOf(args[i], task));
    return this.rankMethods(classes.map(cls => rt.closPrecedence(cls, task)), (specializer, i) => eql(specializer.object, args[i]), false);
  }
  rankMethods(cpls, matchesEql, classOnly) {
    const matches = [];
    for (const method of this.methods) {
      if (!method.specializers.every(standardSpecializer)) fail('Custom specializers require custom method applicability');
      const ranks = [];
      let applicable = true, hasEql = false;
      for (const i of this.argumentPrecedenceOrder) {
        const spec = method.specializers[i];
        if (spec instanceof EqlSpecializer) {
          if (!matchesEql(spec, i)) { applicable = false; break; }
          hasEql = true;
          ranks.push(-1);
        } else {
          const rank = cpls[i].indexOf(spec);
          if (rank < 0) { applicable = false; break; }
          ranks.push(rank);
        }
      }
      if (applicable) {
        if (classOnly && hasEql) return null;
        matches.push({ method, ranks });
      }
    }
    matches.sort((a, b) => {
      for (let i = 0; i < a.ranks.length; i++) if (a.ranks[i] !== b.ranks[i]) return a.ranks[i] - b.ranks[i];
      return 0;
    });
    return matches.map(match => match.method);
  }
  *invoke(args, rt, task) {
    if (this.target) return yield rt.requestCall(this.target, args);
    return yield* this.invokeDefault(args, rt, task);
  }
  nextArguments(applicable, rt) {
    const generic = this;
    return function* (actual, task) {
      generic.checkArity(actual);
      const changed = yield* rt.selectClosMethods(generic, actual, task);
      if (changed.length !== applicable.length || changed.some((m, i) => m !== applicable[i]))
        fail('CALL-NEXT-METHOD arguments change the ordered applicable methods');
      generic.checkKeywords(actual, changed, rt);
      return actual;
    };
  }
  *invokeDefault(args, rt, task) {
    this.checkArity(args);
    const applicable = yield* rt.selectClosMethods(this, args, task);
    if (!applicable.length) return yield rt.requestCall(rt.intern('NO-APPLICABLE-METHOD', 'COMMON-LISP'), [this, ...args]);
    this.checkKeywords(args, applicable, rt);
    const generic = this, validateNextArgs = this.nextArguments(applicable, rt);
    const effective = yield* rt.customEffectiveMethod(this, applicable, task);
    if (effective) return yield* rt.methodCombinations.invokeForm(primary(effective), effective.values?.[1] ?? NIL, this, args, validateNextArgs);
    if (this.methodCombination !== null) return yield* rt.methodCombinations.invoke(this.methodCombination, this, applicable, args, validateNextArgs);
    let groups = this.groupedApplicable === applicable ? this.methodGroups : undefined;
    if (!groups) {
      groups = standardMethodGroups(rt, applicable);
      if (this.classDispatch) { this.groupedApplicable = applicable; this.methodGroups = groups; }
    }
    const { around, before, after, primary: primaries } = groups;
    if (!primaries.length) fail('No primary method');
    const core = function* (actual, _rt, activeTask) {
      for (const method of before) yield* invokeMethod(rt, activeTask, method, actual, null, undefined, generic, true);
      const saved = yield* invokeMethodChain(rt, activeTask, generic, primaries, actual, validateNextArgs);
      for (const method of after) yield* invokeMethod(rt, activeTask, method, actual, null, undefined, generic, true);
      return saved;
    };
    if (!around.length) return yield* core(args, rt, task);
    return yield* invokeMethodChain(rt, task, generic, [...around, new Method({ fn: { invoke: core } })], args, validateNextArgs);
  }
  checkKeywords(args, applicable, rt) {
    const sig = this.callSignature, start = sig.required + sig.optional;
    if (args.length <= start) return;
    const methods = applicable.map(method => method.callSignature);
    if (!sig.key && !methods.some(method => method.key)) return;
    if ((args.length - start) % 2) fail('Odd number of generic keyword arguments');
    if (!sig.key) return;
    const keys = new Set([...sig.keys, ...methods.flatMap(s => s.keys)].map(key => typeof key === 'string' ? rt.intern(key, 'KEYWORD') : key));
    const override = rt.intern('ALLOW-OTHER-KEYS', 'KEYWORD');
    let explicitAllow, unknown;
    for (let i = start; i < args.length; i += 2) {
      const key = args[i];
      if (!(key instanceof LispSymbol)) fail('Generic keyword argument names must be symbols');
      if (key === override) explicitAllow ??= args[i + 1] !== NIL;
      else if (!keys.has(key)) unknown ??= key;
    }
    if (unknown !== undefined && !explicitAllow && !sig.allowOtherKeys && !methods.some(method => method.allowOtherKeys))
      fail(`Unrecognized generic keyword argument ${spelling(unknown)}`);
  }
}
function* invokeMethodChain(rt, task, generic, methods, args, validateNextArgs, explicitWithoutNext = false, extra = []) {
  const chain = function* (index, actual, activeTask) {
    const more = index + 1 < methods.length;
    const next = more ? function* (changed, nextTask) {
      return yield* chain(index + 1, changed === null ? actual : yield* validateNextArgs(changed, nextTask), nextTask);
    } : null;
    return yield* invokeMethod(rt, activeTask, methods[index], actual, next, () => methods.slice(index + 1), generic,
      explicitWithoutNext && index === 0, index === 0 ? extra : [], validateNextArgs);
  };
  return yield* chain(0, args, task);
}
function* invokeMethod(rt, task, method, args, next, nextMethods = () => [], generic = method.genericFunction,
    explicitWithoutNext = false, extra = [], validateNextArgs = null) {
  const stack = task.closMethodContexts ?? (task.closMethodContexts = []);
  stack.push({ next, args, generic, method, explicitWithoutNext, validateNextArgs, get methodArgs() { return [fromArray(nextMethods()), ...extra]; } });
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
  const intern = name => rt.intern(name, 'COMMON-LISP');
  const keyword = name => rt.intern(name, 'KEYWORD');
  const mop = rt.findPackage('ZK-MOP') ?? rt.makePackage('ZK-MOP', { use: ['COMMON-LISP'] });
  const mopSymbol = name => rt.exportSymbol(rt.intern(name, mop), mop);
  const setter = name => fromArray([intern('SETF'), name]);
  const classUpdates = new WeakMap(), classPromotions = new WeakMap(), subclassLinks = new WeakMap();
  const inTask = (table, task) => {
    let entries = table.get(task);
    if (!entries) { entries = new Map(); table.set(task, entries); }
    return entries;
  };
  const definition = (cls, task = rt.activeTask) => classUpdates.get(task)?.get(cls) ?? cls;
  const instanceRecord = (object, task) => classPromotions.get(task)?.get(object) ?? object;
  const instanceDefinition = (record, task) => {
    const state = definition(record.classObject, task);
    // A finalizer may inspect a newly allocated prototype using the draft
    // layout. Existing instances retain their published layout until commit.
    return state.layoutReady && state.slots === record.storage.layout ? state : record.classObject;
  };
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
  const builtin = (name, supers = [], packageName = 'COMMON-LISP') => {
    const cls = new LispClass(rt.exportSymbol(rt.intern(name, packageName), packageName), { builtin: true });
    cls.directSuperclasses = supers.map(s => classes.get(s));
    cls.cpl = classPrecedence(cls);
    cls.finalized = true;
    for (const parent of cls.directSuperclasses) {
      parent.children.add(cls);
      parent.subclasses.add(cls);
    }
    classes.set(name, cls); classes.set(cls.name, cls); return cls;
  };
  const mopClass = (name, ...supers) => builtin(name, supers, mop);
  builtin('T'); builtin('STANDARD-OBJECT', ['T']);
  mopClass('METAOBJECT', 'STANDARD-OBJECT'); mopClass('SPECIALIZER', 'METAOBJECT');
  builtin('CLASS', ['SPECIALIZER']);
  builtin('STANDARD-CLASS', ['CLASS']); builtin('BUILT-IN-CLASS', ['CLASS']);
  mopClass('FUNCALLABLE-STANDARD-CLASS', 'CLASS');
  mopClass('FORWARD-REFERENCED-CLASS', 'CLASS');
  builtin('STRUCTURE-CLASS', ['CLASS']); builtin('STRUCTURE-OBJECT', ['T']);
  builtin('FUNCTION', ['T']); mopClass('FUNCALLABLE-STANDARD-OBJECT', 'FUNCTION', 'STANDARD-OBJECT');
  builtin('GENERIC-FUNCTION', ['METAOBJECT', 'FUNCALLABLE-STANDARD-OBJECT']);
  builtin('STANDARD-GENERIC-FUNCTION', ['GENERIC-FUNCTION']); builtin('METHOD', ['METAOBJECT']);
  builtin('STANDARD-METHOD', ['METHOD']); builtin('SYMBOL', ['T']); builtin('SEQUENCE', ['T']); builtin('LIST', ['SEQUENCE']);
  mopClass('STANDARD-ACCESSOR-METHOD', 'STANDARD-METHOD');
  mopClass('STANDARD-READER-METHOD', 'STANDARD-ACCESSOR-METHOD');
  mopClass('STANDARD-WRITER-METHOD', 'STANDARD-ACCESSOR-METHOD');
  mopClass('EQL-SPECIALIZER', 'SPECIALIZER');
  mopClass('SLOT-DEFINITION', 'METAOBJECT');
  mopClass('DIRECT-SLOT-DEFINITION', 'SLOT-DEFINITION');
  mopClass('EFFECTIVE-SLOT-DEFINITION', 'SLOT-DEFINITION');
  mopClass('STANDARD-SLOT-DEFINITION', 'SLOT-DEFINITION');
  mopClass('STANDARD-DIRECT-SLOT-DEFINITION', 'STANDARD-SLOT-DEFINITION', 'DIRECT-SLOT-DEFINITION');
  mopClass('STANDARD-EFFECTIVE-SLOT-DEFINITION', 'STANDARD-SLOT-DEFINITION', 'EFFECTIVE-SLOT-DEFINITION');
  builtin('METHOD-COMBINATION', ['METAOBJECT']);
  builtin('NULL', ['SYMBOL', 'LIST']); builtin('CONS', ['LIST']); builtin('NUMBER', ['T']);
  builtin('REAL', ['NUMBER']); builtin('RATIONAL', ['REAL']); builtin('INTEGER', ['RATIONAL']);
  builtin('RATIO', ['RATIONAL']); builtin('FLOAT', ['REAL']); builtin('DOUBLE-FLOAT', ['FLOAT']); builtin('SINGLE-FLOAT', ['FLOAT']);
  builtin('COMPLEX', ['NUMBER']); builtin('ARRAY', ['T']); builtin('VECTOR', ['ARRAY', 'SEQUENCE']);
  builtin('STRING', ['VECTOR']); builtin('BIT-VECTOR', ['VECTOR']); builtin('CHARACTER', ['T']); builtin('HASH-TABLE', ['T']);
  builtin('RESTART', ['T']);
  builtin('STREAM', ['T']); builtin('STRING-STREAM', ['STREAM']);
  builtin('TWO-WAY-STREAM', ['STREAM']); builtin('FILE-STREAM', ['STREAM']);
  for (const name of ['BROADCAST-STREAM', 'CONCATENATED-STREAM', 'ECHO-STREAM', 'SYNONYM-STREAM']) builtin(name, ['STREAM']);
  builtin('RANDOM-STATE', ['T']);
  const builtinConditionNames = new Map();
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
    const cls = builtin(name, parents);
    cls.condition = true;
    builtinConditionNames.set(cls, name);
  }
  for (const [className, ...fields] of [
    ['SIMPLE-CONDITION', ['FORMAT-CONTROL', 'formatControl'], ['FORMAT-ARGUMENTS', 'formatArguments']],
    ['TYPE-ERROR', ['DATUM', 'datum'], ['EXPECTED-TYPE', 'expectedType']], ['CELL-ERROR', ['NAME', 'cellName']],
    ['UNBOUND-SLOT', ['INSTANCE', 'instance']], ['ARITHMETIC-ERROR', ['OPERATION', 'operation'], ['OPERANDS', 'operands']],
    ['STREAM-ERROR', ['STREAM', 'stream']], ['PACKAGE-ERROR', ['PACKAGE', 'package']], ['FILE-ERROR', ['PATHNAME', 'pathname']],
    ['PRINT-NOT-READABLE', ['OBJECT', 'object']],
  ]) classes.get(className).directSlots = fields.map(([name, conditionField]) => new SlotDefinition(classes.get('STANDARD-DIRECT-SLOT-DEFINITION'), {
    name: intern(name), initargs: [keyword(name)], allocation: 'INSTANCE', conditionField,
    hasInitform: conditionField === 'formatArguments', initfunction: conditionField === 'formatArguments' ? { invoke: () => NIL } : NIL,
    initform: NIL, type: T, readers: [], writers: [], documentation: NIL,
  }));
  for (const [name, cls] of classes) if (typeof name === 'string' && cls.condition) {
    const groups = groupDirectSlots(cls.cpl, c => c);
    const slots = [...groups].map(([slotName, direct]) => new SlotDefinition(classes.get('STANDARD-EFFECTIVE-SLOT-DEFINITION'),
      effectiveSlotFields(slotName, direct, types => fromArray([intern('AND'), ...types]))));
    Object.assign(cls, slotLayout(slots));
  }
  builtin('PATHNAME', ['T']); builtin('LOGICAL-PATHNAME', ['PATHNAME']);
  builtin('PACKAGE', ['T']); builtin('READTABLE', ['T']);
  const standardClass = classes.get('STANDARD-CLASS'), funcallableClass = classes.get('FUNCALLABLE-STANDARD-CLASS');
  const standardObject = classes.get('STANDARD-OBJECT'), funcallableObject = classes.get('FUNCALLABLE-STANDARD-OBJECT');
  for (const [name, cls] of classes) if (typeof name === 'string') {
    const meta = cls.cpl.includes(funcallableObject) ? funcallableClass
      : cls.cpl.includes(standardObject) || cls.condition ? standardClass
        : classes.get(name === 'STRUCTURE-OBJECT' ? 'STRUCTURE-CLASS' : 'BUILT-IN-CLASS');
    initializeStorage(cls, meta);
  }
  const findClass = (name, errorp = true) => {
    if (name instanceof LispClass) return name;
    const cls = classes.get(name);
    if (!cls && errorp) fail(`Unknown class ${spelling(name)}`);
    return cls ?? null;
  };
  const classDeclarations = environment => {
    for (let scope = environment; scope; scope = scope.parent) if (scope.closClassDeclarations) return scope.closClassDeclarations;
    return null;
  };
  const findClassInEnvironment = (name, errorp, environment) => classDeclarations(environment)?.get(name) ?? findClass(name, errorp);
  const noteClosClass = (environment, name, parents, metaclass, documentation) => {
    const declarations = classDeclarations(environment);
    if (!declarations) return;
    const previous = declarations.get(name) ?? findClass(name, false);
    const meta = metaclass === NIL ? previous?.classObject ?? standardClass : findClassInEnvironment(metaclass, false, environment) ?? standardClass;
    const cls = new LispClass(name);
    initializeStorage(cls, meta);
    cls.documentation = documentation;
    cls.directSuperclasses = parents.length ? parents.map(parent => findClassInEnvironment(parent, false, environment)).filter(Boolean)
      : [meta.cpl.includes(funcallableClass) ? funcallableObject : standardObject];
    cls.cpl = classPrecedence(cls);
    declarations.set(name, cls);
  };
  const classOf = (object, task = rt.activeTask) => {
    if (hasStandardStorage(object)) return instanceRecord(object, task).classObject;
    if (object instanceof StructureInstance) return object.classObject;
    if (object instanceof SlotDefinition) return object.classObject;
    if (object instanceof LispCondition) return classes.get(object.className) ?? classes.get('ERROR');
    if (object instanceof GenericFunction) return classes.get('STANDARD-GENERIC-FUNCTION');
    if (object instanceof Method) return classes.get('STANDARD-METHOD');
    if (object instanceof EqlSpecializer) return classes.get('EQL-SPECIALIZER');
    if (object instanceof LispRestart) return classes.get('RESTART');
    const streamClass = rt.streamClassName?.(object);
    if (streamClass !== undefined && classes.has(streamClass)) return classes.get(streamClass);
    if (object === NIL) return classes.get('NULL');
    if (object instanceof LispSymbol) return classes.get('SYMBOL');
    if (object instanceof Cons) return classes.get('CONS');
    if (typeof object === 'bigint') return classes.get('INTEGER');
    if (typeof object === 'number') return classes.get(Number.isInteger(object) ? 'INTEGER' : 'DOUBLE-FLOAT');
    if (typeof object === 'string' || object instanceof LispString) return classes.get('STRING');
    if (object instanceof LispArray) {
      if (object.rank !== 1) return classes.get('ARRAY');
      return classes.get(object.elementKind === 'bit' ? 'BIT-VECTOR' : object.elementKind === 'character' ? 'STRING' : 'VECTOR');
    }
    if (Array.isArray(object) || ArrayBuffer.isView(object)) return classes.get('VECTOR');
    if (object instanceof LispHashTable) return classes.get('HASH-TABLE');
    if (object instanceof LispCharacter) return classes.get('CHARACTER');
    if (object instanceof LispPathname) return classes.get(object.logical ? 'LOGICAL-PATHNAME' : 'PATHNAME');
    if (object instanceof Ratio) return classes.get('RATIO');
    if (object instanceof Complex) return classes.get('COMPLEX');
    if (object instanceof SingleFloat) return classes.get('SINGLE-FLOAT');
    if (object instanceof DoubleFloat) return classes.get('DOUBLE-FLOAT');
    if (object instanceof RandomState) return classes.get('RANDOM-STATE');
    if (rt.isReadtable?.(object)) return classes.get('READTABLE');
    if (object?.symbols instanceof Map && object.exports instanceof Set) return classes.get('PACKAGE');
    if (callable(object) || typeof object === 'function') return classes.get('FUNCTION');
    return classes.get('T');
  };
  const normalizeSlot = spec => {
    if (spec instanceof LispSymbol) spec = { name: spec };
    if (!spec || !(spec.name instanceof LispSymbol) || spec.name.constant) fail('Slot name must be a variable name');
    const allocation = spec.allocation ?? 'INSTANCE';
    if (typeof allocation !== 'string' && !(allocation instanceof LispSymbol)) fail('Slot allocation must be a symbol');
    return {
      ...spec, allocation: allocation === keyword('INSTANCE') ? 'INSTANCE' : allocation === keyword('CLASS') ? 'CLASS' : allocation,
      initargs: [...(spec.initargs ?? [])], readers: [...(spec.readers ?? [])], writers: [...(spec.writers ?? [])],
      hasInitform: spec.hasInitform ?? (spec.initfunction !== undefined && spec.initfunction !== NIL),
      initfunction: spec.initfunction ?? NIL, initform: spec.initform ?? NIL, type: spec.type ?? T, documentation: spec.documentation ?? NIL,
    };
  };
  const initargValues = args => {
    const values = new Map();
    if (args.length % 2) fail('Initialization arguments must be symbol/value pairs');
    for (let i = 0; i < args.length; i += 2) {
      if (!(args[i] instanceof LispSymbol)) fail('Initialization argument names must be symbols');
      if (!values.has(args[i])) values.set(args[i], args[i + 1]);
    }
    return values;
  };
  const slotArguments = spec => [
    keyword('NAME'), spec.name, keyword('ALLOCATION'), typeof spec.allocation === 'string' ? keyword(spec.allocation) : spec.allocation,
    keyword('INITARGS'), fromArray(spec.initargs), keyword('READERS'), fromArray(spec.readers), keyword('WRITERS'), fromArray(spec.writers),
    keyword('INITFUNCTION'), spec.initfunction, keyword('INITFORM'), spec.initform, keyword('TYPE'), spec.type, keyword('DOCUMENTATION'), spec.documentation,
  ];
  const slotFields = args => {
    const values = initargValues(args), field = (name, value) => values.get(keyword(name)) ?? value;
    const fields = normalizeSlot({
      name: field('NAME', undefined), allocation: field('ALLOCATION', undefined),
      initargs: array(field('INITARGS', NIL)), readers: array(field('READERS', NIL)), writers: array(field('WRITERS', NIL)),
      initfunction: field('INITFUNCTION', NIL), initform: field('INITFORM', NIL), type: field('TYPE', T), documentation: field('DOCUMENTATION', NIL),
    });
    if (fields.initargs.some(name => !(name instanceof LispSymbol)) || fields.hasInitform && !callable(fields.initfunction))
      fail('Invalid slot initialization metadata');
    return fields;
  };
  const mopCall = (name, args) => rt.requestCall(rt.intern(name, mop), args);
  const clCall = (name, args) => rt.requestCall(intern(name), args);
  const defaultEntries = value => {
    const defaults = new Map();
    for (const entry of array(value)) {
      const fields = array(entry);
      if (fields.length !== 3 || !(fields[0] instanceof LispSymbol) || !callable(fields[2])) fail('Invalid default initialization argument');
      if (!defaults.has(fields[0])) defaults.set(fields[0], [fields[1], fields[2]]);
    }
    return defaults;
  };
  const defaultList = defaults => fromArray([...defaults].map(([key, [form, fn]]) => fromArray([key, form, fn])));
  const classClaims = new Map();
  const resolveParents = (designators, condition, pending) => array(designators).map(designator => {
    const parent = findClass(designator, false) ?? pending.get(designator);
    if (parent && (!condition || parent.condition)) {
      if (parent.unpublishedForward) pending.set(parent.name, parent);
      return parent;
    }
    if (parent || condition || !(designator instanceof LispSymbol) || designator === NIL) fail('Invalid direct superclass');
    if (classClaims.has(designator)) fail('Superclass is still being defined');
    const forward = new LispClass(designator);
    forward.forwardReferenced = true;
    forward.unpublishedForward = true;
    initializeStorage(forward, classes.get('FORWARD-REFERENCED-CLASS'));
    pending.set(designator, forward);
    return forward;
  });
  const requireClass = cls => {
    if (!(cls instanceof LispClass)) throw new LispCondition('TYPE-ERROR', 'Expected a class metaobject', { datum: cls, expectedType: intern('CLASS') });
    return cls;
  };
  const requireSpecializer = spec => {
    if (!(spec instanceof LispClass || spec instanceof Specializer))
      throw new LispCondition('TYPE-ERROR', 'Expected a specializer', { datum: spec, expectedType: mopSymbol('SPECIALIZER') });
    return spec;
  };
  const finalizeLayout = function* (cls, candidate, task) {
    const cpl = array(primary(yield mopCall('COMPUTE-CLASS-PRECEDENCE-LIST', [cls])));
    const expected = candidate.cpl;
    if (cpl[0] !== cls || cpl.length !== expected.length || new Set(cpl).size !== cpl.length || cpl.some(parent => !expected.includes(parent)))
      fail('Invalid class precedence list');
    candidate.cpl = cpl;
    candidate.finalized = true;
    const slots = array(primary(yield mopCall('COMPUTE-SLOTS', [cls])));
    if (slots.some(slot => !(slot instanceof SlotDefinition) || !classOf(slot, task).cpl.includes(classes.get('EFFECTIVE-SLOT-DEFINITION')))
        || new Set(slots.map(slot => slot.name)).size !== slots.length) fail('Invalid effective slot definitions');
    const sharedInitializers = [];
    for (const slot of slots) {
      if (slot.finalized) fail('COMPUTE-SLOTS returned a previously finalized slot definition');
      if (slot.allocation === 'CLASS' && !slot.cell) {
        for (const owner of cpl) {
          const direct = definition(owner, task).directSlots.find(direct => direct.name === slot.name && direct.cell);
          if (direct) { slot.cell = direct.cell; break; }
        }
        if (!slot.cell) {
          slot.cell = { value: UNBOUND };
          if (slot.hasInitform) sharedInitializers.push(slot);
        }
      }
    }
    Object.assign(candidate, slotLayout(slots));
    candidate.defaultInitargs = defaultEntries(primary(yield mopCall('COMPUTE-DEFAULT-INITARGS', [cls])));
    candidate.layoutReady = true;
    for (const slot of sharedInitializers) slot.cell.value = primary(yield rt.requestCall(slot.initfunction, []));
    for (const slot of candidate.directSlots) if (slot.allocation === 'CLASS' && slot.cell.pending) {
      if (slot.hasInitform) slot.cell.value = primary(yield rt.requestCall(slot.initfunction, []));
      slot.cell.pending = false;
    }
    return NIL;
  };
  const configureClass = function* (cls, values, task, { condition = !!cls.condition, provisional = false, alreadyEntered = false } = {}) {
    const pending = new Map();
    const updates = inTask(classUpdates, task), observed = new Map();
    const parents = values.has(keyword('DIRECT-SUPERCLASSES'))
      ? resolveParents(values.get(keyword('DIRECT-SUPERCLASSES')), condition, pending) : cls.directSuperclasses;
    const meta = classOf(cls, task);
    const name = values.get(keyword('NAME')) ?? cls.name;
    if (!(name instanceof LispSymbol)) fail('Class name must be a symbol');
    const directSuperclasses = parents.length ? parents : [condition ? classes.get('CONDITION') : meta.cpl.includes(funcallableClass) ? funcallableObject : standardObject];
    if (new Set(directSuperclasses).size !== directSuperclasses.length) fail('Duplicate direct superclass');
    if (directSuperclasses.some(parent => parent === cls || parent.cpl.includes(cls) || parent.unpublishedForward && parent.name === name)) fail('Circular class inheritance');
    if (!condition && directSuperclasses.some(parent => parent.condition || parent.classObject === classes.get('STRUCTURE-CLASS')))
      fail('Incompatible superclass storage');
    const affected = new Map([[cls, [...cls.children]]]);
    for (const children of affected.values()) for (const child of children)
      if (!affected.has(child)) affected.set(child, [...child.children]);
    const graphParents = current => {
      if (updates.has(current)) fail('Overlapping class finalization');
      observed.set(current, current.version);
      return current === cls ? directSuperclasses : current.directSuperclasses;
    };
    const candidates = new Map();
    for (const current of affected.keys()) {
      const cpl = classPrecedence(current, graphParents);
      candidates.set(current, {
        cpl, directSuperclasses: current === cls ? directSuperclasses : current.directSuperclasses,
        directSlots: current.directSlots, directDefaultInitargs: current.directDefaultInitargs,
        documentation: current.documentation, version: current.version + 1,
        slots: new Map(), locations: [], defaultInitargs: new Map(), prototype: undefined,
        finalized: false, forwardReferenced: false, condition: current === cls ? condition : current.condition,
        complete: cpl.every(parent => parent === cls || !parent.forwardReferenced), layoutReady: false,
      });
    }
    const candidate = candidates.get(cls);
    const newSlots = values.get(keyword('DIRECT-SLOTS'));
    if (values.has(keyword('DOCUMENTATION'))) candidate.documentation = values.get(keyword('DOCUMENTATION'));
    if (values.has(keyword('DIRECT-DEFAULT-INITARGS'))) candidate.directDefaultInitargs = defaultEntries(values.get(keyword('DIRECT-DEFAULT-INITARGS')));
    candidate.name = name;
    for (const [current, state] of candidates) updates.set(current, state);
    const links = inTask(subclassLinks, task), linkChanges = new Map();
    const oldParents = cls.directSuperclasses;
    try {
      if (!alreadyEntered && !provisional) for (const parent of directSuperclasses)
        if (primary(yield mopCall('VALIDATE-SUPERCLASS', [cls, parent])) === NIL) fail('Invalid superclass for metaclass');
      if (newSlots !== undefined) {
        const old = new Map(cls.directSlots.map(slot => [slot.name, slot]));
        candidate.directSlots = [];
        const seen = new Set();
        for (const spec of array(newSlots)) {
          const args = array(spec);
          const slot = provisional ? new SlotDefinition(classes.get('STANDARD-DIRECT-SLOT-DEFINITION'), slotFields(args))
            : primary(yield clCall('MAKE-INSTANCE', [primary(yield mopCall('DIRECT-SLOT-DEFINITION-CLASS', [cls, ...args])), ...args]));
          if (!(slot instanceof SlotDefinition) || !classOf(slot, task).cpl.includes(classes.get('DIRECT-SLOT-DEFINITION')) || seen.has(slot.name))
            fail('Invalid or duplicate direct slot');
          seen.add(slot.name);
          if (slot.allocation === 'CLASS') slot.cell = old.get(slot.name)?.cell ?? { value: UNBOUND, pending: true };
          if (!provisional) checkAccessors(slot);
          candidate.directSlots.push(slot);
        }
      }
      const ordered = [...candidates].sort(([, a], [, b]) => a.cpl.length - b.cpl.length);
      for (const [current, state] of ordered) if (state.complete && !provisional) {
        if (alreadyEntered && current === cls) yield* finalizeLayout(current, state, task);
        else yield mopCall('FINALIZE-INHERITANCE', [current]);
        if (!state.layoutReady) fail('FINALIZE-INHERITANCE did not establish a class layout');
      }
      links.set(cls, linkChanges);
      for (const parent of oldParents) if (!directSuperclasses.includes(parent)) {
        linkChanges.set(parent, parent.subclasses.has(cls));
        if (provisional) linkChanges.set(parent, false);
        else yield mopCall('REMOVE-DIRECT-SUBCLASS', [parent, cls]);
      }
      for (const parent of directSuperclasses) if (!oldParents.includes(parent)) {
        linkChanges.set(parent, parent.subclasses.has(cls));
        if (provisional) linkChanges.set(parent, true);
        else yield mopCall('ADD-DIRECT-SUBCLASS', [parent, cls]);
      }
      if ([...observed].some(([current, version]) => current.version !== version)
          || [...affected].some(([current, children]) => current.children.size !== children.length || children.some(child => !current.children.has(child)))
          || [...pending].some(([parentName]) => classes.has(parentName) || classClaims.has(parentName))) fail('Class graph changed during finalization');
      for (const parent of oldParents) if (!directSuperclasses.includes(parent)) parent.children.delete(cls);
      for (const parent of directSuperclasses) if (!oldParents.includes(parent)) parent.children.add(cls);
      for (const [parent, present] of linkChanges)
        if (present) parent.subclasses.add(cls); else parent.subclasses.delete(cls);
      for (const [parentName, parent] of pending) {
        parent.unpublishedForward = false;
        classes.set(parentName, parent);
      }
      for (const [current, state] of candidates) {
        const { complete, layoutReady, ...published } = state;
        Object.assign(current, published);
      }
      const promotion = classPromotions.get(task)?.get(cls);
      if (promotion) Object.assign(cls, promotion);
      cls.declarationActive = !provisional;
    } finally {
      links.delete(cls);
      for (const current of candidates.keys()) updates.delete(current);
    }
    if (!provisional && newSlots !== undefined) yield* installAccessors(cls, task);
    return cls;
  };
  const instance = (object, task) => {
    if (hasStandardStorage(object)) return instanceRecord(object, task);
    if (object instanceof LispCondition) {
      initializeStorage(object, classOf(object, task));
      return object;
    }
    throw new LispCondition('TYPE-ERROR', 'Expected a standard instance', { datum: object, expectedType: standardObject });
  };
  const validateInitargs = (object, slotNames, args, calls, task) => {
    const values = initargValues(args), override = keyword('ALLOW-OTHER-KEYS');
    if (!values.size || (values.get(override) ?? NIL) !== NIL) return;
    const methods = [...calls, ['SHARED-INITIALIZE', [object, slotNames]]].flatMap(([name, actual]) =>
      generics.get(intern(name)).applicable(actual, rt, task));
    const signatures = methods.map(method => method.callSignature);
    if (signatures.some(sig => sig.allowOtherKeys)) return;
    const accepted = new Set([...definition(classOf(object, task), task).slots.values()].flatMap(slot => slot.initargs));
    for (const sig of signatures) for (const key of sig.keys) accepted.add(typeof key === 'string' ? keyword(key) : key);
    for (const key of values.keys()) if (key !== override && !accepted.has(key)) fail(`Invalid initialization argument ${spelling(key)}`);
  };
  const initializeSlots = function* (object, slotNames, args, task) {
    const values = initargValues(args);
    const initialize = slotNames === T ? null : new Set(array(slotNames));
    const record = instance(object, task);
    const slots = instanceDefinition(record, task).slots;
    for (const name of slots.keys()) {
      const slot = instanceDefinition(record, task).slots.get(name);
      if (!slot) continue;
      let supplied = false;
      for (const [key, value] of values) if (slot.initargs.includes(key)) {
        yield* slotOperation(object, name, 'set', value, task);
        supplied = true;
        break;
      }
      if (!supplied && slot.hasInitform && (initialize === null || initialize.has(name))
          && (yield* slotOperation(object, name, 'bound', undefined, task)) === NIL) {
        const value = primary(yield rt.requestCall(slot.initfunction, []));
        yield* slotOperation(object, name, 'set', value, task);
      }
    }
    return object;
  };
  const withDefaultInitargs = function* (cls, args, task) {
    const supplied = [...args], keys = initargValues(args);
    for (const [key, [, fn]] of definition(cls, task).defaultInitargs) if (!keys.has(key))
      supplied.push(key, primary(yield rt.requestCall(fn, [])));
    return supplied;
  };
  const makeConditionIntersection = function* (parents, task) {
    const cls = new LispClass(NIL);
    initializeStorage(cls, standardClass);
    try {
      yield* configureClass(cls, new Map([[keyword('DIRECT-SUPERCLASSES'), fromArray(parents)]]), task, { condition: true });
      return [cls];
    } catch (error) {
      if (error instanceof ClassPrecedenceError && error.precedenceClass === cls) return [null, error];
      throw error;
    }
  };
  const resolveConditionClass = conditionClassResolver(rt, classes.get('CONDITION'), definition, makeConditionIntersection);
  const makeConditionSteps = function* (type, args, task = rt.activeTask) {
    const cls = yield* resolveConditionClass(type, task);
    const supplied = yield* withDefaultInitargs(cls, args, task);
    const object = allocate(cls, task);
    validateInitargs(object, T, supplied, [], task);
    yield clCall('SHARED-INITIALIZE', [object, T, ...supplied]);
    return object;
  };
  const migrate = function* (object, task) {
    const record = instance(object, task);
    let state = instanceDefinition(record, task);
    while (record.storage.version !== state.version) {
      const added = [], discarded = [], discardedValues = [], next = new Map();
      const old = record.storage.layout;
      for (const slot of state.locations) {
        const previous = old.get(slot.name);
        if (!previous || (previous.allocation !== 'INSTANCE' && previous.allocation !== 'CLASS')) added.push(slot.name);
        else {
          const value = rawSlotValue(record, object, previous);
          if (!slot.conditionField && value !== UNBOUND) next.set(slot.name, value);
        }
      }
      for (const [name, slot] of old) if (slot.allocation === 'INSTANCE' && state.slots.get(name)?.allocation !== 'INSTANCE') {
        discarded.push(name);
        const value = rawSlotValue(record, object, slot);
        if (value !== UNBOUND) discardedValues.push(name, value);
      }
      record.storage = { version: state.version, layout: state.slots, values: next };
      yield clCall('UPDATE-INSTANCE-FOR-REDEFINED-CLASS', [object, fromArray(added), fromArray(discarded), fromArray(discardedValues)]);
      state = instanceDefinition(record, task);
    }
    return record;
  };
  const missingSlotSteps = function* (cls, object, name, operation, value) {
    const operationName = { get: 'SLOT-VALUE', bound: 'SLOT-BOUNDP', unbind: 'SLOT-MAKUNBOUND', set: 'SETF' }[operation];
    const more = operation === 'set' ? [value] : [];
    const result = yield clCall('SLOT-MISSING', [cls, object, name, intern(operationName), ...more]);
    if (operation === 'set') return value;
    if (operation === 'unbind') return object;
    const answer = primary(result);
    return operation === 'bound' ? (answer === NIL ? NIL : T) : answer;
  };
  const slotProtocol = {
    get: 'SLOT-VALUE-USING-CLASS', set: 'SLOT-VALUE-USING-CLASS',
    bound: 'SLOT-BOUNDP-USING-CLASS', unbind: 'SLOT-MAKUNBOUND-USING-CLASS',
  };
  const setSlotUsingClass = setter(mopSymbol(slotProtocol.set));
  const slotProtocols = new Map();
  const slotOperation = function* (object, name, operation, value, task) {
    if (!(name instanceof LispSymbol)) throw new LispCondition('TYPE-ERROR', 'Slot name must be a symbol', { datum: name, expectedType: intern('SYMBOL') });
    const structure = object instanceof StructureInstance;
    const record = structure ? object : yield* migrate(object, task);
    const cls = record.classObject, slot = (structure ? cls.slots : record.storage.layout).get(name);
    if (!slot) return yield* missingSlotSteps(cls, object, name, operation, value);
    const { gf, method } = slotProtocols.get(operation);
    if (!customized(gf, method) && standardApplicability() && !customized(effectiveMethod.gf, effectiveMethod.method)
        && !customized(combinationReader.gf, combinationReader.method))
      return yield* operateSlot(record, object, slot, operation, value);
    const result = primary(yield rt.requestCall(gf, operation === 'set' ? [value, cls, object, slot] : [cls, object, slot]));
    if (operation === 'set') return value;
    if (operation === 'unbind') return object;
    return result;
  };
  const useSlot = function* ([cls, object, slot], operation, value, task) {
    const structure = object instanceof StructureInstance;
    const record = structure ? object : yield* migrate(object, task);
    if (cls !== record.classObject || (structure ? cls.slots : record.storage.layout).get(slot.name) !== slot) fail('Slot definition is not effective for this instance');
    return yield* operateSlot(record, object, slot, operation, value);
  };
  const operateSlot = function* (record, object, slot, operation, value) {
    if (object instanceof StructureInstance) {
      if (operation === 'set' || operation === 'unbind') {
        if (slot.readOnly) fail('Cannot change a read-only structure slot');
        object.values[slot.structureIndex] = operation === 'set' ? value : UNBOUND;
      }
      const current = object.values[slot.structureIndex];
      if (operation === 'bound') return current === UNBOUND ? NIL : T;
      if (operation === 'set') return value;
      if (operation === 'unbind') return object;
      if (current !== UNBOUND) return current;
      return primary(yield clCall('SLOT-UNBOUND', [record.classObject, object, slot.name]));
    }
    if (slot.allocation !== 'INSTANCE' && slot.allocation !== 'CLASS') fail('Custom slot allocation requires a slot access method');
    if (operation === 'set') return writeRawSlot(record, object, slot, value);
    if (operation === 'unbind') { writeRawSlot(record, object, slot, UNBOUND); return object; }
    const result = rawSlotValue(record, object, slot);
    if (operation === 'bound') return result === UNBOUND ? NIL : T;
    if (result === UNBOUND) return primary(yield clCall('SLOT-UNBOUND', [record.classObject, object, slot.name]));
    return slot.conditionField && Array.isArray(result) ? fromArray(result) : result;
  };
  const ensureGenericFunction = (name, options = {}, replaceOrdinary = false) => {
    const existing = lookupFunction(name);
    if (existing && !(existing instanceof GenericFunction) && !replaceOrdinary) fail('Generic function name already denotes an ordinary function');
    const methodCombination = options.methodCombination === undefined ? undefined : methodCombinations.parse(options.methodCombination);
    let gf = existing instanceof GenericFunction ? existing : null;
    if (!gf) {
      gf = new GenericFunction(name, { ...options, methodCombination });
      initializeStorage(gf, classes.get('STANDARD-GENERIC-FUNCTION'));
      gf.methodClass = classes.get('STANDARD-METHOD');
      rt.defineFunction(name, gf);
    }
    else {
      const lambdaList = options.lambdaList ?? gf.lambdaList;
      const required = requiredParameters(lambdaList);
      const callSignature = signature(lambdaList);
      if (gf.methods.some(m => !congruent(m.callSignature, callSignature))) fail('Incongruent generic lambda list');
      const previousOrder = gf.argumentPrecedenceOrder.map(i => required.indexOf(gf.required[i]));
      const order = validateArgumentOrder(options.argumentPrecedenceOrder ?? (required.length === previousOrder.length && previousOrder.every(i => i >= 0) ? previousOrder : null), required.length);
      gf.lambdaList = lambdaList; gf.required = required; gf.callSignature = callSignature;
      gf.argumentPrecedenceOrder = order;
      if (options.documentation !== undefined) gf.documentation = options.documentation;
      if (methodCombination !== undefined) { gf.methodCombination = methodCombination; gf.combination = undefined; }
      genericChanged(gf);
    }
    return gf;
  };
  const detachMethod = (gf, method) => {
    if (method.genericFunction !== gf) return false;
    gf.methods.splice(gf.methods.indexOf(method), 1);
    method.genericFunction = null;
    for (const spec of method.specializers) spec.directMethods.delete(method);
    genericChanged(gf);
    return true;
  };
  const attachMethod = (gf, method) => {
    if (method.genericFunction === gf) return;
    method.genericFunction = gf;
    gf.methods.push(method);
    for (const spec of method.specializers) spec.directMethods.add(method);
    genericChanged(gf);
  };
  const genericChanged = gf => {
    gf.target = null;
    gf.classDispatch = undefined;
    gf.groupedApplicable = undefined;
    gf.methodGroups = undefined;
    gf.revision++;
  };
  const matchingMethod = (gf, method) => gf.methods.find(other => sameQualifiers(other.qualifiers, method.qualifiers) && sameSpecializers(other.specializers, method.specializers));
  const checkCongruence = (gf, method) => {
    if (method.specializers.length !== gf.required.length || !congruent(method.callSignature, gf.callSignature))
      fail('Method and generic function lambda lists are incongruent');
  };
  const ensureMethod = (generic, options, fn) => {
    const specializers = options.specializers.map(spec => spec instanceof Specializer ? spec : findClass(spec));
    const existing = generic instanceof GenericFunction ? generic : lookupFunction(generic);
    if (existing && !(existing instanceof GenericFunction)) fail('Method name already denotes an ordinary function');
    const gf = existing ?? ensureGenericFunction(generic, { lambdaList: implicitGenericLambdaList(options.lambdaList) });
    const qualifiers = normalizeQualifiers(options.qualifiers ?? []);
    const method = new Method({ ...options, specializers, qualifiers, fn: fn ?? options.fn });
    initializeStorage(method, options.methodClass ?? gf.methodClass);
    checkCongruence(gf, method);
    const old = matchingMethod(gf, method);
    if (old) detachMethod(gf, old);
    attachMethod(gf, method);
    return method;
  };
  const checkAccessors = slot => {
    for (const [names, count] of [[slot.readers, 1], [slot.writers, 2]]) for (const name of names) {
      if (!(name instanceof LispSymbol) && !rt.setfName?.(name)) fail('Invalid slot accessor function name');
      const existing = lookupFunction(name);
      if (existing && (!(existing instanceof GenericFunction) || existing.required.length !== count || existing.callSignature.optional || existing.callSignature.key || existing.callSignature.rest))
        fail('Incompatible slot accessor function');
    }
  };
  const installAccessors = function* (cls, task) {
    const slots = cls.directSlots, oldMethods = cls.accessorMethods;
    cls.accessorMethods = [];
    try {
      for (const method of oldMethods) if (method.genericFunction) yield clCall('REMOVE-METHOD', [method.genericFunction, method]);
    } finally {
      for (const method of oldMethods) if (method.genericFunction) detachMethod(method.genericFunction, method);
    }
    for (const slot of slots) for (const [names, writer] of [[slot.readers, false], [slot.writers, true]]) {
      const lambdaList = fromArray(writer ? [rt.intern('VALUE'), rt.intern('OBJECT')] : [rt.intern('OBJECT')]);
      const specializers = writer ? [classes.get('T'), cls] : [cls];
      for (const name of names) {
        if (slots !== cls.directSlots) return;
        const gf = functionTarget(name) ?? (yield* ensureGenericSteps(name, [keyword('LAMBDA-LIST'), lambdaList], task));
        const entry = { invoke: (args, _rt, activeTask) =>
          slotOperation(args[writer ? 3 : 2], slot.name, writer ? 'set' : 'get', args[2], activeTask) };
        const method = yield* makeMethodInstance(null, lambdaList, NIL, specializers, entry, NIL, NIL, false, slot, NIL, task, cls, writer);
        yield clCall('ADD-METHOD', [gf, method]);
        if (method.genericFunction === gf) {
          if (slots === cls.directSlots) cls.accessorMethods.push(method);
          else detachMethod(gf, method);
        }
      }
    }
  };
  const allocate = (cls, task) => {
    const state = definition(cls, task);
    if (!state.finalized || (state !== cls && !state.layoutReady)) fail('Class is not finalized');
    if (cls.structure) return new StructureInstance(cls, Array(cls.structureDefinition.slots.length).fill(UNBOUND));
    let object;
    if (state.condition) {
      const kind = builtinConditionNames.get(state.cpl.find(parent => builtinConditionNames.has(parent)));
      const name = state.name ?? cls.name;
      object = new LispCondition(kind, name === NIL ? kind : name.name);
    }
    else if (state.cpl.includes(classes.get('CLASS'))) object = new LispClass(NIL);
    else if (state.cpl.includes(classes.get('STANDARD-GENERIC-FUNCTION'))) {
      object = new GenericFunction();
      object.methodClass = classes.get('STANDARD-METHOD');
    }
    else if (state.cpl.includes(classes.get('STANDARD-METHOD'))) object = new Method();
    else if (state.cpl.includes(classes.get('SLOT-DEFINITION'))) object = new SlotDefinition(cls);
    else if (state.cpl.includes(combinationClass)) return new MethodCombination(null, cls);
    else if (state.cpl.includes(classes.get('EQL-SPECIALIZER'))) object = new EqlSpecializer();
    else if (state.cpl.includes(classes.get('SPECIALIZER'))) object = new Specializer();
    else if (state.cpl.includes(funcallableObject)) return new FuncallableInstance(cls, state);
    else if (state.cpl.includes(standardObject)) return new StandardInstance(cls, state);
    else fail('Cannot allocate an instance of this class');
    initializeStorage(object, cls, state);
    return object;
  };
  const ensureFinalized = function* (cls, task) {
    const state = definition(cls, task);
    if (!state.finalized) yield mopCall('FINALIZE-INHERITANCE', [cls]);
    return cls;
  };
  const classConstructorArgs = (supers, specs, options) => [
    keyword('DIRECT-SUPERCLASSES'), fromArray(array(supers)),
    keyword('DIRECT-SLOTS'), fromArray(array(specs).map(spec => fromArray(slotArguments(normalizeSlot(spec))))),
    keyword('DIRECT-DEFAULT-INITARGS'), defaultList(new Map((options.defaultInitargs ?? []).map(([key, fn]) => [key, [NIL, fn]]))),
    ...(options.metaclass ? [keyword('METACLASS'), options.metaclass] : []),
  ];
  const ensureClass = (name, supers = [], specs = [], options = {}, task) => primary(rt.call(rt.intern('ENSURE-CLASS', mop),
    [name, ...classConstructorArgs(supers, specs, options)], task));
  Object.assign(rt, { findClass, findClassInEnvironment, classOf, ensureClass, ensureGenericFunction, ensureMethod, makeConditionSteps, noteClosClass, structures, structureSourceDefinitions: new Map(),
    closPrecedence: (cls, task) => definition(cls, task).cpl,
    isInstanceOf: (object, cls, task = rt.activeTask) => definition(classOf(object, task), task).cpl.includes(findClass(cls)),
    slotValueSteps: (object, name, task) => slotOperation(object, name, 'get', undefined, task),
    isStandardInstance: hasStandardStorage,
    slotValue: (object, name, task) => primary(rt.call(intern('SLOT-VALUE'), [object, name], task)),
    setSlotValue: (object, name, value, task) => primary(rt.call(intern('%SET-SLOT-VALUE'), [value, object, name], task)),
    closDocumentationTarget(value, kind) {
      if (value instanceof LispClass || value instanceof Method || value instanceof SlotDefinition) return value;
      if (kind === intern('TYPE') && value instanceof LispSymbol) return findClass(value, false);
      if (kind === intern('FUNCTION') && (value instanceof LispSymbol || rt.setfName?.(value))) return lookupFunction(value);
      return null;
    },
    expandClosForm: (form, environment) => expandClosForm(rt, form, environment) });
  const handlers = new Map();
  const arities = {
    'FIND-CLASS': [1, 3], 'CLASS-OF': [1, 1], 'CLASS-NAME': [1, 1], '(SETF CLASS-NAME)': [2, 2],
    'MAKE-INSTANCE': [1], 'ALLOCATE-INSTANCE': [1], 'INITIALIZE-INSTANCE': [1], 'REINITIALIZE-INSTANCE': [1], 'SHARED-INITIALIZE': [2],
    'UPDATE-INSTANCE-FOR-REDEFINED-CLASS': [4], 'UPDATE-INSTANCE-FOR-DIFFERENT-CLASS': [2], 'CHANGE-CLASS': [2],
    'SLOT-VALUE': [2, 2], 'SLOT-BOUNDP': [2, 2], 'SLOT-MAKUNBOUND': [2, 2], 'SLOT-EXISTS-P': [2, 2], '%SET-SLOT-VALUE': [3, 3],
    'MAKE-INSTANCES-OBSOLETE': [1, 1], 'CALL-NEXT-METHOD': [0], 'NEXT-METHOD-P': [0, 0], 'COMPUTE-APPLICABLE-METHODS': [2, 2],
    'METHOD-QUALIFIERS': [1, 1], 'ADD-METHOD': [2, 2], 'REMOVE-METHOD': [2, 2], 'FIND-METHOD': [3, 4],
    'SLOT-MISSING': [4, 5], 'SLOT-UNBOUND': [3, 3], 'NO-APPLICABLE-METHOD': [1], 'NO-NEXT-METHOD': [2], 'PRINT-OBJECT': [2, 2],
    'COPY-STRUCTURE': [1, 1], 'ENSURE-GENERIC-FUNCTION': [1], 'MAKE-LOAD-FORM': [1, 2], 'MAKE-LOAD-FORM-SAVING-SLOTS': [1],
    '(SETF FIND-CLASS)': [2, 4],
    '%%REGISTER-STRUCTURE': [5, 5], '%%STRUCTURE-DEFAULT': [2, 2], '%%MAKE-STRUCTURE': [1], '%%READ-STRUCTURE-SLOT': [3, 3], '%%WRITE-STRUCTURE-SLOT': [4, 4], '%%STRUCTURE-PREDICATE': [2, 2], '%%COPY-STRUCTURE': [2, 2],
    '%%DEFINE-CONDITION': [6, 6], '%%DEFINE-GENERIC': [5, 5], '%%DEFINE-METHOD': [6, 7],
  };
  const checkedCall = (name, [minimum, maximum = Infinity], fn) => (args, runtime, task) => {
      if (args.length < minimum || args.length > maximum) fail(`Wrong number of arguments to ${name}`);
      return fn(args, runtime, task);
  };
  const define = (name, fn) => {
    const checked = checkedCall(name, arities[name], fn);
    handlers.set(name, checked);
    return rt.defineBuiltin(name, checked);
  };
  const defineMop = (name, range, fn) => rt.defineFunction(mopSymbol(name), { invoke: checkedCall(name, range, fn) });
  define('FIND-CLASS', args => {
    if (!(args[0] instanceof LispSymbol)) throw new LispCondition('TYPE-ERROR', 'Class name must be a symbol', { datum: args[0] });
    return findClassInEnvironment(args[0], args[1] !== NIL, args[2]) ?? NIL;
  });
  define('(SETF FIND-CLASS)', args => {
    const [value, name] = args;
    if (!(name instanceof LispSymbol) || (value !== NIL && !(value instanceof LispClass))) throw new LispCondition('TYPE-ERROR', 'Expected a class binding and a symbol name', { datum: value });
    if (classClaims.has(name)) fail('Class name is already being defined');
    if (classes.get(name)?.builtin) fail('Cannot change a built-in class binding');
    if (value === NIL) classes.delete(name); else classes.set(name, value);
    return value;
  });
  define('CLASS-OF', (args, _rt, task) => classOf(args[0], task));
  define('CLASS-NAME', ([cls], _rt, task) => definition(requireClass(cls), task).name ?? cls.name);
  define('(SETF CLASS-NAME)', function* ([name, cls], _rt, task) {
    requireClass(cls);
    if (!(name instanceof LispSymbol)) throw new LispCondition('TYPE-ERROR', 'Class name must be a symbol', { datum: name });
    const meta = classOf(cls, task);
    if (meta.cpl.includes(standardClass) || meta.cpl.includes(funcallableClass)) yield clCall('REINITIALIZE-INSTANCE', [cls, keyword('NAME'), name]);
    else cls.name = name;
    return name;
  });
  define('ALLOCATE-INSTANCE', function* ([cls], _rt, task) {
    yield* ensureFinalized(requireClass(cls), task);
    return allocate(cls, task);
  });
  define('MAKE-INSTANCE', function* ([classDesignator, ...initargs], _rt, task) {
    const cls = findClass(classDesignator);
    if (classDesignator instanceof LispSymbol) return yield clCall('MAKE-INSTANCE', [cls, ...initargs]);
    yield* ensureFinalized(cls, task);
    if (cls.structure) return yield* rt.constructStructureSteps(cls.structureDefinition.name, initargs);
    const supplied = yield* withDefaultInitargs(cls, initargs, task);
    const object = primary(yield clCall('ALLOCATE-INSTANCE', [cls, ...supplied]));
    validateInitargs(object, T, supplied, [['ALLOCATE-INSTANCE', [cls]], ['INITIALIZE-INSTANCE', [object]]], task);
    yield clCall('INITIALIZE-INSTANCE', [object, ...supplied]);
    return object;
  });
  define('SHARED-INITIALIZE', function* ([object, slots, ...args], _rt, task) {
    yield* migrate(object, task);
    return yield* initializeSlots(object, slots, args, task);
  });
  define('INITIALIZE-INSTANCE', ([object, ...args]) => clCall('SHARED-INITIALIZE', [object, T, ...args]));
  define('REINITIALIZE-INSTANCE', function* ([object, ...args], _rt, task) {
    yield* migrate(object, task);
    validateInitargs(object, NIL, args, [['REINITIALIZE-INSTANCE', [object]]], task);
    yield clCall('SHARED-INITIALIZE', [object, NIL, ...args]);
    return object;
  });
  define('UPDATE-INSTANCE-FOR-REDEFINED-CLASS', ([object, added, discarded, plist, ...args], _rt, task) => {
    validateInitargs(object, added, args, [['UPDATE-INSTANCE-FOR-REDEFINED-CLASS', [object, added, discarded, plist]]], task);
    return clCall('SHARED-INITIALIZE', [object, added, ...args]);
  });
  define('UPDATE-INSTANCE-FOR-DIFFERENT-CLASS', ([previous, current, ...args], _rt, task) => {
    const old = instance(previous, task), next = instance(current, task);
    const added = fromArray([...next.storage.layout.values()]
      .filter(slot => slot.allocation === 'INSTANCE' && !old.storage.layout.has(slot.name)).map(slot => slot.name));
    validateInitargs(current, added, args, [['UPDATE-INSTANCE-FOR-DIFFERENT-CLASS', [previous, current]]], task);
    return clCall('SHARED-INITIALIZE', [current, added, ...args]);
  });
  define('CHANGE-CLASS', function* ([object, classDesignator, ...args], _rt, task) {
    const cls = findClass(classDesignator);
    if (classDesignator instanceof LispSymbol) return yield clCall('CHANGE-CLASS', [object, cls, ...args]);
    const promotion = classPromotions.get(task)?.has(object);
    const generic = object instanceof GenericFunction;
    const canChange = promotion || generic && cls.cpl.includes(classes.get('STANDARD-GENERIC-FUNCTION')) || object instanceof StandardInstance
      && !cls.cpl.includes(classes.get('METAOBJECT')) && (object instanceof FuncallableInstance) === cls.cpl.includes(funcallableObject);
    if (!canChange || cls.condition || !cls.cpl.includes(standardObject)) fail('Incompatible classes for CHANGE-CLASS');
    initargValues(args);
    yield* ensureFinalized(cls, task);
    const record = yield* migrate(object, task);
    const previous = Object.assign(Object.create(Object.getPrototypeOf(object)), object);
    attachStorage(previous, record.classObject, record.storage);
    const old = previous.storage.layout, state = definition(cls, task);
    initializeStorage(record, cls, state);
    for (const { name } of state.locations) {
      const previousSlot = old.get(name);
      if (previousSlot?.allocation !== 'INSTANCE' && previousSlot?.allocation !== 'CLASS') continue;
      const value = rawSlotValue(previous, previous, previousSlot);
      if (value !== UNBOUND) record.storage.values.set(name, value);
    }
    if (generic) genericChanged(object);
    yield clCall('UPDATE-INSTANCE-FOR-DIFFERENT-CLASS', [previous, object, ...args]);
    if (generic) yield* refreshGeneric(object);
    return object;
  });
  define('SLOT-VALUE', (args, _rt, task) => slotOperation(args[0], args[1], 'get', undefined, task));
  define('%SET-SLOT-VALUE', (args, _rt, task) => slotOperation(args[1], args[2], 'set', args[0], task));
  define('SLOT-BOUNDP', (args, _rt, task) => slotOperation(args[0], args[1], 'bound', undefined, task));
  define('SLOT-MAKUNBOUND', (args, _rt, task) => slotOperation(args[0], args[1], 'unbind', undefined, task));
  define('SLOT-EXISTS-P', function* (args, _rt, task) {
    if (!(args[1] instanceof LispSymbol)) throw new LispCondition('TYPE-ERROR', 'Slot name must be a symbol', { datum: args[1] });
    const object = args[0];
    if (object instanceof StructureInstance) return object.classObject.slots.has(args[1]) ? T : NIL;
    if (!(object instanceof LispCondition) && !hasStandardStorage(object)) return NIL;
    return (yield* migrate(object, task)).storage.layout.has(args[1]) ? T : NIL;
  });
  define('MAKE-INSTANCES-OBSOLETE', args => {
    const cls = findClass(args[0]), affected = new Set([cls]);
    for (const current of affected) {
      current.version++;
      for (const child of current.children) affected.add(child);
    }
    return cls;
  });
  const callNext = function* (context, args, task) {
    if (context.next) {
      const result = context.next(args.length ? args : null, task);
      return typeof result?.next === 'function' ? yield* result : yield result;
    }
    if (context.explicitWithoutNext) throw new LispCondition('CONTROL-ERROR', 'CALL-METHOD supplied no next methods');
    const actual = args.length ? (context.validateNextArgs ? yield* context.validateNextArgs(args, task) : args) : context.args;
    return yield clCall('NO-NEXT-METHOD', [context.generic ?? NIL, context.method, ...actual]);
  };
  define('CALL-NEXT-METHOD', (args, _rt, task) => {
    const context = task?.closMethodContexts?.at(-1);
    if (!context) fail('No next method outside a method invocation');
    return callNext(context, args, task);
  });
  define('NEXT-METHOD-P', (_args, _rt, task) => task?.closMethodContexts?.at(-1)?.next ? T : NIL);
  const requireGeneric = gf => {
    if (!(gf instanceof GenericFunction)) throw new LispCondition('TYPE-ERROR', 'Expected a generic function', { datum: gf, expectedType: intern('GENERIC-FUNCTION') });
    return gf;
  };
  const requireMethod = method => {
    if (!(method instanceof Method)) throw new LispCondition('TYPE-ERROR', 'Expected a method', { datum: method, expectedType: intern('METHOD') });
    return method;
  };
  define('COMPUTE-APPLICABLE-METHODS', ([gf, args], _rt, task) => fromArray(requireGeneric(gf).applicable(array(args), rt, task)));
  define('METHOD-QUALIFIERS', ([method]) => fromArray(requireMethod(method).qualifiers));
  define('NO-APPLICABLE-METHOD', args => fail(`No applicable method for ${spelling(args[0].name)}`));
  define('NO-NEXT-METHOD', () => fail('No next method'));
  define('SLOT-MISSING', args => { throw new LispCondition('ERROR', `Missing slot ${spelling(args[2])}`, { instance: args[1], slotName: args[2] }); });
  define('SLOT-UNBOUND', args => { throw new LispCondition('UNBOUND-SLOT', `Unbound slot ${spelling(args[2])}`, { instance: args[1], cellName: args[2] }); });
  define('PRINT-OBJECT', (args, _rt, task) => rt.printObjectFallbackSteps(args[0], args[1], task));
  rt.defineFunction(rt.intern('%DEFAULT-STRUCTURE-PRINT', mop), { invoke: handlers.get('PRINT-OBJECT') });
  rt.primitives?.set('%DEFAULT-PRINT-OBJECT', { invoke: (args, runtime, task) => handlers.get('PRINT-OBJECT')(args.slice(2), runtime, task) });
  const notifyDependents = function* (object, args) {
    for (const dependent of [...object.dependents]) yield mopCall('UPDATE-DEPENDENT', [object, dependent, ...args]);
  };
  const refreshGeneric = function* (gf) {
    const revision = gf.revision;
    const fn = primary(yield mopCall('COMPUTE-DISCRIMINATING-FUNCTION', [gf]));
    if (!callable(fn)) fail('COMPUTE-DISCRIMINATING-FUNCTION must return a function');
    if (revision === gf.revision) gf.target = fn === defaultDiscriminator(gf) ? null : fn;
  };
  define('REMOVE-METHOD', function* ([gf, method]) {
    requireGeneric(gf); requireMethod(method);
    if (!detachMethod(gf, method)) return gf;
    for (const spec of method.specializers) yield mopCall('REMOVE-DIRECT-METHOD', [spec, method]);
    yield* refreshGeneric(gf);
    yield* notifyDependents(gf, [intern('REMOVE-METHOD'), method]);
    return gf;
  });
  define('ADD-METHOD', function* ([gf, method]) {
    requireGeneric(gf); requireMethod(method);
    if (method.genericFunction || !callable(method.fn)) fail('Method is already attached or has no function');
    checkCongruence(gf, method);
    const previous = matchingMethod(gf, method);
    if (previous) {
      yield clCall('REMOVE-METHOD', [gf, previous]);
      if (matchingMethod(gf, method) || method.genericFunction) fail('Methods changed during replacement');
      checkCongruence(gf, method);
    }
    attachMethod(gf, method);
    for (const spec of method.specializers) yield mopCall('ADD-DIRECT-METHOD', [spec, method]);
    yield* refreshGeneric(gf);
    yield* notifyDependents(gf, [intern('ADD-METHOD'), method]);
    return gf;
  });
  define('FIND-METHOD', args => {
    const [gf, qualifiers, specs, errorp = T] = args, q = normalizeQualifiers(qualifiers), raw = array(specs);
    if (raw.length !== requireGeneric(gf).required.length) fail('Wrong number of method specializers');
    // Accept the class designators and literal EQL form used by ANSI-TEST.
    const s = raw.map(spec => {
      if (spec instanceof LispSymbol) return findClass(spec);
      if (spec instanceof Cons && spec.car === eqlSymbol && spec.cdr instanceof Cons && spec.cdr.cdr === NIL)
        return internEqlSpecializer(spec.cdr.car);
      return requireSpecializer(spec);
    });
    const result = gf.methods.find(m => sameQualifiers(m.qualifiers, q) && sameSpecializers(m.specializers, s));
    if (!result && errorp !== NIL) fail('Method not found'); return result ?? NIL;
  });
  const classTarget = name => {
    if (!(name instanceof LispSymbol) || name === NIL || name.constant) fail('Class name must be a non-constant symbol');
    const cls = findClass(name, false);
    if (cls?.builtin || cls?.structure) fail('Cannot redefine this class');
    return cls?.name === name ? cls : null;
  };
  const defineConditionClass = function* (name, parents, slots, defaults, documentation, active, task) {
    const existing = classTarget(name);
    if (existing && !existing.condition) fail('Cannot change a standard class into a condition class');
    if (!active && existing?.declarationActive) return existing;
    const cls = existing ?? new LispClass(name);
    if (!existing) initializeStorage(cls, standardClass);
    const values = initargValues([keyword('DIRECT-SUPERCLASSES'), parents, keyword('DIRECT-SLOTS'), slots,
      keyword('DIRECT-DEFAULT-INITARGS'), defaults, keyword('DOCUMENTATION'), documentation]);
    if (classClaims.has(name)) fail('Class name is already being defined');
    classClaims.set(name, cls);
    try {
      yield* configureClass(cls, values, task, { condition: true, provisional: !active });
      classes.set(name, cls);
      return cls;
    } finally { classClaims.delete(name); }
  };
  define('%%DEFINE-CONDITION', ([name, parents, slots, defaults, documentation, active], _rt, task) =>
    defineConditionClass(name, parents, slots, defaults, documentation, active !== NIL, task));
  const ensureClassUsingClass = function* ([previous, name, ...args], _rt, task) {
    const cls = previous === NIL ? null : requireClass(previous);
    if (classTarget(name) !== cls || classClaims.has(name)) fail('Class binding changed during definition');
    if (cls?.condition) fail('Cannot change a condition class into a standard class');
    const options = initargValues(args), metaclassKey = keyword('METACLASS');
    const meta = findClass(options.get(metaclassKey) ?? (cls && !cls.forwardReferenced ? classOf(cls, task) : standardClass));
    if (!meta.cpl.includes(standardClass) && !meta.cpl.includes(funcallableClass)) fail('Invalid class metaclass');
    options.delete(metaclassKey);
    const parentsKey = keyword('DIRECT-SUPERCLASSES');
    if (options.has(parentsKey)) options.set(parentsKey, fromArray(resolveParents(options.get(parentsKey), false, new Map())));
    options.delete(keyword('NAME'));
    const initargs = [...options].flat();
    classClaims.set(name, cls);
    const promotions = inTask(classPromotions, task);
    try {
      let result = cls;
      if (!cls) result = primary(yield clCall('MAKE-INSTANCE', [meta, keyword('NAME'), name, ...initargs]));
      else {
        if (cls.forwardReferenced) {
          promotions.set(cls, { classObject: cls.classObject, storage: cls.storage });
          yield clCall('CHANGE-CLASS', [cls, meta]);
        } else if (classOf(cls, task) !== meta) fail('Cannot change the metaclass of a defined class');
        yield clCall('REINITIALIZE-INSTANCE', [cls, ...(cls.forwardReferenced ? [keyword('NAME'), name] : []), ...initargs]);
      }
      classes.set(name, requireClass(result));
      return result;
    } finally {
      promotions.delete(cls);
      classClaims.delete(name);
    }
  };
  defineMop('ENSURE-CLASS', [1], ([name, ...args]) => mopCall('ENSURE-CLASS-USING-CLASS', [classTarget(name) ?? NIL, name, ...args]));
  const argumentOrder = (names, lambdaList) => {
    if (names === undefined) return undefined;
    const required = requiredParameters(lambdaList);
    return array(names).map(name => required.indexOf(name));
  };
  const defaultCombination = fromArray([intern('STANDARD')]);
  const combinationClass = classes.get('METHOD-COMBINATION');
  const makeCombination = spec => new MethodCombination(methodCombinations.parse(spec), combinationClass);
  const standardCombination = makeCombination(defaultCombination);
  const defaultDiscriminator = gf => gf.defaultDiscriminator ??= { invoke: (args, runtime, task) => gf.invokeDefault(args, runtime, task) };
  const eqlSymbol = intern('EQL');
  const identitySpecializers = new WeakMap(), eqlValues = new LispHashTable('EQL');
  const eqlFinalizers = new FinalizationRegistry(({ key, reference }) => {
    if (eqlValues.get(key)[0] === reference) eqlValues.delete(key);
  });
  const internEqlSpecializer = object => {
    if (object !== null && (typeof object === 'object' || typeof object === 'function') && !isNumber(object) && !(object instanceof LispCharacter)) {
      let specializer = identitySpecializers.get(object)?.deref();
      if (!specializer) {
        specializer = new EqlSpecializer(object);
        identitySpecializers.set(object, new WeakRef(specializer));
      }
      return specializer;
    }
    let reference = eqlValues.get(object)[0], specializer = reference === NIL ? undefined : reference.deref();
    if (!specializer) {
      specializer = new EqlSpecializer(object);
      reference = new WeakRef(specializer);
      eqlValues.set(object, reference);
      eqlFinalizers.register(specializer, { key: object, reference });
    }
    return specializer;
  };
  const decodeSpecializers = specs => array(specs).map(spec => spec instanceof Cons && spec.car === eqlSymbol ? internEqlSpecializer(spec.cdr)
    : spec instanceof Specializer ? spec : findClass(spec));
  const methodCall = rt.intern('*METHOD-CALL*', mop);
  methodCall.value = NIL; rt.specials?.add(methodCall);
  const methodMetadata = new WeakMap(), definingGeneric = new WeakMap(), ensuringGeneric = new WeakMap();
  const contextualMethod = (fast, publicEntry) => ({
      invoke: function* (actual, _rt, task) {
        const context = task.closMethodContexts.at(-1);
        const next = { invoke: (forwarded, _rt, nextTask) => callNext(context, forwarded, nextTask) };
        if (publicEntry !== NIL) {
          const args = fromArray(actual);
          task.bind(task.currentFrame, [[methodCall, new Cons(args, next)]]);
          return yield rt.requestCall(publicEntry, [args, ...context.methodArgs]);
        }
        return yield rt.requestCall(fast, [next, context.next ? T : NIL, ...actual]);
      },
  });
  const defaultMethodFunction = (getMethod, fast = NIL) => ({
    invoke: ([argumentsList, nextList], _rt, task) => {
      const method = requireMethod(getMethod());
      const next = nextList === NIL ? null : (changed, _task) => rt.requestCall(methodFunction(nextList.car), [changed === null ? argumentsList : fromArray(changed), nextList.cdr]);
      if (fast !== NIL) {
        const args = array(argumentsList);
        const continuation = { invoke: (changed, _rt, activeTask) => callNext({ next, args, generic: method.genericFunction ?? NIL, method }, changed, activeTask) };
        return rt.requestCall(fast, [continuation, next ? T : NIL, ...args]);
      }
      return invokeMethod(rt, task, method, array(argumentsList), next, () => array(nextList));
    },
  });
  const methodFunction = method => {
    requireMethod(method);
    if (method.publicEntry !== NIL) return method.publicEntry;
    return method.methodFunction ??= defaultMethodFunction(() => method);
  };
  const makeMethodInstance = function* (methodClass, lambdaList, qualifiers, specializers, fast, publicEntry, documentation, inline, slotDefinition, args, task, accessorOwner, writer) {
    if (publicEntry === NIL && !callable(fast) || publicEntry !== NIL && !callable(publicEntry)) fail('Method has no function');
    const metadata = { fast, publicEntry, method: null };
    const fn = publicEntry === NIL ? defaultMethodFunction(() => metadata.method, fast) : publicEntry;
    const initargs = [keyword('LAMBDA-LIST'), lambdaList, keyword('QUALIFIERS'), qualifiers, keyword('SPECIALIZERS'), fromArray(specializers),
      keyword('FUNCTION'), fn, keyword('DOCUMENTATION'), documentation,
      ...(slotDefinition === NIL ? [] : [keyword('SLOT-DEFINITION'), slotDefinition]), ...array(args)];
    if (accessorOwner) {
      const kind = writer ? 'WRITER' : 'READER';
      methodClass = requireClass(primary(yield mopCall(`${kind}-METHOD-CLASS`, [accessorOwner, slotDefinition, ...initargs])));
      if (!methodClass.cpl.includes(classes.get(`STANDARD-${kind}-METHOD`))) fail('Invalid accessor method class');
    }
    const entries = inTask(methodMetadata, task), previous = entries.get(fn);
    const inlineKey = keyword('INLINE-METHOD'), previousInline = entries.get(inlineKey);
    entries.set(fn, metadata);
    entries.set(inlineKey, inline);
    try {
      const method = primary(yield clCall('MAKE-INSTANCE', [methodClass, ...initargs]));
      return requireMethod(method);
    } finally {
      if (previous === undefined) entries.delete(fn); else entries.set(fn, previous);
      if (previousInline === undefined) entries.delete(inlineKey); else entries.set(inlineKey, previousInline);
    }
  };
  const functionTarget = name => {
    if (!(name instanceof LispSymbol) && !rt.setfName?.(name)) fail('Invalid function name');
    const gf = lookupFunction(name);
    const macro = rt.macroFunction?.(name) ?? NIL;
    if (gf && !(gf instanceof GenericFunction) || macro !== NIL || rt.isSpecialOperator?.(name))
      fail('Generic function name already denotes an ordinary function, macro or special operator');
    return gf;
  };
  const ensureGenericUsingClass = function* ([old, name, ...args], _rt, task) {
    const declarations = keyword('DECLARATIONS');
    const options = initargValues(args.map((arg, i) => i % 2 === 0 && arg === keyword('DECLARE') ? declarations : arg));
    const classKey = keyword('GENERIC-FUNCTION-CLASS');
    const cls = findClass(options.get(classKey) ?? (old === NIL ? classes.get('STANDARD-GENERIC-FUNCTION') : classOf(old, task)));
    if (!cls.cpl.includes(classes.get('STANDARD-GENERIC-FUNCTION'))) fail('Invalid generic function class');
    options.delete(classKey);
    options.delete(keyword('ENVIRONMENT'));
    options.delete(keyword('NAME'));
    if (old === NIL) return yield clCall('MAKE-INSTANCE', [cls, keyword('NAME'), name, ...[...options].flat()]);
    if (classOf(old, task) !== cls) {
      // The named ANSI operation may change the class. A direct MOP call
      // requires the existing generic function to have the requested class.
      const request = ensuringGeneric.get(task)?.get(old);
      if (request === undefined || !equal(request, name)) fail('Generic function and requested class differ');
      yield clCall('CHANGE-CLASS', [old, cls]);
    }
    yield clCall('REINITIALIZE-INSTANCE', [old, keyword('NAME'), name, ...[...options].flat()]);
    return old;
  };
  const ensureGenericSteps = function* (name, args, task, defining = false) {
    const old = functionTarget(name);
    const definitions = inTask(definingGeneric, task), previous = definitions.get(name);
    const requests = inTask(ensuringGeneric, task), previousRequest = requests.get(old);
    try {
      if (defining) {
        for (const method of old?.methods.slice() ?? []) if (method.inline && method.genericFunction === old) {
          yield clCall('REMOVE-METHOD', [old, method]);
          if (method.genericFunction === old) fail('Could not remove an inline method');
        }
        definitions.set(name, old ?? NIL);
      }
      if (old) requests.set(old, name);
      const gf = requireGeneric(primary(yield mopCall('ENSURE-GENERIC-FUNCTION-USING-CLASS', [old ?? NIL, name, ...args])));
      if (functionTarget(name) !== old) fail('Function binding changed during generic function definition');
      rt.defineFunction(name, gf);
      return gf;
    } finally {
      if (old) { if (previousRequest === undefined) requests.delete(old); else requests.set(old, previousRequest); }
      if (defining) { if (previous === undefined) definitions.delete(name); else definitions.set(name, previous); }
    }
  };
  const defineGeneric = ([name, lambdaList, order, combination, documentation], _rt, task) => ensureGenericSteps(name,
    [keyword('LAMBDA-LIST'), lambdaList, keyword('METHOD-COMBINATION'), combination, keyword('DOCUMENTATION'), documentation,
      ...(order === NIL ? [] : [keyword('ARGUMENT-PRECEDENCE-ORDER'), order])], task, true);
  define('%%DEFINE-GENERIC', defineGeneric);
  define('ENSURE-GENERIC-FUNCTION', ([name, ...args], _rt, task) => ensureGenericSteps(name, args, task));
  rt.exportSymbol(intern('ENSURE-GENERIC-FUNCTION'), mop);
  const compiledMethod = function* ([name, qualifiers, lambdaList, specs, fast, publicEntry, doc, inline, initargs], _rt, task) {
    const specializers = decodeSpecializers(specs);
    const gf = functionTarget(name) ?? (yield* ensureGenericSteps(name, [keyword('LAMBDA-LIST'), implicitGenericLambdaList(lambdaList)], task));
    const methodClass = primary(yield mopCall('GENERIC-FUNCTION-METHOD-CLASS', [gf]));
    const method = yield* makeMethodInstance(methodClass, lambdaList, qualifiers, specializers, fast, publicEntry, doc, inline !== NIL, NIL, initargs, task);
    yield clCall('ADD-METHOD', [gf, method]);
    return method;
  };
  define('%%DEFINE-METHOD', ([name, qualifiers, specs, lambdaList, fast, inline, doc = NIL], runtime, task) =>
    compiledMethod([name, qualifiers, lambdaList, specs, fast, NIL, doc, inline, NIL], runtime, task));
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
      if (cls && !cls.structure) fail('Structure name already denotes another class');
      if (!cls) {
        cls = new LispClass(name);
        initializeStorage(cls, classes.get('STRUCTURE-CLASS'));
      }
      const ancestor = parent?.classObject ?? classes.get('STRUCTURE-OBJECT');
      for (const previous of cls.directSuperclasses) {
        previous.children.delete(cls);
        previous.subclasses.delete(cls);
      }
      cls.directSuperclasses = [ancestor];
      ancestor.children.add(cls);
      ancestor.subclasses.add(cls);
      cls.cpl = classPrecedence(cls);
      const slotFields = (slot, structureIndex) => normalizeSlot({ ...slot, structureIndex, allocation: 'INSTANCE', initargs: [keyword(slot.name.name)] });
      cls.directSlots = slots.flatMap((slot, i) => slot.direct === false ? [] : [new SlotDefinition(classes.get('STANDARD-DIRECT-SLOT-DEFINITION'), slotFields(slot, i))]);
      Object.assign(cls, slotLayout(slots.map((slot, i) => new SlotDefinition(classes.get('STANDARD-EFFECTIVE-SLOT-DEFINITION'), slotFields(slot, i)))));
      cls.structure = true; cls.structureDefinition = definition; cls.finalized = true;
      cls.prototype = undefined;
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
    if (!definition.representation) {
      const value = object.values[index];
      return value === UNBOUND ? clCall('SLOT-UNBOUND', [object.classObject, object, object.classObject.structureDefinition.slots[index].name]) : value;
    }
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
  installers.set(9, (args, runtime, task) => {
    if (args.length !== 5) fail('Invalid compiled generic definition');
    return defineGeneric(args, runtime, task);
  });
  installers.set(10, (args, runtime, task) => {
    if (args.length !== 7) fail('Invalid compiled method definition');
    return compiledMethod([args[0], args[1], args[3], args[2], args[4], NIL, args[5], args[6], NIL], runtime, task);
  });
  installers.set(11, function* (args, _rt, task) {
    if (args.length !== 9) fail('Invalid compiled class declaration');
    const [condition, active, name, parents, slots, defaults, doc, , meta] = args;
    if (condition !== NIL) yield* defineConditionClass(name, parents, slots, defaults, doc, active !== NIL, task);
    else if (active !== NIL) yield mopCall('ENSURE-CLASS', [name, keyword('DIRECT-SUPERCLASSES'), parents, keyword('DIRECT-SLOTS'), slots,
      keyword('DIRECT-DEFAULT-INITARGS'), defaults, keyword('DOCUMENTATION'), doc, ...(meta === NIL ? [] : [keyword('METACLASS'), meta])]);
    return rt.multipleValues(NIL, NIL);
  });
  installers.set(13, args => {
    if (args.length !== 4) fail('Invalid shared slot initializer');
    const slot = findClass(args[0]).slots.get(args[1]);
    if (!slot || slot.allocation !== 'CLASS' || slot.cell !== args[2]) fail('Shared slot initializer does not belong to the class');
    if (slot.cell.value === UNBOUND) slot.cell.value = args[3];
    return rt.multipleValues();
  });
  installers.set(24, checkedCall('PRINT-DEPTH', [0, 0], () => 0));
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
    const options = initargValues(args), slotNames = rt.intern('SLOT-NAMES', 'KEYWORD'), environment = rt.intern('ENVIRONMENT', 'KEYWORD');
    const allowOtherKeys = rt.intern('ALLOW-OTHER-KEYS', 'KEYWORD');
    if ((options.get(allowOtherKeys) ?? NIL) === NIL)
      for (const key of options.keys()) if (key !== slotNames && key !== environment && key !== allowOtherKeys) fail('Unrecognized MAKE-LOAD-FORM-SAVING-SLOTS keyword');
    const structure = object instanceof StructureInstance;
    const record = structure ? object : yield* migrate(object, task), cls = record.classObject;
    const layout = structure ? cls.slots : record.storage.layout;
    const names = options.has(slotNames) ? array(options.get(slotNames))
      : [...layout.values()].filter(slot => slot.allocation === 'INSTANCE').map(slot => slot.name);
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
  for (const [name, range, fn] of [
    ['%DEFGENERIC', [1], ([name, ...args], _rt, task) => ensureGenericSteps(name, args, task, true)],
    ['%LOAD-METHOD', [9, 9], compiledMethod],
    ['%FINISH-CLASS-CHANGE', [1, 2], args => args[0]],
    ['%INSTALL-ACCESSORS', [1, 1], ([cls]) => requireClass(cls)],
  ]) rt.defineFunction(rt.intern(name, mop), { invoke: checkedCall(name, range, fn) });
  const installProtocol = (symbol, parameters, specs, fn, qualifiers = [], methodParameters) => {
    let gf = generics.get(symbol);
    const lambda = names => fromArray(names.map(intern));
    if (!gf) {
      gf = ensureGenericFunction(symbol, { lambdaList: lambda(parameters) }, true);
      generics.set(symbol, gf);
    }
    const lambdaList = methodParameters ? lambda(methodParameters) : parameters.length ? lambda(parameters) : gf.lambdaList;
    const method = ensureMethod(gf, { lambdaList, specializers: specs.map(name => classes.get(name)), qualifiers }, { invoke: fn });
    return { gf, method };
  };
  const installMop = (name, params, specs, fn, qualifiers) => installProtocol(mopSymbol(name), params, specs, fn, qualifiers);
  // These standard generic functions retain their primary method when a user
  // adds before/after/around methods or invokes CALL-NEXT-METHOD.
  for (const [name, parameters, specs] of [
    ['CLASS-NAME', ['CLASS'], ['T']],
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
    ['ADD-METHOD', ['GENERIC', 'METHOD'], ['T', 'T']],
    ['REMOVE-METHOD', ['GENERIC', 'METHOD'], ['T', 'T']],
    ['FIND-METHOD', ['GENERIC', 'QUALIFIERS', 'SPECIALIZERS', '&OPTIONAL', 'ERRORP'], ['T', 'T', 'T']],
    ['METHOD-QUALIFIERS', ['METHOD'], ['T']],
    ['COMPUTE-APPLICABLE-METHODS', ['GENERIC', 'ARGUMENTS'], ['STANDARD-GENERIC-FUNCTION', 'T']],
    ['MAKE-INSTANCES-OBSOLETE', ['CLASS'], ['CLASS']],
  ]) {
    installProtocol(intern(name), parameters, specs, handlers.get(name));
  }
  for (const [name, specs] of [
    ['MAKE-INSTANCE', ['SYMBOL']], ['CHANGE-CLASS', ['T', 'SYMBOL']], ['MAKE-INSTANCES-OBSOLETE', ['SYMBOL']],
    ['INITIALIZE-INSTANCE', ['CONDITION']], ['REINITIALIZE-INSTANCE', ['CONDITION']], ['SHARED-INITIALIZE', ['CONDITION', 'T']],
    ['UPDATE-INSTANCE-FOR-REDEFINED-CLASS', ['CONDITION', 'T', 'T', 'T']],
  ]) installProtocol(intern(name), [], specs, handlers.get(name));
  installProtocol(setter(intern('CLASS-NAME')), ['NAME', 'CLASS'], ['T', 'CLASS'], handlers.get('(SETF CLASS-NAME)'));
  rt.defineFunction(setter(intern('SLOT-VALUE')), { invoke: handlers.get('%SET-SLOT-VALUE') });
  const loadFormParameters = ['OBJECT', '&OPTIONAL', 'ENVIRONMENT'];
  for (const name of ['STANDARD-OBJECT', 'STRUCTURE-OBJECT', 'CONDITION'])
    installProtocol(intern('MAKE-LOAD-FORM'), loadFormParameters, [name], handlers.get('MAKE-LOAD-FORM'));
  installProtocol(intern('MAKE-LOAD-FORM'), loadFormParameters, ['CLASS'], ([cls, environment]) => {
    if (findClassInEnvironment(cls.name, false, environment) !== cls && findClass(cls.name, false) !== cls)
      throw new LispCondition('ERROR', `Class ${cls.name.name} has no proper name`);
    return loadForm('FIND-CLASS', quoteLoadForm(cls.name));
  });

  const classParameters = ['CLASS'], classSpec = ['CLASS'];
  const finalizedDefinition = (cls, task) => {
    const state = definition(cls, task);
    if (!state.finalized) fail('Class is not finalized');
    return state;
  };
  const classReaders = {
    'CLASS-DIRECT-SLOTS': (cls, task) => fromArray(definition(cls, task).directSlots),
    'CLASS-SLOTS': (cls, task) => fromArray([...finalizedDefinition(cls, task).slots.values()]),
    'CLASS-DIRECT-DEFAULT-INITARGS': (cls, task) => defaultList(definition(cls, task).directDefaultInitargs),
    'CLASS-DEFAULT-INITARGS': (cls, task) => defaultList(finalizedDefinition(cls, task).defaultInitargs),
    'CLASS-DIRECT-SUPERCLASSES': (cls, task) => fromArray(definition(cls, task).directSuperclasses),
    'CLASS-DIRECT-SUBCLASSES': (cls, task) => {
      const result = new Set(cls.subclasses);
      for (const [child, changes] of subclassLinks.get(task) ?? []) if (changes.has(cls)) {
        if (changes.get(cls)) result.add(child); else result.delete(child);
      }
      return fromArray([...result]);
    },
    'CLASS-FINALIZED-P': (cls, task) => definition(cls, task).finalized ? T : NIL,
    'CLASS-PRECEDENCE-LIST': (cls, task) => fromArray(finalizedDefinition(cls, task).cpl),
  };
  for (const [name, read] of Object.entries(classReaders)) installMop(name, classParameters, classSpec, ([cls], _rt, task) => read(cls, task));
  const builtinPrototypes = new Map([
    ['T', NIL], ['NULL', NIL], ['SYMBOL', new LispSymbol('PROTOTYPE')], ['LIST', NIL], ['CONS', new Cons(NIL)], ['CHARACTER', new LispCharacter(0)],
    ['INTEGER', 0], ['NUMBER', 0], ['REAL', 0], ['RATIONAL', 0], ['RATIO', new Ratio(1, 2)],
    ['COMPLEX', makeComplex(0, 1)], ['FLOAT', new SingleFloat(0)],
    ['SINGLE-FLOAT', new SingleFloat(0)], ['DOUBLE-FLOAT', new DoubleFloat(0)], ['STRING', new LispString('')],
    ['VECTOR', new LispArray([0])], ['ARRAY', new LispArray([0, 0])], ['BIT-VECTOR', new LispArray([0], { elementType: intern('BIT') })],
    ['HASH-TABLE', new LispHashTable()], ['FUNCTION', { invoke: ([value = NIL]) => value }], ['SEQUENCE', NIL],
  ].map(([name, value]) => [classes.get(name), value]));
  installMop('CLASS-PROTOTYPE', classParameters, classSpec, function* ([cls], _rt, task) {
    yield* ensureFinalized(cls, task);
    const state = finalizedDefinition(cls, task);
    if (builtinPrototypes.has(cls)) return builtinPrototypes.get(cls);
    return state.prototype ??= allocate(cls, task);
  });
  installMop('ENSURE-CLASS-USING-CLASS', ['CLASS', 'NAME', '&REST', 'INITARGS'], ['NULL', 'T'], ensureClassUsingClass);
  installMop('ENSURE-CLASS-USING-CLASS', ['CLASS', 'NAME', '&REST', 'INITARGS'], ['CLASS', 'T'], ensureClassUsingClass);
  installMop('VALIDATE-SUPERCLASS', ['CLASS', 'SUPERCLASS'], ['CLASS', 'CLASS'], ([cls, parent], _rt, task) => {
    const meta = classOf(cls, task), other = classOf(parent, task);
    const standard = value => value === standardClass || value === funcallableClass;
    return parent === classes.get('T') || parent.forwardReferenced || meta === other || standard(meta) && standard(other) ? T : NIL;
  });
  for (const [name, present] of [['ADD-DIRECT-SUBCLASS', true], ['REMOVE-DIRECT-SUBCLASS', false]])
    installMop(name, ['SUPERCLASS', 'SUBCLASS'], ['CLASS', 'CLASS'], ([parent, child], _rt, task) => {
      const staged = subclassLinks.get(task)?.get(child);
      if (staged?.has(parent)) staged.set(parent, present);
      else if (present) parent.subclasses.add(child);
      else parent.subclasses.delete(child);
      return NIL;
    });
  installMop('FINALIZE-INHERITANCE', classParameters, classSpec, function* ([cls], _rt, task) {
    const state = definition(cls, task);
    if (state !== cls) {
      if (!state.complete) fail('Class has forward-referenced superclasses');
      if (!state.layoutReady) yield* finalizeLayout(cls, state, task);
    } else {
      if (!cls.declarationActive && !cls.finalized || cls.cpl.some(parent => parent.forwardReferenced))
        fail('Class has forward-referenced superclasses');
      if (!cls.builtin && !cls.structure) yield* configureClass(cls, new Map(), task, { alreadyEntered: true });
    }
    return NIL;
  });
  installMop('COMPUTE-CLASS-PRECEDENCE-LIST', classParameters, classSpec, ([cls], _rt, task) => fromArray(definition(cls, task).cpl));
  installMop('COMPUTE-DEFAULT-INITARGS', classParameters, classSpec, ([cls], _rt, task) => defaultList(collectDefaultInitargs(definition(cls, task).cpl, parent => definition(parent, task))));
  const slotClassParameters = ['CLASS', '&REST', 'INITARGS'];
  for (const [kind, spec] of [['DIRECT', 'STANDARD-DIRECT-SLOT-DEFINITION'], ['EFFECTIVE', 'STANDARD-EFFECTIVE-SLOT-DEFINITION']])
    installMop(`${kind}-SLOT-DEFINITION-CLASS`, slotClassParameters, classSpec, () => classes.get(spec));
  for (const kind of ['READER', 'WRITER']) installMop(`${kind}-METHOD-CLASS`, ['CLASS', 'SLOT', '&REST', 'INITARGS'],
    ['CLASS', 'DIRECT-SLOT-DEFINITION'], () => classes.get(`STANDARD-${kind}-METHOD`));
  installMop('COMPUTE-SLOTS', classParameters, classSpec, function* ([cls], _rt, task) {
    const groups = groupDirectSlots(definition(cls, task).cpl, parent => definition(parent, task)), result = [];
    for (const [name, direct] of groups)
      result.push(primary(yield mopCall('COMPUTE-EFFECTIVE-SLOT-DEFINITION', [cls, name, fromArray(direct)])));
    return fromArray(result);
  });
  installMop('COMPUTE-SLOTS', classParameters, classSpec, function* ([cls], _rt, task) {
    const slots = primary(yield clCall('CALL-NEXT-METHOD', []));
    const definitions = array(slots);
    if (definitions.some(slot => !(slot instanceof SlotDefinition))) fail('COMPUTE-SLOTS must return slot metaobjects');
    if (definition(cls, task) !== cls) {
      if (definitions.some(slot => slot.finalized)) fail('COMPUTE-SLOTS cannot reorder finalized slot definitions');
      assignSlotLocations(definitions);
    }
    return slots;
  }, ['AROUND']);
  installMop('COMPUTE-EFFECTIVE-SLOT-DEFINITION', ['CLASS', 'NAME', 'SLOTS'], ['CLASS', 'T', 'T'], function* ([cls, name, direct]) {
    const fields = effectiveSlotFields(name, array(direct), types => fromArray([intern('AND'), ...types]));
    const args = slotArguments(fields);
    const slotClass = requireClass(primary(yield mopCall('EFFECTIVE-SLOT-DEFINITION-CLASS', [cls, ...args])));
    if (!slotClass.cpl.includes(classes.get('EFFECTIVE-SLOT-DEFINITION'))) fail('Invalid effective slot definition class');
      const slot = primary(yield clCall('MAKE-INSTANCE', [slotClass, ...args]));
    if (!(slot instanceof SlotDefinition)) fail('Invalid effective slot definition');
    if (slot.allocation === 'CLASS') slot.cell = fields.cell;
    slot.conditionField = fields.conditionField;
    return slot;
  });
  for (const [name, read] of Object.entries({
    'NAME': slot => slot.name, 'ALLOCATION': slot => typeof slot.allocation === 'string' ? keyword(slot.allocation) : slot.allocation,
    'INITARGS': slot => fromArray(slot.initargs), 'INITFORM': slot => slot.initform, 'INITFUNCTION': slot => slot.initfunction,
    'READERS': slot => fromArray(slot.readers), 'WRITERS': slot => fromArray(slot.writers), 'TYPE': slot => slot.type,
    'DOCUMENTATION': slot => slot.documentation, 'LOCATION': slot => slot.location,
  })) installMop(`SLOT-DEFINITION-${name}`, ['SLOT'], ['SLOT-DEFINITION'], ([slot]) => read(slot));
  for (const operation of ['get', 'bound', 'unbind']) slotProtocols.set(operation,
    installMop(slotProtocol[operation], ['CLASS', 'OBJECT', 'SLOT'], ['CLASS', 'T', 'EFFECTIVE-SLOT-DEFINITION'], (args, _rt, task) => useSlot(args, operation, undefined, task)));
  slotProtocols.set('set', installProtocol(setSlotUsingClass, ['NEW-VALUE', 'CLASS', 'OBJECT', 'SLOT'], ['T', 'CLASS', 'T', 'EFFECTIVE-SLOT-DEFINITION'], ([value, ...args], _rt, task) => useSlot(args, 'set', value, task)));
  const localSlot = function* (object, location, funcallable, task) {
    if (funcallable && !(object instanceof FuncallableInstance || object instanceof GenericFunction)) fail('Expected a funcallable instance');
    const record = yield* migrate(object, task);
    const slot = instanceDefinition(record, task).locations[location];
    if (!Number.isSafeInteger(location) || !slot) fail('Invalid local slot location');
    return [record, slot];
  };
  for (const [name, funcallable] of [['STANDARD-INSTANCE-ACCESS', false], ['FUNCALLABLE-STANDARD-INSTANCE-ACCESS', true]]) {
    const symbol = mopSymbol(name);
    defineMop(name, [2, 2], function* ([object, location], _rt, task) {
      const [record, slot] = yield* localSlot(object, location, funcallable, task);
      const value = rawSlotValue(record, object, slot);
      if (value === UNBOUND) return primary(yield clCall('SLOT-UNBOUND', [record.classObject, object, slot.name]));
      return value;
    });
    rt.defineFunction(setter(symbol), { invoke: checkedCall(`(SETF ${name})`, [3, 3], function* ([value, object, location], _rt, task) {
      const [record, slot] = yield* localSlot(object, location, funcallable, task);
      return writeRawSlot(record, object, slot, value);
    }) });
  }
  defineMop('SET-FUNCALLABLE-INSTANCE-FUNCTION', [2, 2], ([object, fn]) => {
    if (!(object instanceof FuncallableInstance || object instanceof GenericFunction) || !callable(fn)) fail('Expected a funcallable instance and a function');
    object.target = fn;
    if (object instanceof GenericFunction) object.revision++;
    return fn;
  });
  defineMop('CLASSP', [1, 1], ([object]) => object instanceof LispClass ? T : NIL);

  const sharedParams = ['OBJECT', 'SLOTS', '&REST', 'INITARGS'];
  const withKeys = (...keys) => [...sharedParams, '&KEY', ...keys];
  const classKeys = ['NAME', 'DIRECT-SUPERCLASSES', 'DIRECT-SLOTS', 'DIRECT-DEFAULT-INITARGS', 'DOCUMENTATION'];
  const initializeClassObject = function* ([cls, , ...args], _rt, task) {
    const values = initargValues(args);
    const layout = classKeys.slice(1, 4).some(key => values.has(keyword(key)));
    if (layout || !cls.declarationActive && !cls.finalized && !classPromotions.get(task)?.has(cls)) {
      if (cls.builtin || cls.structure) fail('Cannot redefine this class');
      yield* configureClass(cls, values, task);
    } else if (classPromotions.get(task)?.has(cls) && cls.forwardReferenced && values.size) {
      yield* configureClass(cls, values, task);
    } else {
      const name = values.get(keyword('NAME'));
      if (name !== undefined) {
        if (!(name instanceof LispSymbol)) fail('Class name must be a symbol');
        cls.name = name;
      }
      if (values.has(keyword('DOCUMENTATION'))) cls.documentation = values.get(keyword('DOCUMENTATION'));
    }
    return cls;
  };
  for (const meta of ['STANDARD-CLASS', 'FUNCALLABLE-STANDARD-CLASS']) installProtocol(intern('SHARED-INITIALIZE'), sharedParams,
    [meta, 'T'], initializeClassObject, ['AFTER'], withKeys(...classKeys));
  installProtocol(intern('SHARED-INITIALIZE'), sharedParams, ['STANDARD-SLOT-DEFINITION', 'T'], ([slot, , ...args]) => {
    if (args.length) Object.assign(slot, slotFields(args));
    return slot;
  }, ['AFTER'], withKeys('NAME', 'ALLOCATION', 'INITARGS', 'READERS', 'WRITERS', 'INITFORM', 'INITFUNCTION', 'TYPE', 'DOCUMENTATION'));
  installProtocol(intern('REINITIALIZE-INSTANCE'), [], ['SLOT-DEFINITION'], () => fail('Cannot reinitialize a slot definition'));
  for (const [field, property] of Object.entries({ NAME: 'name', TYPE: 'type', INITARGS: 'initargs', INITFORM: 'initform',
    INITFUNCTION: 'initfunction', ALLOCATION: 'allocation' })) {
    const symbol = mopSymbol(`SLOT-DEFINITION-${field}`);
    installProtocol(setter(symbol), ['VALUE', 'SLOT'], ['T', 'STANDARD-EFFECTIVE-SLOT-DEFINITION'], ([value, slot]) => {
      if (slot.finalized) fail('Cannot change a finalized slot definition');
      if (field === 'NAME' && (!(value instanceof LispSymbol) || value.constant)) fail('Invalid slot name');
      if (field === 'INITFUNCTION' && value !== NIL && !callable(value)) fail('Invalid slot initialization function');
      if (field === 'ALLOCATION' && (!(value instanceof LispSymbol) || value === NIL)) fail('Invalid slot allocation');
      const stored = field === 'INITARGS' ? array(value).slice() : field === 'ALLOCATION'
        ? value === keyword('INSTANCE') ? 'INSTANCE' : value === keyword('CLASS') ? 'CLASS' : value : value;
      if (field === 'INITARGS' && stored.some(name => !(name instanceof LispSymbol))) fail('Invalid slot initialization argument');
      slot[property] = stored;
      if (field === 'INITFUNCTION') slot.hasInitform = value !== NIL;
      if (field === 'ALLOCATION' && stored !== 'CLASS') slot.cell = undefined;
      return value;
    });
  }

  installMop('ENSURE-GENERIC-FUNCTION-USING-CLASS', ['GENERIC', 'NAME', '&REST', 'INITARGS'], ['NULL', 'T'], ensureGenericUsingClass);
  installMop('ENSURE-GENERIC-FUNCTION-USING-CLASS', ['GENERIC', 'NAME', '&REST', 'INITARGS'], ['GENERIC-FUNCTION', 'T'], ensureGenericUsingClass);
  installMop('FIND-METHOD-COMBINATION', ['GENERIC', 'NAME', 'OPTIONS'], ['GENERIC-FUNCTION', 'T', 'T'],
    ([, name, options]) => makeCombination(new Cons(name, options)));
  installMop('COMPUTE-DISCRIMINATING-FUNCTION', ['GENERIC'], ['STANDARD-GENERIC-FUNCTION'], ([gf]) => defaultDiscriminator(gf));
  const classApplicability = installMop('COMPUTE-APPLICABLE-METHODS-USING-CLASSES', ['GENERIC', 'CLASSES'], ['STANDARD-GENERIC-FUNCTION', 'T'], ([gf, list], _rt, task) => {
    const args = array(list).map(requireClass);
    if (args.length !== gf.required.length) fail('Expected one class per required argument');
    const methods = gf.rankMethods(args.map(cls => definition(cls, task).cpl), (spec, i) => classOf(spec.object, task) === args[i], true);
    return methods === null ? rt.multipleValues(NIL, NIL) : rt.multipleValues(fromArray(methods), T);
  }).gf;
  const valueApplicability = generics.get(intern('COMPUTE-APPLICABLE-METHODS'));
  const classDefault = classApplicability.methods[0], valueDefault = valueApplicability.methods[0];
  const customized = (gf, base) => gf.target || gf.methods.length !== 1 || gf.methods[0] !== base;
  const standardApplicability = () => !customized(classApplicability, classDefault) && !customized(valueApplicability, valueDefault);
  const selectingMethods = new WeakMap();
  rt.selectClosMethods = function* (gf, args, task) {
    const byClass = customized(classApplicability, classDefault), byValue = customized(valueApplicability, valueDefault);
    if ((!byClass && !byValue) || gf === classApplicability || gf === valueApplicability || selectingMethods.get(task)?.has(gf))
      return gf.orderedMethods(args, rt, task);
    const current = inTask(selectingMethods, task);
    current.set(gf, true);
    try {
      if (byClass) {
        const classes = fromArray(gf.required.map((_, i) => classOf(args[i], task)));
        const result = yield rt.requestCall(classApplicability, [gf, classes]);
        if ((result.values?.[1] ?? NIL) !== NIL) return array(primary(result)).map(requireMethod);
      }
      return array(primary(yield rt.requestCall(valueApplicability, [gf, fromArray(args)]))).map(requireMethod);
    } finally { current.delete(gf); }
  };
  const combinationOf = gf => gf.combination ??= gf.methodCombination === null ? standardCombination : new MethodCombination(gf.methodCombination, combinationClass);
  const combinationReader = installMop('GENERIC-FUNCTION-METHOD-COMBINATION', ['GENERIC'], ['STANDARD-GENERIC-FUNCTION'], ([gf]) => combinationOf(gf));
  const invokeCombination = rt.intern('%INVOKE-METHOD-COMBINATION', mop);
  rt.defineFunction(invokeCombination, { invoke: ([combination, gf, methods, argumentsList]) => {
    const applicable = array(methods);
    return methodCombinations.invoke(combination.spec, gf, applicable, array(argumentsList), gf.nextArguments(applicable, rt));
  } });
  const effectiveMethod = installMop('COMPUTE-EFFECTIVE-METHOD', ['GENERIC', 'COMBINATION', 'METHODS'],
    ['STANDARD-GENERIC-FUNCTION', 'METHOD-COMBINATION', 'T'], ([gf, combination, methodList]) => {
      if (!(combination instanceof MethodCombination)) fail('Custom method combinations require a COMPUTE-EFFECTIVE-METHOD method');
      if (combination.spec !== null) return rt.multipleValues(fromArray([invokeCombination, quoteLoadForm(combination), quoteLoadForm(gf),
        quoteLoadForm(methodList), intern('%%ZK-EM-ARGUMENTS')]), NIL);
      const { around, before, after, primary: primaries } = standardMethodGroups(rt, array(methodList));
      if (!primaries.length) return rt.multipleValues(loadForm('ERROR', new LispString('No applicable primary method')), NIL);
      const call = (method, next) => next ? loadForm('CALL-METHOD', method, fromArray(next)) : loadForm('CALL-METHOD', method);
      let core = call(primaries[0], primaries.slice(1));
      if (after.length) core = loadForm('MULTIPLE-VALUE-PROG1', core, ...after.map(method => call(method)));
      if (before.length) core = loadForm('PROGN', ...before.map(method => call(method)), core);
      return rt.multipleValues(around.length ? call(around[0], [...around.slice(1), loadForm('MAKE-METHOD', core)]) : core, NIL);
    });
  const computingEffective = new WeakMap();
  rt.customEffectiveMethod = function* (gf, methods, task) {
    const readsCombination = customized(combinationReader.gf, combinationReader.method);
    if ((!readsCombination && !customized(effectiveMethod.gf, effectiveMethod.method) && gf.combination instanceof MethodCombination)
        || gf === effectiveMethod.gf || gf === combinationReader.gf || gf === classApplicability || gf === valueApplicability
        || computingEffective.get(task)?.has(gf)) return undefined;
    const active = inTask(computingEffective, task);
    active.set(gf, true);
    try {
      const combination = readsCombination ? primary(yield rt.requestCall(combinationReader.gf, [gf])) : combinationOf(gf);
      if (combination === combinationOf(gf) && !customized(effectiveMethod.gf, effectiveMethod.method) && combination instanceof MethodCombination)
        return undefined;
      const args = [gf, combination, fromArray(methods)];
      if (combination === combinationOf(gf) && combination instanceof MethodCombination && !effectiveMethod.gf.target && standardApplicability()) {
      const applicable = effectiveMethod.gf.orderedMethods(args, rt, task);
        if (applicable.length === 1 && applicable[0] === effectiveMethod.method) return undefined;
      }
      return yield rt.requestCall(effectiveMethod.gf, args);
    } finally { active.delete(gf); }
  };
  const methodLambda = installMop('MAKE-METHOD-LAMBDA', ['GENERIC', 'METHOD', 'EXPRESSION', 'ENVIRONMENT'],
    ['STANDARD-GENERIC-FUNCTION', 'STANDARD-METHOD', 'T', 'T'], ([, , expression]) => {
      const argumentsList = new LispSymbol('ARGUMENTS'), next = new LispSymbol('NEXT-METHODS');
      const continuation = new LispSymbol('CONTINUATION'), changed = new LispSymbol('CHANGED');
      const methodFunctionName = rt.intern('METHOD-FUNCTION', mop);
      const call = loadForm('FLET', fromArray([
        loadForm('NEXT-METHOD-P', NIL, loadForm('NOT', loadForm('NULL', next))),
        loadForm('CALL-NEXT-METHOD', fromArray([intern('&REST'), changed]), loadForm('COND',
          fromArray([continuation, loadForm('APPLY', continuation, changed)]),
          fromArray([next, loadForm('FUNCALL', fromArray([methodFunctionName, loadForm('CAR', next)]), loadForm('OR', changed, argumentsList), loadForm('CDR', next))]),
          fromArray([T, loadForm('ERROR', new LispString('No next method'))]))),
      ]), loadForm('APPLY', loadForm('FUNCTION', expression), argumentsList));
      const current = loadForm('AND', loadForm('EQ', loadForm('CAR', methodCall), argumentsList), loadForm('CDR', methodCall));
      return rt.multipleValues(loadForm('LAMBDA', fromArray([argumentsList, next]), loadForm('LET', fromArray([fromArray([continuation, current])]), call)), NIL);
    });
  rt.makeClosMethodLambda = function* (name, expression, environment, declared) {
    if (!customized(methodLambda.gf, methodLambda.method)) return null;
    const known = lookupFunction(name);
    const genericClass = declared?.get(keyword('GENERIC-FUNCTION-CLASS'));
    const cls = genericClass === undefined ? (known instanceof GenericFunction ? classOf(known) : classes.get('STANDARD-GENERIC-FUNCTION')) : findClass(genericClass, false);
    if (!cls && classDeclarations(environment)?.has(genericClass)) return null;
    if (!cls) fail('Unknown generic function class');
    const gf = known instanceof GenericFunction && classOf(known) === cls ? known : primary(yield mopCall('CLASS-PROTOTYPE', [cls]));
    const methodClass = declared?.get(keyword('METHOD-CLASS')) ?? primary(yield mopCall('GENERIC-FUNCTION-METHOD-CLASS', [gf]));
    const methodMeta = findClass(methodClass, false);
    if (!methodMeta && classDeclarations(environment)?.has(methodClass)) return null;
    if (!methodMeta) fail('Unknown method class');
    const method = primary(yield mopCall('CLASS-PROTOTYPE', [methodMeta]));
    return yield rt.requestCall(methodLambda.gf, [gf, method, expression, environment]);
  };
  defineMop('EXTRACT-LAMBDA-LIST', [1, 1], ([lambdaList]) => splitSpecializedLambdaList(rt, lambdaList)[0]);
  defineMop('EXTRACT-SPECIALIZER-NAMES', [1, 1], ([lambdaList]) => fromArray(splitSpecializedLambdaList(rt, lambdaList)[1]));
  const genericFields = ['NAME', 'LAMBDA-LIST', 'ARGUMENT-PRECEDENCE-ORDER', 'METHOD-CLASS', 'METHOD-COMBINATION', 'DECLARATIONS', 'DOCUMENTATION'];
  installProtocol(intern('SHARED-INITIALIZE'), sharedParams, ['STANDARD-GENERIC-FUNCTION', 'T'], function* ([gf, , ...args], _rt, task) {
    const values = initargValues(args), option = key => values.get(keyword(key));
    const name = option('NAME') ?? gf.name, lambdaList = option('LAMBDA-LIST') ?? gf.lambdaList;
    const parameters = rt.validateGenericLambdaList?.(lambdaList);
    const required = parameters?.required ?? requiredParameters(lambdaList);
    const callSignature = parameters ? { required: required.length, optional: parameters.optional.length, rest: parameters.rest !== null,
      key: parameters.keys !== null, keys: parameters.keys?.map(key => key.keyword) ?? [], allowOtherKeys: parameters.allow } : signature(lambdaList);
    const order = validateArgumentOrder(argumentOrder(option('ARGUMENT-PRECEDENCE-ORDER'), lambdaList) ?? (option('LAMBDA-LIST') === undefined ? gf.argumentPrecedenceOrder : null), required.length);
    const methodClass = findClass(option('METHOD-CLASS') ?? gf.methodClass ?? classes.get('STANDARD-METHOD'));
    if (!methodClass.cpl.includes(classes.get('STANDARD-METHOD'))) fail('Invalid standard method class');
    const defining = definingGeneric.get(task)?.get(name);
    const validCombination = value => value instanceof MethodCombination || classOf(value, task).cpl.includes(combinationClass);
    let combination = option('METHOD-COMBINATION') ?? (defining ? defaultCombination : gf.combination ?? defaultCombination);
    const revision = gf.revision;
    if (!validCombination(combination)) {
      const [type, ...options] = array(combination);
      combination = primary(yield mopCall('FIND-METHOD-COMBINATION', [gf, type, fromArray(options)]));
      if (!validCombination(combination)) fail('Expected a method combination');
    }
    if (revision !== gf.revision) fail('Generic function changed during reinitialization');
    if (gf.methods.some(method => !congruent(method.callSignature, callSignature))) fail('Incongruent generic lambda list');
    Object.assign(gf, { name, lambdaList, required, callSignature, argumentPrecedenceOrder: order, methodClass,
      combination, methodCombination: combination instanceof MethodCombination ? combination.spec : null,
      declarations: option('DECLARATIONS') ?? gf.declarations,
      documentation: option('DOCUMENTATION') ?? gf.documentation });
    genericChanged(gf);
    yield* refreshGeneric(gf);
    return gf;
  }, ['AFTER'], withKeys(...genericFields));
  const methodFields = ['LAMBDA-LIST', 'SPECIALIZERS', 'QUALIFIERS', 'FUNCTION', 'DOCUMENTATION', 'SLOT-DEFINITION'];
  installProtocol(intern('SHARED-INITIALIZE'), sharedParams, ['STANDARD-METHOD', 'T'], ([method, , ...args], _rt, task) => {
    if (method.fn && !args.length) return method;
    const values = initargValues(args), option = key => values.get(keyword(key));
    const lambdaList = option('LAMBDA-LIST'), fn = option('FUNCTION'), specs = option('SPECIALIZERS');
    if (lambdaList === undefined || specs === undefined || !callable(fn)) fail('Method initialization requires a lambda list, specializers and a function');
    const specializers = array(specs).map(requireSpecializer);
    const qualifiers = normalizeQualifiers(option('QUALIFIERS') ?? NIL);
    if (qualifiers.some(q => q === NIL || q instanceof Cons)) fail('Method qualifiers must be non-NIL atoms');
    const slotDefinition = option('SLOT-DEFINITION') ?? NIL;
    if (classOf(method, task).cpl.includes(classes.get('STANDARD-ACCESSOR-METHOD')) && !(slotDefinition instanceof SlotDefinition)) fail('Accessor method requires a direct slot definition');
    const metadata = methodMetadata.get(task)?.get(fn);
    if (metadata) metadata.method = method;
    Object.assign(method, { lambdaList, callSignature: signature(lambdaList), qualifiers, specializers, publicEntry: fn,
      fn: metadata?.publicEntry === NIL ? contextualMethod(metadata.fast, NIL) : contextualMethod(NIL, fn),
      slotDefinition, documentation: option('DOCUMENTATION') ?? NIL,
      inline: methodMetadata.get(task)?.get(keyword('INLINE-METHOD')) ?? false });
    return method;
  }, ['AFTER'], withKeys(...methodFields));
  installProtocol(intern('SHARED-INITIALIZE'), sharedParams, ['EQL-SPECIALIZER', 'T'], ([specializer, , ...args]) => {
    const options = initargValues(args);
    if (options.has(keyword('OBJECT'))) specializer.object = options.get(keyword('OBJECT'));
    return specializer;
  }, ['AFTER'], withKeys('OBJECT'));
  installProtocol(intern('REINITIALIZE-INSTANCE'), [], ['STANDARD-METHOD'], () => fail('Cannot reinitialize a method'));
  for (const spec of ['CLASS', 'GENERIC-FUNCTION']) installProtocol(intern('REINITIALIZE-INSTANCE'), [], [spec], function* ([object, ...args]) {
    yield* notifyDependents(object, args);
    return NIL;
  }, ['AFTER']);
  const genericReaders = {
    'NAME': gf => gf.name, 'LAMBDA-LIST': gf => gf.lambdaList, 'METHOD-CLASS': gf => gf.methodClass,
    'ARGUMENT-PRECEDENCE-ORDER': gf => fromArray(gf.argumentPrecedenceOrder.map(position => gf.required[position])),
    'METHODS': gf => fromArray(gf.methods), 'DECLARATIONS': gf => gf.declarations, 'DOCUMENTATION': gf => gf.documentation,
  };
  for (const [name, read] of Object.entries(genericReaders)) installMop(`GENERIC-FUNCTION-${name}`, ['GENERIC'], ['STANDARD-GENERIC-FUNCTION'], ([gf]) => read(gf));
  installProtocol(setter(mopSymbol('GENERIC-FUNCTION-NAME')), ['NAME', 'GENERIC'], ['T', 'STANDARD-GENERIC-FUNCTION'], function* ([name, gf]) {
    yield clCall('REINITIALIZE-INSTANCE', [gf, keyword('NAME'), name]);
    return name;
  });
  for (const [name, read] of Object.entries({
    'SPECIALIZERS': method => fromArray(method.specializers), 'LAMBDA-LIST': method => method.lambdaList,
    'GENERIC-FUNCTION': method => method.genericFunction ?? NIL, 'FUNCTION': methodFunction,
  })) installMop(`METHOD-${name}`, ['METHOD'], ['STANDARD-METHOD'], ([method]) => read(method));
  installMop('ACCESSOR-METHOD-SLOT-DEFINITION', ['METHOD'], ['STANDARD-ACCESSOR-METHOD'], ([method]) => method.slotDefinition);
  installMop('METHOD-COMBINATION-NAME', ['COMBINATION'], ['METHOD-COMBINATION'], ([combination]) => combination.spec?.name ?? intern('STANDARD'));
  installMop('METHOD-COMBINATION-ARGLIST', ['COMBINATION'], ['METHOD-COMBINATION'], ([combination]) => methodCombinations.argumentList(combination.spec?.name));
  defineMop('INTERN-EQL-SPECIALIZER', [1, 1], ([object]) => internEqlSpecializer(object));
  installMop('EQL-SPECIALIZER-OBJECT', ['SPECIALIZER'], ['EQL-SPECIALIZER'], ([specializer]) => specializer.object === UNBOUND
    ? clCall('SLOT-UNBOUND', [classOf(specializer), specializer, rt.intern('OBJECT', mop)]) : specializer.object);
  for (const [name, add] of [['ADD-DIRECT-METHOD', true], ['REMOVE-DIRECT-METHOD', false]])
    installMop(name, ['SPECIALIZER', 'METHOD'], ['SPECIALIZER', 'METHOD'], ([spec, method]) => {
      if (add && method.genericFunction) spec.directMethods.add(method);
      else if (!add && !method.genericFunction) spec.directMethods.delete(method);
      return NIL;
    });
  installMop('SPECIALIZER-DIRECT-METHODS', ['SPECIALIZER'], ['SPECIALIZER'], ([spec]) => fromArray([...spec.directMethods]));
  installMop('SPECIALIZER-DIRECT-GENERIC-FUNCTIONS', ['SPECIALIZER'], ['SPECIALIZER'], ([spec]) =>
    fromArray([...new Set([...spec.directMethods].map(method => method.genericFunction).filter(Boolean))]));
  for (const spec of ['CLASS', 'GENERIC-FUNCTION']) {
    installMop('ADD-DEPENDENT', ['OBJECT', 'DEPENDENT'], [spec, 'T'], ([object, dependent]) => { object.dependents.add(dependent); return NIL; });
    installMop('REMOVE-DEPENDENT', ['OBJECT', 'DEPENDENT'], [spec, 'T'], ([object, dependent]) => { object.dependents.delete(dependent); return NIL; });
    installMop('MAP-DEPENDENTS', ['OBJECT', 'FUNCTION'], [spec, 'T'], function* ([object, fn]) {
      for (const dependent of [...object.dependents]) yield rt.requestCall(fn, [dependent]);
      return NIL;
    });
    installMop('UPDATE-DEPENDENT', ['OBJECT', 'DEPENDENT', '&REST', 'INITARGS'], [spec, 'T'], () => NIL);
  }
  const printObject = generics.get(intern('PRINT-OBJECT'));
  rt.usesPrintObject = value => value instanceof LispCondition || hasStandardStorage(value) || value instanceof LispRestart ||
    value instanceof StructureInstance && printObject.methods.length > 1;
  return rt;
}
