import { NIL, T, Cons, LispString, LispSymbol, fromArray, toArray } from './objects.mjs';
import { LispCondition } from './execution.mjs';
import { expandMethodCombination } from './method_combination.mjs';

const spelling = symbol => symbol instanceof LispSymbol ? symbol.name : String(symbol).toUpperCase();
const array = toArray;
const fail = message => { throw new LispCondition('PROGRAM-ERROR', message); };

export function splitSpecializedLambdaList(rt, parameters) {
  const lambda = [], specializers = [];
  const eql = rt.intern('EQL', 'COMMON-LISP');
  let required = true;
  for (const parameter of array(parameters)) {
    if (parameter instanceof LispSymbol && parameter.name.startsWith('&')) required = false;
    if (!required) { lambda.push(parameter); continue; }
    if (parameter instanceof Cons) {
      const parts = array(parameter);
      if (parts.length !== 2 || !(parts[0] instanceof LispSymbol)) fail('Invalid specialized parameter');
      const [variable, spec] = parts;
      if (spec instanceof Cons && (spec.car !== eql || array(spec).length !== 2)) fail('Invalid EQL specializer');
      lambda.push(variable);
      specializers.push(spec);
    } else {
      lambda.push(parameter);
      specializers.push(T);
    }
  }
  return [fromArray(lambda), specializers];
}

function fileGenericDeclaration(rt, environment, name, classes) {
  for (let scope = environment; scope; scope = scope.parent) {
    if (!scope.closGenericDeclarations) continue;
    const { functions, setters } = scope.closGenericDeclarations, accessor = rt.setfName?.(name);
    const bindings = accessor ? setters : functions, key = accessor ?? name;
    if (classes !== undefined) bindings.set(key, classes);
    return bindings.get(key);
  }
}

export function* expandClosForm(rt, form, environment = NIL, inline = false, declaration = null) {
  if (!(form instanceof Cons)) return undefined;
  const forms = array(form), name = spelling(forms[0]);
  if (name === 'DEFINE-METHOD-COMBINATION') return expandMethodCombination(rt, forms);
  if (!['DEFCLASS', 'DEFINE-CONDITION', 'DEFGENERIC', 'DEFMETHOD', 'DEFSTRUCT', 'WITH-SLOTS', 'WITH-ACCESSORS'].includes(name)) return undefined;
  const sym = name => rt.intern(name, 'COMMON-LISP'), quote = x => fromArray([sym('QUOTE'), x]);
  const invoke = (name, ...args) => fromArray([sym(name), ...args]);
  const thunk = body => invoke('FUNCTION', invoke('LAMBDA', NIL, body));
  const keyword = name => rt.intern(name, 'KEYWORD');
  const mopForm = (name, ...args) => fromArray([rt.intern(name, 'ZK-MOP'), ...args]);
  if (name === 'DEFSTRUCT') {
    const header = forms[1] instanceof LispSymbol ? [forms[1]] : array(forms[1]), structureName = header[0];
    if (!(structureName instanceof LispSymbol)) fail('DEFSTRUCT requires a symbol name');
    let prefix = `${structureName.name}-`, copier = rt.intern(`COPY-${structureName.name}`), predicate = rt.intern(`${structureName.name}-P`);
    let included = null, includeOverrides = [], kind = null, elementType = T, named = false, initialOffset = 0, printer;
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
      else if (key === 'PRINT-FUNCTION' || key === 'PRINT-OBJECT') {
        if (printer || args.length > 1) fail('DEFSTRUCT takes one printer option');
        printer = { fn: args[0], withDepth: key === 'PRINT-FUNCTION' };
      }
      else fail(`Unsupported DEFSTRUCT option ${key}`);
    }
    if (!constructorsSpecified) constructors.push({ name: rt.intern(`MAKE-${structureName.name}`), parameters: null });
    if (kind && printer) fail('A DEFSTRUCT printer requires an untyped structure');
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
          readOnly: replacement.specified.has('READ-ONLY') ? replacement.readOnly : old.readOnly, direct: true };
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
    if (printer) {
      const stream = new LispSymbol('STREAM'), parameters = fromArray([fromArray([object, structureName]), stream]);
      // The printer binds *PRINT-LEVEL* to the remaining depth at each call.
      const body = printer.fn === undefined ? mopForm('%DEFAULT-STRUCTURE-PRINT', object, stream)
        : invoke('FUNCALL', invoke('FUNCTION', printer.fn), object, stream, ...(printer.withDepth ? [0] : []));
      definitions.push(invoke('DEFMETHOD', sym('PRINT-OBJECT'), parameters, body));
    }
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
    const slots = [], defaults = [], options = [], seenSlots = new Set();
    let report = NIL, documentation = NIL, metaclass = NIL;
    for (const raw of array(forms[3])) {
      const [slot, ...spec] = raw instanceof LispSymbol ? [raw] : array(raw);
      if (!(slot instanceof LispSymbol) || slot.constant || seenSlots.has(slot)) fail('Invalid or duplicate slot name');
      if (spec.length % 2) fail('Missing slot option value');
      seenSlots.add(slot);
      const initargs = [], readers = [], writers = [], extensions = new Map(), seen = new Set();
      const fields = [quote(keyword('NAME')), quote(slot)];
      for (let i = 0; i < spec.length; i += 2) {
        const key = spec[i], value = spec[i + 1];
        const option = key instanceof LispSymbol && key === keyword(key.name) ? key.name : null;
        if (['INITFORM', 'ALLOCATION', 'TYPE', 'DOCUMENTATION'].includes(option)) {
          if (seen.has(key)) fail(`Duplicate slot option ${option}`);
          seen.add(key);
          fields.push(quote(key), quote(value));
          if (option === 'INITFORM') fields.push(quote(keyword('INITFUNCTION')), thunk(value));
        } else if (option === 'INITARG') initargs.push(value);
        else if (option === 'READER') readers.push(value);
        else if (option === 'WRITER') writers.push(value);
        else if (option === 'ACCESSOR') { readers.push(value); writers.push(fromArray([sym('SETF'), value])); }
        else {
          if (!(key instanceof LispSymbol) || condition) fail('Invalid slot option');
          const previous = extensions.get(key);
          if (previous) previous.push(value); else extensions.set(key, [value]);
        }
      }
      for (const [key, values] of [['INITARGS', initargs], ['READERS', readers], ['WRITERS', writers]])
        fields.push(quote(keyword(key)), quote(fromArray(values)));
      for (const [key, values] of extensions) fields.push(quote(key), quote(values.length === 1 ? values[0] : fromArray(values)));
      slots.push(invoke('LIST', ...fields));
    }
    const seenOptions = new Set();
    for (const option of forms.slice(4)) {
      if (!(option instanceof Cons) || !(option.car instanceof LispSymbol)) fail('Expected a class option');
      const key = option.car;
      if (seenOptions.has(key)) fail(`Duplicate class option ${key.name}`);
      seenOptions.add(key);
      if (key === keyword('DEFAULT-INITARGS')) {
        const args = array(option.cdr);
        if (args.length % 2) fail('Malformed default initargs');
        const seen = new Set();
        for (let i = 0; i < args.length; i += 2) {
          const [name, form] = [args[i], args[i + 1]];
          if (!(name instanceof LispSymbol) || seen.has(name)) fail('Invalid or duplicate default initarg name');
          seen.add(name);
          defaults.push(invoke('LIST', quote(name), quote(form), thunk(form)));
        }
      } else if (key === keyword('DOCUMENTATION') || key === keyword('METACLASS') || condition && key === keyword('REPORT')) {
        const args = array(option.cdr);
        if (args.length !== 1) fail(`Malformed class option ${key.name}`);
        if (key === keyword('DOCUMENTATION') && args[0] instanceof LispString) documentation = args[0];
        else if (key === keyword('METACLASS') && !condition && args[0] instanceof LispSymbol && args[0] !== NIL) {
          metaclass = args[0];
          options.push(quote(key), quote(metaclass));
        }
        else if (key === keyword('REPORT') && condition) report = args[0];
        else fail(`Invalid class option ${key.name}`);
      } else if (!condition) options.push(quote(key), quote(option.cdr));
      else fail(`Unsupported condition option ${key.name}`);
    }
    const [className, parents] = forms.slice(1, 3), parentNames = array(parents);
    if (!(className instanceof LispSymbol) || className.constant || parentNames.some(parent => !(parent instanceof LispSymbol) || parent === NIL))
      fail('Invalid class or direct superclass name');
    if (new Set(parentNames).size !== parentNames.length) fail('Duplicate direct superclass');
    const nameForm = quote(className), parentForm = quote(parents), slotForm = invoke('LIST', ...slots), defaultForm = invoke('LIST', ...defaults);
    if (!condition) {
      rt.noteClosClass(environment, className, parentNames, metaclass, documentation);
      return mopForm('ENSURE-CLASS', nameForm, quote(keyword('DIRECT-SUPERCLASSES')), parentForm,
        quote(keyword('DIRECT-SLOTS')), slotForm, quote(keyword('DIRECT-DEFAULT-INITARGS')), defaultForm,
        quote(keyword('DOCUMENTATION')), quote(documentation), ...options);
    }
    const define = flag => invoke('%%DEFINE-CONDITION', nameForm, parentForm, slotForm, defaultForm, quote(documentation), quote(flag));
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
    if (forms.length < 3) fail('DEFGENERIC requires a name and lambda list');
    const required = rt.validateGenericLambdaList(forms[2]).required;
    const methods = [], seen = new Set(), options = [], declarations = [], classes = new Map();
    for (const option of forms.slice(3)) {
      const [tag, ...args] = array(option);
      if (!(tag instanceof LispSymbol) || tag !== sym('DECLARE') && tag !== keyword(tag.name)) fail('Invalid DEFGENERIC option');
      const key = tag.name;
      if (key !== 'METHOD' && key !== 'DECLARE' && seen.has(key)) fail(`Duplicate generic option ${key}`);
      seen.add(key);
      if (tag === sym('DECLARE')) {
        if (!args.length || args.some(spec => !(spec instanceof Cons) || spec.car !== sym('OPTIMIZE'))) fail('Invalid DEFGENERIC declaration');
        declarations.push(...args);
      }
      else if (key === 'METHOD') methods.push(fromArray([sym('DEFMETHOD'), forms[1], ...args]));
      else if (key === 'ARGUMENT-PRECEDENCE-ORDER') {
        if (!args.length || args.length !== required.length || new Set(args).size !== args.length || args.some(name => !required.includes(name)))
          fail('Invalid argument precedence order');
        options.push(quote(tag), quote(fromArray(args)));
      } else if (key === 'METHOD-COMBINATION' && args[0] instanceof LispSymbol && args[0] !== NIL) options.push(quote(tag), quote(fromArray(args)));
      else if (args.length === 1 && (key === 'DOCUMENTATION' && args[0] instanceof LispString ||
          ['GENERIC-FUNCTION-CLASS', 'METHOD-CLASS'].includes(key) && args[0] instanceof LispSymbol)) {
        options.push(quote(tag), quote(args[0]));
        if (key !== 'DOCUMENTATION') classes.set(tag, args[0]);
      }
      else fail(`Invalid generic option ${key}`);
    }
    if (declarations.length) options.push(quote(keyword('DECLARATIONS')), quote(fromArray(declarations)));
    fileGenericDeclaration(rt, environment, forms[1], classes);
    const inlineForms = [];
    for (const method of methods) inlineForms.push(yield* expandClosForm(rt, method, environment, true, classes));
    const definition = mopForm('%DEFGENERIC', quote(forms[1]), quote(keyword('LAMBDA-LIST')), quote(forms[2]), ...options);
    return inlineForms.length ? invoke('PROGN', definition, ...inlineForms, invoke('FDEFINITION', quote(forms[1]))) : definition;
  }
  const qualifiers = []; let cursor = 2;
  while (forms[cursor] !== undefined && forms[cursor] !== NIL && !(forms[cursor] instanceof Cons)) qualifiers.push(forms[cursor++]);
  if (forms[cursor] === undefined) fail('DEFMETHOD requires a specialized lambda list');
  const [lambdaList, specs] = splitSpecializedLambdaList(rt, forms[cursor++]);
  const lambda = array(lambdaList);
  const specializers = specs.map(spec => spec instanceof Cons ? invoke('CONS', quote(sym('EQL')), spec.cdr.car) : quote(spec));
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
  let documentation = NIL;
  const declarations = [];
  for (;;) {
    if (body[0] instanceof Cons && body[0].car === sym('DECLARE')) declarations.push(body.shift());
    else if (documentation === NIL && body.length > 1 && body[0] instanceof LispString) documentation = body.shift();
    else break;
  }
  const block = forms[1] instanceof Cons ? array(forms[1])[1] : forms[1];
  const declared = declaration ?? fileGenericDeclaration(rt, environment, forms[1]);
  const expression = invoke('LAMBDA', fromArray(implementationLambda), ...declarations, invoke('BLOCK', block, ...body));
  const transformed = yield* rt.makeClosMethodLambda(forms[1], expression, environment, declared);
  if (transformed) {
    const [publicLambda, initargs = NIL] = transformed.values;
    if (!(publicLambda instanceof Cons) || publicLambda.car !== sym('LAMBDA')) fail('MAKE-METHOD-LAMBDA must return a lambda expression');
    return mopForm('%LOAD-METHOD', quote(forms[1]), quote(fromArray(qualifiers)), quote(lambdaList), invoke('LIST', ...specializers),
      NIL, invoke('FUNCTION', publicLambda), quote(documentation), inline ? T : NIL, quote(initargs));
  }
  const implementation = fromArray([sym('LAMBDA'), fromArray([next, nextp, ...implementationLambda]), ...declarations,
    invoke('BLOCK', block, invoke('FLET', localFunctions, ...body))]);
  return invoke('%%DEFINE-METHOD', quote(forms[1]), quote(fromArray(qualifiers)), invoke('LIST', ...specializers), quote(lambdaList), invoke('FUNCTION', implementation), inline ? T : NIL, quote(documentation));
}
