import { NIL, T, Cons, Closure, LispCode, LispSymbol, LispString, fromArray, toArray, equal } from './objects.mjs';
import { LispCondition } from './execution.mjs';

const shortOperators = ['+', 'AND', 'APPEND', 'LIST', 'MAX', 'MIN', 'NCONC', 'OR', 'PROGN'];
const list = (...elements) => fromArray(elements);
const fail = message => { throw new LispCondition('PROGRAM-ERROR', message); };
const primary = result => result.values[0] ?? NIL;

function formsFor(rt) {
  const cl = name => rt.intern(name, 'COMMON-LISP');
  const kw = name => rt.intern(name, 'KEYWORD');
  const form = (name, ...args) => list(cl(name), ...args);
  const quote = value => form('QUOTE', value);
  return { cl, kw, form, quote };
}

export function installMethodCombinations(rt, mop, clos) {
  const { cl, kw, form } = formsFor(rt);
  const standard = cl('STANDARD');
  const aroundQualifier = kw('AROUND');
  const star = cl('*');
  const combinations = new Map(shortOperators.map(name => [cl(name), { kind: 'short', operator: cl(name), identity: false, documentation: NIL }]));
  const orderLast = order => {
    if (order === kw('MOST-SPECIFIC-FIRST')) return false;
    if (order === kw('MOST-SPECIFIC-LAST')) return true;
    fail('Invalid method combination order');
  };
  const parse = spec => {
    const [name, ...options] = Array.isArray(spec) ? spec : toArray(spec);
    if (name === standard) {
      if (options.length) fail('STANDARD method combination accepts no options');
      return null;
    }
    const definition = combinations.get(name);
    if (!definition) fail(`Unknown method combination ${name?.name ?? ''}`);
    if (definition.kind === 'short') {
      if (options.length > 1) fail('Short method combination accepts at most one option');
      if (options.length) orderLast(options[0]);
    }
    return { name, options };
  };
  const define = (symbol, min, max, fn) => rt.defineFunction(symbol, { invoke(args, _rt, task) {
    if (args.length < min || args.length > max) fail(`Wrong number of arguments to ${symbol.name}`);
    return fn(args, task);
  } });
  const register = (name, definition) => {
    if (!(name instanceof LispSymbol) || name === NIL || name === standard) fail('Invalid method combination name');
    if (definition.documentation !== NIL && !(definition.documentation instanceof LispString)) fail('Method combination documentation must be a string');
    combinations.set(name, definition);
    return name;
  };
  define(cl('%ZK-DEFINE-SHORT-METHOD-COMBINATION'), 4, 4, ([name, documentation, operator, identity]) => {
    if (!(operator instanceof LispSymbol)) fail('Method combination operator must be a symbol');
    return register(name, { kind: 'short', operator, identity: identity !== NIL, documentation });
  });
  define(cl('%ZK-DEFINE-LONG-METHOD-COMBINATION'), 4, 4, ([name, bindOptions, documentation, parameters]) => {
    if (!(bindOptions instanceof Closure || bindOptions instanceof LispCode) && typeof bindOptions?.invoke !== 'function') fail('Expected a method combination option binder');
    return register(name, { kind: 'long', bindOptions, documentation, parameters });
  });
  define(rt.intern('%LONG-EFFECTIVE-ARGUMENT-FORM', mop), 5, 5, ([generic, parameters, variables, binder, body]) => {
    return rt.multipleValues(effectiveArgumentsForm(rt, clos.signature(generic.lambdaList), parameters, variables, binder, body), binder);
  });
  define(cl('%%ZK-EXPAND-CALL-METHOD'), 7, 7, ([method, next, supplied, argv, dispatcher, binder, extra]) => {
    return expandCallMethod(rt, clos.isMethod, method, next, supplied, argv, dispatcher, binder, extra);
  });
  define(cl('%%ZK-MAKE-EFFECTIVE-METHOD'), 3, 3, ([entry, dispatch, sampler]) => clos.makeMethod(entry, dispatch, sampler));
  for (const name of ['STANDARD', 'CALL-METHOD', 'MAKE-METHOD', 'METHOD-COMBINATION']) rt.exportSymbol(cl(name), 'COMMON-LISP');
  define(cl('METHOD-COMBINATION-ERROR'), 1, Infinity, function* ([control, ...args]) {
    return yield rt.requestCall(cl('ERROR'), [control, ...args]);
  });
  define(cl('INVALID-METHOD-ERROR'), 2, Infinity, function* ([_method, control, ...args]) {
    return yield rt.requestCall(cl('ERROR'), [control, ...args]);
  });
  for (const name of ['METHOD-COMBINATION-ERROR', 'INVALID-METHOD-ERROR']) rt.exportSymbol(cl(name), 'COMMON-LISP');

  function shortForm(spec, definition, methods) {
    const around = [], primaries = [];
    for (const method of methods) {
      const [qualifier] = method.qualifiers;
      if (method.qualifiers.length !== 1 || (qualifier !== aroundQualifier && qualifier !== spec.name))
        fail(`Invalid method qualifier for ${spec.name.name} method combination`);
      (qualifier === aroundQualifier ? around : primaries).push(method);
    }
    if (!primaries.length) fail('No applicable primary method');
    if (spec.options.length && orderLast(spec.options[0])) primaries.reverse();
    const calls = primaries.map(method => form('CALL-METHOD', method));
    const core = definition.identity && calls.length === 1 ? calls[0] : list(definition.operator, ...calls);
    return around.length ? form('CALL-METHOD', around[0], list(...around.slice(1), form('MAKE-METHOD', core))) : core;
  }

  function* longForm(definition, options, methods, generic) {
    const [descriptors, build] = toArray(primary(yield rt.requestCall(definition.bindOptions, [fromArray(options)])));
    const groups = toArray(descriptors).map(descriptor => {
      const [name, patterns, order, required] = toArray(descriptor);
      return { name, patterns: toArray(patterns), reverse: orderLast(order), required: required !== NIL, methods: [] };
    });
    // A single group with * or a predicate can hand the ordering of ties to Lisp.
    const singlePattern = groups.length === 1 && groups[0].patterns.length === 1 ? groups[0].patterns[0] : NIL;
    const userOrdersTies = singlePattern instanceof LispSymbol && singlePattern !== NIL;
    applicable: for (const method of methods) {
      for (const group of groups) for (const pattern of group.patterns) {
        const qualifiers = method.qualifiers;
        const matches = pattern instanceof LispSymbol && pattern !== NIL && pattern !== star
          ? primary(yield rt.requestCall(pattern, [fromArray(qualifiers)])) !== NIL
          : qualifierMatches(pattern, qualifiers, star);
        if (!matches) continue;
        const previous = group.methods.at(-1);
        if (!userOrdersTies && previous && clos.sameSpecializers(previous.specializers, method.specializers))
          fail(`Methods in group ${group.name.name} have the same specializers`);
        group.methods.push(method);
        continue applicable;
      }
      yield rt.requestCall(cl('INVALID-METHOD-ERROR'), [method, new LispString('Applicable method with qualifiers ~S belongs to no method group'), fromArray(method.qualifiers)]);
    }
    for (const group of groups) {
      if (group.required && !group.methods.length) fail(`Required method group ${group.name.name} is empty`);
      if (group.reverse) group.methods.reverse();
    }
    const result = yield rt.requestCall(build, [fromArray(groups.map(group => fromArray(group.methods))), generic]);
    return [primary(result), definition.parameters !== NIL ? result.values[1] ?? NIL : NIL];
  }

  return {
    parse,
    argumentList(name) {
      const definition = combinations.get(name);
      return definition?.kind === 'long' ? rt.multipleValues(definition.parameters, T) : rt.multipleValues(NIL, NIL);
    },
    documentation: name => combinations.get(name)?.documentation ?? NIL,
    setDocumentation(name, text) {
      if (text !== NIL && !(text instanceof LispString)) throw new LispCondition('TYPE-ERROR', 'Method combination documentation must be a string', { datum: text });
      const definition = combinations.get(name);
      if (definition) definition.documentation = text;
      return text;
    },
    *invoke(spec, generic, methods, args, validateNextArgs) {
      const definition = combinations.get(spec.name);
      const [effective, binder] = definition.kind === 'short' ? [shortForm(spec, definition, methods), NIL]
        : yield* longForm(definition, spec.options, methods, generic);
      return yield* invokeEffective(rt, clos, effective, binder, generic, args, validateNextArgs);
    },
    *invokeForm(body, options, generic, args, validateNextArgs) {
      const { kw, form, quote } = formsFor(rt), seen = new Set();
      for (const clause of toArray(options)) {
        const [key, ...parameters] = toArray(clause);
        if (seen.has(key)) fail('Repeated effective method option');
        seen.add(key);
        if (key === kw('ARGUMENTS')) body = bindEffectiveArguments(rt, generic.callSignature, fromArray(parameters), body);
        else if (key === kw('GENERIC-FUNCTION') && parameters.length === 1)
          body = form('LET', list(list(parameters[0], quote(generic))), body);
        else fail('Invalid effective method option');
      }
      return yield* invokeEffective(rt, clos, body, NIL, generic, args, validateNextArgs);
    },
  };
}

export function expandMethodCombination(rt, [, name, ...body]) {
  if (!(name instanceof LispSymbol) || name === NIL) fail('DEFINE-METHOD-COMBINATION requires a symbol name');
  if (body[0] === NIL || body[0] instanceof Cons) return expandLongCombination(rt, name, body);
  const { kw, form, quote } = formsFor(rt);
  const shortOptions = [kw('OPERATOR'), kw('DOCUMENTATION'), kw('IDENTITY-WITH-ONE-ARGUMENT')];
  const options = parseOptions(body, shortOptions);
  return form('%ZK-DEFINE-SHORT-METHOD-COMBINATION', quote(name), quote(options.get(kw('DOCUMENTATION')) ?? NIL),
    quote(options.get(kw('OPERATOR')) ?? name), quote(options.get(kw('IDENTITY-WITH-ONE-ARGUMENT')) ?? NIL));
}

function parseOptions(items, keys) {
  const options = new Map();
  if (items.length % 2) fail('Missing method combination option value');
  for (let i = 0; i < items.length; i += 2) {
    const key = items[i];
    if (!keys.includes(key) || options.has(key)) fail('Invalid or repeated method combination option');
    options.set(key, items[i + 1]);
  }
  return options;
}

function qualifierMatches(pattern, qualifiers, star) {
  let cursor = pattern, index = 0;
  while (cursor instanceof Cons && index < qualifiers.length) {
    if (cursor.car !== star && !equal(cursor.car, qualifiers[index])) return false;
    index++;
    cursor = cursor.cdr;
  }
  return cursor === star || (cursor === NIL && index === qualifiers.length);
}

function effectiveScope(rt, argv, dispatcher, binder, body) {
  const { cl, form, quote } = formsFor(rt);
  const method = new LispSymbol('METHOD'), next = new LispSymbol('NEXT'), supplied = new LispSymbol('NEXT-SUPPLIED');
  const extra = new LispSymbol('EXTRA');
  const macro = form('CALL-METHOD', list(method, cl('&OPTIONAL'), list(next, NIL, supplied), cl('&REST'), extra),
    form('%%ZK-EXPAND-CALL-METHOD', method, next, supplied, quote(argv), quote(dispatcher), quote(binder), extra));
  return form('MACROLET', list(macro), body);
}

function* compileEffectiveLambda(rt, parameters, body) {
  const { form } = formsFor(rt);
  // Effective Lisp forms are produced by arbitrary user code at dispatch time.
  // Use the shared Lisp bytecode compiler without requiring host eval.
  const code = yield* rt.compileLambdaSteps(form('LAMBDA', parameters, body));
  return rt.makeFunction(code, { constants: code.constants });
}

function* invokeEffective(rt, clos, body, binder, generic, args, validateNextArgs) {
  const { cl } = formsFor(rt);
  const argv = cl('%%ZK-EM-ARGUMENTS'), dispatcher = new LispSymbol('CALL-EFFECTIVE-METHOD');
  const entry = yield* compileEffectiveLambda(rt, list(argv, dispatcher), effectiveScope(rt, argv, dispatcher, binder, body));
  const invoke = ([actual, first, rest, omitted, ...extra], _rt, task) => {
    const methods = [first, ...toArray(rest)];
    return clos.invokeMethodChain(rt, task, generic, methods, toArray(actual), validateNextArgs, omitted !== NIL, extra);
  };
  return yield rt.requestCall(entry, [fromArray(args), { invoke }]);
}

function* expandCallMethod(rt, isMethod, first, rest, supplied, argv, dispatcher, binder, extra) {
  const { cl, form, quote } = formsFor(rt);
  const makeMethod = cl('MAKE-METHOD');
  const expand = function* (method) {
    if (isMethod(method)) return quote(method);
    if (!(method instanceof Cons) || method.car !== makeMethod) fail('CALL-METHOD requires a method object or MAKE-METHOD form');
    const args = toArray(method.cdr);
    if (args.length !== 1) fail('MAKE-METHOD requires one form');
    const next = new LispSymbol('NEXT'), more = new LispSymbol('NEXT-P'), changed = new LispSymbol('CHANGED');
    const sampler = cl('%%ZK-EM-ARGUMENT-BINDER');
    const local = [form('CALL-NEXT-METHOD', list(cl('&REST'), changed), form('APPLY', next, changed)), form('NEXT-METHOD-P', NIL, more)];
    if (binder !== NIL) {
      const binderArg = new LispSymbol('BINDER-ARGUMENTS');
      local.push(list(binder, list(binderArg), form('FUNCALL', sampler, binderArg)));
    }
    const body = form('FLET', fromArray(local), form('LET', list(list(cl('%%ZK-EM-ARGUMENT-VALUES'), NIL)), args[0]));
    // Compile the synthetic method in its own lexical environment. The only
    // state it receives from the caller is the dispatch and :ARGUMENTS protocol.
    const entry = yield* compileEffectiveLambda(rt, list(next, more, argv, dispatcher, sampler), effectiveScope(rt, argv, dispatcher, binder, body));
    return form('%%ZK-MAKE-EFFECTIVE-METHOD', quote(entry), dispatcher, binder === NIL ? NIL : sampler);
  };
  const entry = yield* expand(first), next = [];
  for (const method of toArray(rest)) next.push(yield* expand(method));
  return form('FUNCALL', dispatcher, argv, entry, form('LIST', ...next), supplied === NIL ? T : NIL, ...toArray(extra));
}

function expandLongCombination(rt, name, [parameters, specs, ...body]) {
  const { cl, kw, form, quote } = formsFor(rt);
  const groups = new LispSymbol('METHOD-GROUPS'), generic = new LispSymbol('GENERIC-FUNCTION'), options = new LispSymbol('COMBINATION-OPTIONS');
  const groupKeys = [kw('ORDER'), kw('REQUIRED'), kw('DESCRIPTION')];
  const variables = [], descriptors = toArray(specs).map(spec => {
    const [variable, ...items] = toArray(spec);
    if (!(variable instanceof LispSymbol) || variable.constant) fail('Method group requires a variable');
    const start = items.findIndex(item => item instanceof LispSymbol && item.package?.name === 'KEYWORD');
    const patterns = items.slice(0, start < 0 ? items.length : start);
    if (!patterns.length) fail('Method group requires a qualifier pattern or predicate');
    if (patterns.some(pattern => !(pattern instanceof Cons) && !(pattern instanceof LispSymbol))) fail('Invalid qualifier pattern');
    const groupOptions = start < 0 ? new Map() : parseOptions(items.slice(start), groupKeys);
    variables.push(variable);
    return form('LIST', quote(variable), quote(fromArray(patterns)), groupOptions.get(kw('ORDER')) ?? kw('MOST-SPECIFIC-FIRST'),
      quote(groupOptions.get(kw('REQUIRED')) ?? NIL), quote(groupOptions.get(kw('DESCRIPTION')) ?? NIL));
  });
  let argumentList = NIL, genericVariable = null;
  const seen = new Set();
  while (body[0] instanceof Cons && [kw('ARGUMENTS'), kw('GENERIC-FUNCTION')].includes(body[0].car)) {
    const [key, ...values] = toArray(body.shift());
    if (seen.has(key)) fail('Repeated long method combination option');
    seen.add(key);
    if (key === kw('ARGUMENTS')) argumentList = fromArray(values);
    else {
      if (values.length !== 1 || !(values[0] instanceof LispSymbol) || values[0].constant) fail(':GENERIC-FUNCTION requires a variable');
      genericVariable = values[0];
    }
  }
  const declarations = [];
  let documentation = NIL;
  while (body.length) {
    if (body[0] instanceof Cons && body[0].car === cl('DECLARE')) declarations.push(body.shift());
    else if (documentation === NIL && body.length > 1 && body[0] instanceof LispString) documentation = body.shift();
    else break;
  }
  const argumentVars = effectiveArgumentVariables(rt, argumentList), binder = new LispSymbol('BIND-EFFECTIVE-ARGUMENTS');
  const actual = cl('%%ZK-EM-ARGUMENTS'), cache = cl('%%ZK-EM-ARGUMENT-VALUES');
  const bindings = variables.map((variable, index) => list(variable, form('NTH', index, groups)));
  for (const [index, variable] of argumentVars.entries())
    bindings.push(list(variable, quote(form('SVREF', form('OR', cache, form('SETF', cache, list(binder, actual))), index))));
  if (genericVariable) bindings.push(list(genericVariable, generic));
  const build = form('PROGN', ...body);
  const buildForm = argumentVars.length
    ? list(rt.intern('%LONG-EFFECTIVE-ARGUMENT-FORM', 'ZK-MOP'), generic, quote(argumentList), quote(fromArray(argumentVars)), quote(binder), build)
    : build;
  const builder = form('FUNCTION', form('LAMBDA', list(groups, generic), form('LET', fromArray(bindings), buildForm)));
  const optionBinder = form('FUNCTION', form('LAMBDA', parameters, ...declarations, form('LIST', form('LIST', ...descriptors), builder)));
  return form('%ZK-DEFINE-LONG-METHOD-COMBINATION', quote(name), form('FUNCTION', form('LAMBDA', list(options), form('APPLY', optionBinder, options))),
    quote(documentation), quote(argumentList));
}

function effectiveArgumentVariables(rt, lambdaList) {
  const { cl } = formsFor(rt);
  const parameters = toArray(lambdaList), variables = [];
  let mode = null;
  const markers = ['&WHOLE', '&OPTIONAL', '&REST', '&KEY', '&ALLOW-OTHER-KEYS', '&AUX'].map(cl);
  for (const parameter of parameters) {
    if (markers.includes(parameter)) { mode = parameter; continue; }
    if (!(parameter instanceof Cons)) variables.push(parameter);
    else {
      const [name, , supplied] = toArray(parameter);
      variables.push(mode === cl('&KEY') && name instanceof Cons ? toArray(name)[1] : name);
      if ((mode === cl('&KEY') || mode === cl('&OPTIONAL')) && supplied !== undefined && supplied !== NIL) variables.push(supplied);
    }
  }
  return variables;
}

function bindEffectiveArguments(rt, signature, parameters, body) {
  const { cl, form } = formsFor(rt);
  const args = cl('%%ZK-EM-ARGUMENTS');
  const items = toArray(parameters), bindings = [];
  const markers = ['&WHOLE', '&OPTIONAL', '&REST', '&KEY', '&ALLOW-OTHER-KEYS', '&AUX'].map(cl);
  const positional = signature.required + signature.optional;
  let cursor = 0, index = 0;
  if (items[0] === cl('&WHOLE')) { bindings.push(list(items[1], args)); cursor = 2; }
  while (cursor < items.length && !markers.includes(items[cursor])) {
    bindings.push(list(items[cursor++], index < signature.required ? form('NTH', index, args) : NIL));
    index++;
  }
  if (items[cursor] === cl('&OPTIONAL')) {
    cursor++; index = signature.required;
    while (cursor < items.length && !markers.includes(items[cursor])) {
      const parameter = items[cursor++];
      const [name, init = NIL, supplied = NIL] = parameter instanceof Cons ? toArray(parameter) : [parameter];
      const present = index < positional ? form('NTHCDR', index, args) : NIL;
      bindings.push(list(name, form('IF', present, form('NTH', index, args), init)));
      if (supplied !== NIL) bindings.push(list(supplied, form('NOT', form('NULL', present))));
      index++;
    }
  }
  const tail = items.slice(cursor);
  if (tail.length) {
    if (tail.includes(cl('&KEY')) && !tail.includes(cl('&ALLOW-OTHER-KEYS'))) {
      const aux = tail.indexOf(cl('&AUX'));
      tail.splice(aux < 0 ? tail.length : aux, 0, cl('&ALLOW-OTHER-KEYS'));
    }
    body = form('APPLY', form('FUNCTION', form('LAMBDA', fromArray(tail), body)), tail[0] === cl('&AUX') ? NIL : form('NTHCDR', positional, args));
  }
  return form('LET*', fromArray(bindings), body);
}

function effectiveArgumentsForm(rt, signature, parameters, variables, binder, body) {
  const { cl, form } = formsFor(rt);
  const args = cl('%%ZK-EM-ARGUMENTS'), cache = cl('%%ZK-EM-ARGUMENT-VALUES');
  const sampler = bindEffectiveArguments(rt, signature, parameters, form('VECTOR', ...toArray(variables)));
  return form('FLET', list(list(binder, list(args), sampler)),
    form('LET', list(list(cache, NIL), list(cl('%%ZK-EM-ARGUMENT-BINDER'), form('FUNCTION', binder))), body));
}
