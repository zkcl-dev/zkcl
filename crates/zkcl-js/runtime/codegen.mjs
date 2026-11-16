import { LispCode, Closure, Cons, Cell, LispArray } from './objects.mjs';
import { operations, variant } from './execution.mjs';

export class CodegenError extends Error {
  constructor(message, options) { super(message, options); this.name = 'CodegenError'; }
}

const sourceOperations = new Set('ReadCell DefineFunction ProclaimSpecial BindProgv BoxLocal Closure ReturnFrom Throw Go EnterRestarts AssociateRestarts RestArguments DestructureNext DestructureEnd DestructureKeywords CallValues Call'.split(' '));
const frameTransfers = new Set(['Call', 'CallValues', 'EndCleanup', 'ReturnFrom', 'Throw', 'Go']);
const localTransfers = new Set(['BeginCleanup', 'WhenBound']);
const MULTIPLE_VALUES_LIMIT = 65_536;

export function stepIdentifier(name) {
  return `step_${String(name ?? 'lambda').replace(/[^A-Za-z0-9]/gu, '_')}`;
}

function integer(value, label, maximum = Number.MAX_SAFE_INTEGER) {
  if (!Number.isSafeInteger(value) || value < 0 || value > maximum) throw new CodegenError(`Invalid ${label}: ${value}`);
  return value;
}
function tuple(value, count) {
  if (!Array.isArray(value) || value.length !== count) throw new CodegenError(`Expected ${count} instruction operands`);
  return value;
}

// The same division as the offline emitter: scalar/register/value operations
// are JavaScript statements; calls, bindings and all nonlocal control share the
// canonical Task operation helpers. No opcode decoder runs in compiled steps.
function inlineOperation(name, payload, code, next, bindings) {
  const reg = value => {
    bindings.registers = true;
    return integer(value, 'register', code.registers - 1);
  };
  const object = name => {
    bindings.objects.add(name);
    return name;
  };
  const index = value => integer(value, 'index');
  const pc = value => integer(value, 'branch target', code.instructions.length);
  const range = value => {
    if (!value || typeof value !== 'object') throw new CodegenError('Invalid register range');
    const start = integer(value.start, 'range start', code.registers), end = integer(value.end, 'range end', code.registers);
    if (end < start) throw new CodegenError('Reversed register range');
    return [start, end];
  };
  const branch = (target, test = null, local = '') => {
    if (target === next) return '';
    if (test === null) return `      frame.pc = ${target};\n      continue dispatch;\n`;
    return `${local}      if (${test}) {\n        frame.pc = ${target};\n        continue dispatch;\n      }\n`;
  };
  const line = statement => `      ${statement}\n`;
  switch (name) {
    case 'Load': {
      const [out, immediate] = tuple(payload, 2), r = reg(out), [kind, value] = variant(immediate);
      if (kind === 'Nil' || kind === 'True') return line(`r[${r}] = ${object(kind === 'Nil' ? 'NIL' : 'T')};`);
      if (kind === 'Fixnum') {
        if (typeof value !== 'number' && !(typeof value === 'string' && /^-?\d+$/.test(value))) throw new CodegenError('Invalid immediate fixnum');
        const n = Number(value);
        if (!Number.isInteger(n) || n < -268435456 || n > 268435455) throw new CodegenError('Immediate fixnum outside JavaScript target range');
        return line(`r[${r}] = ${n};`);
      }
      if (kind === 'Character' && typeof value === 'string' && Array.from(value).length === 1) return line(`r[${r}] = new ${object('LispCharacter')}(${value.codePointAt(0)});`);
      throw new CodegenError(`Invalid immediate kind ${kind}`);
    }
    case 'Constant': {
      const [out, value] = tuple(payload, 2);
      return line(`r[${reg(out)}] = frame.constants[${integer(value, 'constant', (code.constants?.length ?? 0) - 1)}];`);
    }
    case 'Copy': {
      const [out, source] = tuple(payload, 2);
      return line(`r[${reg(out)}] = r[${reg(source)}];`);
    }
    case 'Clear': {
      const [start, end] = range(payload);
      bindings.registers = true;
      return line(`r.fill(${object('NIL')}, ${start}, ${end});`);
    }
    case 'Jump': return branch(pc(payload));
    case 'Unless': case 'IfPresent': {
      const [value, target] = tuple(payload, 2);
      const test = name === 'Unless' ? `=== ${object('NIL')}` : `!== ${object('UNBOUND')}`;
      return branch(pc(target), `r[${reg(value)}] ${test}`);
    }
    case 'UnlessFixnum': case 'UnlessFixnumInRange': {
      const values = tuple(payload, name === 'UnlessFixnum' ? 2 : 4), r = reg(values[0]);
      const low = name === 'UnlessFixnum' ? -268435456 : values[1], high = name === 'UnlessFixnum' ? 268435455 : values[2], target = pc(values.at(-1));
      if (!Number.isInteger(low) || !Number.isInteger(high) || low < -268435456 || high > 268435455 || low > high) throw new CodegenError('Invalid fixnum bounds');
      return branch(target, `typeof n !== 'number' || !Number.isInteger(n) || n < ${low} || n > ${high}`, line(`const n = r[${r}];`));
    }
    case 'SetValues': {
      const [start, end] = range(payload);
      if (end - start >= MULTIPLE_VALUES_LIMIT) return null;
      bindings.registers = true;
      return line(`task.values = r.slice(${start}, ${end});`);
    }
    case 'BindValues': {
      const [start, end] = range(payload);
      bindings.registers = true;
      return `      for (let i = ${start}; i < ${end}; i++) {\n        r[i] = task.values[i - ${start}] ?? ${object('NIL')};\n      }\n      task.values = [];\n`;
    }
    case 'ReadValue': {
      const [out, value] = tuple(payload, 2);
      return line(`r[${reg(out)}] = task.values[${index(value)}] ?? ${object('NIL')};`);
    }
    case 'Capture': {
      const [out, value] = tuple(payload, 2);
      return line(`r[${reg(out)}] = frame.captures[${integer(value, 'capture', (code.captures?.length ?? 0) - 1)}];`);
    }
    case 'OptionalArgument': {
      const [value, out] = tuple(payload, 2);
      return line(`r[${reg(out)}] = frame.arguments[${index(value)}] ?? ${object('UNBOUND')};`);
    }
    case 'KeywordArgument': {
      const [value, out] = tuple(payload, 2);
      index(value);
      integer(out, 'register', code.registers - 1);
      return null;
    }
    case 'ReleaseArguments': return `      frame.arguments = [];\n      frame.keywords = [];\n`;
    default: return null;
  }
}

export function generateStep(code) {
  if (!Array.isArray(code.instructions)) throw new CodegenError('Expected canonical bytecode function metadata');
  integer(code.registers, 'frame width', 1 << 20);
  const operands = [], sourcePcs = [], cases = [];
  const bindings = { registers: false, objects: new Set() };
  for (let pc = 0; pc < code.instructions.length; pc++) {
    let name, payload, inline;
    const next = pc + 1;
    try {
      [name, payload] = variant(code.instructions[pc]);
      if (!Object.hasOwn(operations, name)) throw new CodegenError(`Unknown instruction ${name}`);
      inline = inlineOperation(name, payload, code, next, bindings);
      if (sourceOperations.has(name)) {
        const { at, ...operands } = payload;
        sourcePcs.push(at == null ? null : integer(at, 'source byte offset'));
        payload = operands;
      } else {
        sourcePcs.push(null);
      }
    } catch (cause) {
      throw new CodegenError(`${code.name ?? 'lambda'}, instruction ${pc}: ${cause.message}`, { cause });
    }
    const scoped = name === 'UnlessFixnum' || name === 'UnlessFixnumInRange';
    let body = `    case ${pc}:${scoped ? ' {' : ''}\n      if (task.budget-- <= 0) break dispatch;\n      frame.sourcePc = frame.pc++;\n`;
    if (inline !== null) {
      body += inline;
    } else {
      let argument = '';
      if (payload != null) {
        argument = `, operands[${operands.length}]`;
        operands.push(payload);
      }
      body += `      task.ops.${name}(task, frame${argument});\n`;
      // A thrown error already leaves the step. Only these operations can
      // return with a changed frame, suspension, completion or alternate PC.
      const canLeaveFrame = frameTransfers.has(name);
      if (canLeaveFrame) body += '      if (task.currentFrame !== frame || task.suspended || task.done) return;\n';
      if (canLeaveFrame || localTransfers.has(name)) body += `      if (frame.pc !== ${next}) continue dispatch;\n`;
    }
    if (scoped) body += '    }\n';
    cases.push(body);
  }
  let source = '  "use strict";\n';
  if (operands.length) source += '  const operands = frame.code.operands;\n';
  if (bindings.registers) source += '  const r = frame.registers;\n';
  if (bindings.objects.size) source += `  const { ${Array.from(bindings.objects).sort().join(', ')} } = task.runtime.objects;\n`;
  source += `  dispatch: while (true) {\n    switch (frame.pc) {\n${cases.join('')}    case ${code.instructions.length}:\n      return task.returnFrame(frame);\n    default:\n      throw new Error('invalid generated function PC: ' + frame.pc);\n    }\n  }\n  task.yielded = true;`;
  return { source, operands, instruction_count: code.instructions.length, source_pcs: sourcePcs };
}

function lower(code) {
  const { source, ...metadata } = generateStep(code);
  let step;
  try { step = new Function(`return function ${stepIdentifier(code.name)}(task, frame) {\n${source}\n}`)(); }
  catch (cause) { throw new CodegenError(`JavaScript code generation failed: ${cause.message}`, { cause }); }
  return { code, step, ...metadata };
}

/** Install real JavaScript entrypoints in canonical code without replacing it.
 * Nested code constants are compiled in one transaction. Errors never select an
 * interpreter fallback; callers explicitly choose their bytecode strategy.
 */
export function compileJavaScript(metadata) {
  const seen = new Set(), codes = [], pending = [metadata];
  if (!metadata || (!Array.isArray(metadata.instructions) && typeof metadata.step !== 'function')) throw new CodegenError('Expected canonical bytecode function metadata');
  while (pending.length) {
    const value = pending.pop();
    if (!value || typeof value !== 'object' || seen.has(value)) continue;
    seen.add(value);
    if (value instanceof LispCode) pending.push(value.entry, value.constants);
    else if (value instanceof Closure) pending.push(value.code, value.environment);
    else if (value instanceof Cons) pending.push(value.car, value.cdr);
    else if (value instanceof Cell) pending.push(value.value);
    else if (value instanceof LispArray) pending.push(value.storage, value.displacedTo);
    else if (Array.isArray(value)) { for (const item of value) pending.push(item); }
    else if (Array.isArray(value.instructions)) { codes.push(value); pending.push(value.constants); }
    else if (typeof value.step === 'function') pending.push(value.constants);
  }
  const lowered = codes.map(lower);
  for (const { code, ...installation } of lowered) Object.assign(code, installation);
  return metadata;
}
