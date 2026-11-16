import { NIL, T, UNBOUND, LispSymbol, Cons, Cell, LispCode, Closure, LispCharacter, fromArray, toArray, eq } from './objects.mjs';

export class MultipleValues {
  constructor(values = []) { this.values = values; }
}
export class CallRequest {
  constructor(callee,args=[]) { this.callee=callee;this.args=args; }
}
export class NativeFunction {
  constructor(name, invoke, metadata = {}) { this.name = name; this.invoke = invoke; Object.assign(this, metadata); }
}
export class ExitToken {}
export class LispCondition extends Error {
  constructor(className, message, options = {}) {
    super(message); this.name = className; this.className = className; Object.assign(this, options);
  }
}
export class LispRestart {
  constructor(name, report = NIL) { this.name = name; this.report = report; }
}
export class SuspensionError extends Error {
  constructor(task) { super('A host function suspended; continue the task with runAsync()'); this.task = task; }
}
class ControlTransfer extends Error {}
export const CANCELLED_BOUNDARY = Symbol('cancelled boundary');

export function variant(value) {
  if (typeof value === 'string') return [value, undefined];
  const keys = Object.keys(value ?? {});
  if (keys.length !== 1) throw new TypeError('Expected one bytecode variant');
  return [keys[0], value[keys[0]]];
}
const range = r => Array.isArray(r) ? r : [r.start, r.end];
const selected = (frame, r) => { const [start, end] = range(r); return frame.registers.slice(start, end); };
const primary = values => values.length ? values[0] : NIL;
const fixnum = value => typeof value === 'number' && Number.isInteger(value) && value >= -268435456 && value <= 268435455;
function fault(name, message, fields = {}) { throw new LispCondition(name, message, fields); }
function symbol(value) { if (!(value instanceof LispSymbol) && value !== NIL && value !== T) fault('TYPE-ERROR', 'Expected a symbol', { datum: value }); return value; }
function immediate(value) {
  const [kind, payload] = variant(value);
  switch (kind) {
    case 'Nil': return NIL;
    case 'True': return T;
    case 'Fixnum': { const result = Number(payload); if (!fixnum(result)) fault('PROGRAM-ERROR', 'Immediate fixnum outside JavaScript target range'); return result; }
    case 'Character': return new LispCharacter(payload.codePointAt(0));
    default: throw new TypeError(`Unknown immediate ${kind}`);
  }
}
function cell(frame, location) {
  const [kind, index] = variant(location);
  const value = kind === 'Local' ? frame.registers[index] : frame.captures[index];
  if (!(value instanceof Cell)) fault('PROGRAM-ERROR', 'Expected a lexical cell');
  return value;
}
function keywordArguments(args, descriptor, constants, start) {
  const names = descriptor.names.map(i => constants[i]);
  const result = names.map(() => UNBOUND);
  const override = constants[descriptor.override_key];
  let allow, unknown;
  for (let i = start; i + 1 < args.length; i += 2) {
    const key = args[i];
    let recognized = key === override;
    if (recognized && allow === undefined) allow = args[i + 1] !== NIL;
    names.forEach((name, n) => { if (name === key) { if (result[n] === UNBOUND) result[n] = args[i + 1]; recognized = true; } });
    if (!recognized && unknown === undefined) unknown = key;
  }
  if (!descriptor.allow_other_keys && allow !== true) {
    if ((args.length - start) % 2) fault('PROGRAM-ERROR', 'Odd number of keyword arguments');
    if (unknown !== undefined) fault('PROGRAM-ERROR', 'Unrecognized keyword argument', { datum: unknown });
  }
  return result;
}

/** Execution state belongs to one task, including values and dynamic bindings. */
export class Task {
  constructor(runtime, callee, args = []) {
    this.runtime = runtime; this.frames = []; this.scopes = []; this.dynamic = new Map();
    this.values = []; this.done = false; this.suspended = null; this.failure = null;
    this.cancellationRequested = false; this.cancelRequest = null; this.cancelBoundaries = new Set();
    this.wakeSuspension = null;
    this.ops = operations; this.budget = Infinity; this.yielded = false;
    try { this.invoke(callee, args, null); } catch(error) { this.fail(error); }
  }
  get currentFrame() { return this.frames.at(-1); }
  get error() { return this.failure ? this.failure.error : null; }
  constant(frame, index) { return frame.constants[index]; }
  resolve(callee) { return this.runtime.resolveFunction(callee, this); }
  invoke(callee, args, continuation = null) {
    for(;;) {
    if(args.length>=65536) fault('PROGRAM-ERROR','CALL-ARGUMENTS-LIMIT exceeded');
    callee = this.resolve(callee);
    if (callee instanceof Closure || callee instanceof LispCode) {
      const closure = callee instanceof Closure ? callee : new Closure(callee, []);
      const code = closure.code.entry;
      const arity = code.arity ?? { required: 0, maximum: null };
      if (args.length < arity.required || (arity.maximum != null && args.length > arity.maximum)) fault('PROGRAM-ERROR', `Wrong number of arguments to ${code.name ?? 'lambda'}`);
      const registers = Array(code.registers ?? args.length).fill(NIL);
      for (let i = 0; i < arity.required; i++) registers[i] = args[i];
      this.frames.push({ closure, code, constants: closure.code.constants ?? code.constants ?? [], captures: closure.environment ?? [], registers, arguments: args.slice(arity.required), keywords: [], pc: 0, sourcePc: 0, continuation, scopeBase: this.scopes.length });
      return;
    }
    if (!(callee instanceof NativeFunction) && typeof callee?.invoke !== 'function') fault('TYPE-ERROR', 'Expected a function', { datum: callee });
    const origin=this.currentFrame,activation=origin?.resumeEpoch;
    let result;
    const previousTask=this.runtime.activeTask; this.runtime.activeTask=this;
    try { result = callee.invoke(args, this.runtime, this); } catch(error) { if(error instanceof ControlTransfer) return; throw error; }
    finally { this.runtime.activeTask=previousTask; }
    this.deliverCancellation();
    if (this.done || this.currentFrame !== origin || origin?.resumeEpoch !== activation) {
      // An abort inside the host callback can discard the call before it returns its Promise.
      if (result !== undefined) Promise.resolve(result).catch(() => {});
      return;
    }
    if(result instanceof CallRequest) { callee=result.callee;args=result.args;continue; }
    if(result&&typeof result.next==='function'&&typeof result.return==='function'&&typeof result[Symbol.iterator]==='function') {
      const frame={native:true,iterator:result,continuation,pending:{method:'next',value:undefined},scopeBase:this.scopes.length};
      this.frames.push(frame);this.pushScope(frame,'native');return;
    }
    if (result && typeof result.then === 'function') {
      const pending = Promise.resolve(result).then(value => {
        if (this.suspended !== pending) return;
        this.suspended = null;
        try { this.finishNative(value, continuation); } catch (error) { this.fail(error); }
      }, error => {
        if (this.suspended !== pending) return;
        this.suspended = null;
        this.fail(error);
      });
      this.suspended=pending;
      return;
    }
    this.finishNative(result, continuation);
    return;
    }
  }
  finishNative(result, continuation) {
    this.values = result instanceof MultipleValues ? result.values.slice() : [result === undefined ? NIL : result];
    if(this.values.length>=65536) fault('PROGRAM-ERROR','MULTIPLE-VALUES-LIMIT exceeded');
    this.finishContinuation(continuation);
  }
  finishContinuation(continuation) {
    if (continuation?.failure) { this.unwind(continuation.failure); return; }
    if(continuation?.native) continuation.native.pending={method:'next',value:new MultipleValues(this.values.slice())};
    if(continuation?.nested){continuation.completed=true;continuation.result=new MultipleValues(this.values.slice());}
    if (continuation?.out != null) { continuation.frame.registers[continuation.out] = primary(this.values); this.values = []; }
    if (!this.frames.length && !this.deliverCancellation()) this.done = true;
  }
  returnFrame(frame) {
    if (this.currentFrame !== frame) return;
    this.unwind({ kind: 'return', frame, values: this.values.slice() });
  }
  call(callee, args = []) {
    // Host callbacks can reenter Lisp while sharing this task's dynamic context.
    const boundary = this.frames.length, oldDone = this.done, completion={nested:true,completed:false};
    this.done = false; this.invoke(callee, args, completion);
    while (!completion.completed&&!this.done && !this.suspended && this.frames.length > boundary) this.tick();
    if (this.suspended) {
      this.suspended=null;
      fault('PROGRAM-ERROR','A synchronous host callback cannot suspend during Lisp reentry; suspend at the task call boundary');
    }
    if (this.failure) throw this.error;
    if (!completion.completed) throw new ControlTransfer();
    this.done = oldDone;
    return completion.result;
  }
  tickNative(frame) {
    this.budget--;
    const pending=frame.pending;frame.pending=null;
    if(!pending) throw new Error('Native continuation has no pending resume');
    const activation = frame.resumeEpoch;
    const result=frame.iterator[pending.method](pending.value);
    if (this.currentFrame !== frame || frame.resumeEpoch !== activation) return;
    if(!result||typeof result!=='object'||typeof result.done!=='boolean') throw new TypeError('Native continuation must be a synchronous generator');
    if(result.done) {
      if(frame.closing) {const cleanup=this.popScope('native-cleanup');this.frames.pop();this.unwind(cleanup.exit);}
      else {this.values=result.value instanceof MultipleValues?result.value.values.slice():[result.value===undefined?NIL:result.value];if(this.values.length>=65536)fault('PROGRAM-ERROR','MULTIPLE-VALUES-LIMIT exceeded');this.returnFrame(frame);}
      return;
    }
    if(!(result.value instanceof CallRequest)) fault('PROGRAM-ERROR','A native continuation must yield a CallRequest');
    this.invoke(result.value.callee,result.value.args,{native:frame});
  }
  tick() {
    if (this.done || this.suspended) return;
    const frame = this.currentFrame;
    if (!frame) { this.done = true; return; }
    const previousTask=this.runtime.activeTask;this.runtime.activeTask=this;
    try {
      if(frame.native){this.tickNative(frame);return;}
      if (frame.pc === 0 && this.budget === Infinity && typeof frame.code.fast === 'function') {
        frame.code.fast(this, frame);
        return;
      }
      if (typeof frame.code.step === 'function') { frame.code.step(this, frame); return; }
      if (frame.pc >= frame.code.instructions.length) { this.returnFrame(frame); return; }
      this.budget--;
      frame.sourcePc = frame.pc;
      const [op, operands] = variant(frame.code.instructions[frame.pc++]);
      const execute = operations[op];
      if (!execute) throw new TypeError(`Unknown instruction ${op}`);
      execute(this, frame, operands);
    } catch (error) { this.fail(error); } finally { this.runtime.activeTask=previousTask; }
  }
  fail(error) {
    if (this.done || error instanceof ControlTransfer) return;
    if (error instanceof SuspensionError) throw error;
    if (!(error instanceof LispCondition) && typeof error?.kind === 'string' && error.kind === error.kind.toUpperCase()) error = new LispCondition(error.kind,error.message,{cause:error,datum:error.operands?.[0],operands:error.operands});
    const exit = { kind: 'error', error, values: this.values.slice() };
    if (error instanceof LispCondition) {
      // A transfer that leaves dispatch also discards its failure continuation.
      const dispatch = new NativeFunction('DISPATCH-CONDITION', (_args, _runtime, task) => task.signal(error));
      this.invoke(dispatch, [], { failure: exit });
    } else {
      this.unwind(exit);
    }
  }
  run({ budget = Infinity } = {}) {
    if(budget!==Infinity&&(!Number.isSafeInteger(budget)||budget<0)) throw new TypeError('Task budget must be a nonnegative integer');
    this.budget = budget; this.yielded = false;
    while (!this.done) {
      this.deliverCancellation();
      if (this.done || this.suspended || this.yielded || this.budget <= 0) break;
      this.tick();
    }
    if (this.failure) throw this.error;
    return this;
  }
  async runAsync({ slice = 10000, signal } = {}) {
    if(!Number.isSafeInteger(slice)||slice<1) throw new TypeError('Task slice must be a positive integer');
    let budget = slice;
    const cancel = () => {
      if (!this.requestCancellation(signal.reason)) return;
      // Enter cleanup immediately. It may itself suspend on a new host call;
      // the abandoned body's Promise no longer controls this task.
      if (this.runtime.activeTask !== this && !this.suspended) {
        try { this.run({ budget: slice }); } catch (error) { this.failure ??= { error }; }
        budget = Math.max(0, this.budget);
      }
    };
    signal?.addEventListener('abort', cancel, { once: true });
    try {
      while (!this.done) {
        if (signal?.aborted) cancel();
        this.run({ budget });
        budget = Math.max(0, this.budget);
        const pending = this.suspended;
        if (pending) {
          // Lisp and host cancellation can both revoke this suspension.
          let wake;
          try {
            await new Promise((resolve, reject) => {
              this.wakeSuspension = wake = resolve;
              pending.then(resolve, reject);
            });
          } finally { if (this.wakeSuspension === wake) this.wakeSuspension = null; }
        }
        // Settled host Promises can resume in the same event-loop turn.
        if (!this.done && (budget === 0 || this.yielded)) {
          budget = slice;
          await new Promise(resolve => setTimeout(resolve, 0));
        }
      }
    } finally { signal?.removeEventListener('abort', cancel); }
    if (this.failure) throw this.error;
    return new MultipleValues(this.values.slice());
  }
  requestCancellation(error) {
    if (this.done || this.cancellationRequested) return false;
    this.cancellationRequested = true;
    this.cancelRequest = { kind: 'cancel', error };
    if (this.runtime.activeTask !== this) this.deliverCancellation();
    return true;
  }
  pushCancelBoundary({ deadline = false } = {}) {
    return this.pushScope(this.currentFrame, 'cancel-boundary', { active: true, triggered: false, deadline });
  }
  triggerCancelBoundary(scope) {
    if (!scope.active || scope.triggered) return false;
    scope.triggered = true;
    this.cancelBoundaries.add(scope);
    if (this.runtime.activeTask !== this) this.deliverCancellation();
    return true;
  }
  deliverCancellation() {
    if (!this.cancelRequest && !this.cancelBoundaries.size) return false;
    let destination, shielded = false;
    for (let i = this.scopes.length - 1; i >= 0; i--) {
      const scope = this.scopes[i];
      if (scope.kind === 'cancel-shield' || scope.kind === 'cleanup' || scope.kind === 'native-cleanup') {
        shielded = true;
        break;
      }
      if (scope.kind === 'cancel-boundary' && scope.triggered) destination = scope;
    }
    if (!shielded && this.cancelRequest) {
      const request = this.cancelRequest;
      this.cancelRequest = null;
      this.unwind({ ...request, values: [] });
    } else if (destination) {
      this.unwind({ kind: 'cancel', destination, values: [] });
    } else return false;
    return true;
  }
  pushScope(frame, kind, fields = {}) { const scope = { frame, kind, ...fields }; this.scopes.push(scope); return scope; }
  popScope(kind) { const scope = this.scopes.pop(); if (scope?.kind !== kind) throw new Error(`Unbalanced ${kind} scope`); this.dispose(scope); return scope; }
  bind(frame, bindings) {
    bindings.forEach(([name]) => this.runtime.checkMutableSymbol(name));
    for (const [name, value] of bindings) { let stack = this.dynamic.get(name); if (!stack) this.dynamic.set(name, stack = []); stack.push(value); }
    this.pushScope(frame, 'special', { bindings });
  }
  dispose(scope) {
    if (scope.kind === 'cancel-boundary') {
      scope.active = false;
      this.cancelBoundaries.delete(scope);
    }
    if (scope.kind === 'special') for (let i = scope.bindings.length - 1; i >= 0; i--) { const stack = this.dynamic.get(scope.bindings[i][0]); stack.pop(); if (!stack.length) this.dynamic.delete(scope.bindings[i][0]); }
  }
  resume(frame, pc, clear = frame.registers.length) {
    this.frames.length = this.frames.indexOf(frame) + 1;
    frame.resumeEpoch=(frame.resumeEpoch??0)+1;
    frame.registers.fill(NIL, clear); frame.pc = pc;
  }
  transfer(kind, value, label) {
    const destination = this.scopes.findLast(scope => scope.kind === 'target' && scope.targetKind === kind && eq(scope.value, value));
    if (!destination) fault('CONTROL-ERROR', `No active ${kind} target`, { datum: value });
    this.unwind({ kind: 'target', destination, label, values: this.values.slice() });
  }
  unwind(exit) {
    if (exit.kind !== 'return' && this.suspended) {
      this.suspended = null;
      this.wakeSuspension?.();
    }
    this.values = exit.values;
    while (this.scopes.length) {
      const scope = this.scopes.at(-1);
      if (exit.kind === 'return' && scope.frame !== exit.frame) break;
      this.scopes.pop();
      if (scope === exit.destination) {
        if (scope.kind === 'cancel-boundary') {
          this.dispose(scope);
          const frame = scope.frame;
          this.frames.length = this.frames.indexOf(frame) + 1;
          frame.resumeEpoch = (frame.resumeEpoch ?? 0) + 1;
          frame.pending = { method: 'next', value: CANCELLED_BOUNDARY };
          return;
        }
        if (exit.kind === 'native-restart') {
          const frame = scope.frame;
          this.frames.length = this.frames.indexOf(frame) + 1;
          frame.resumeEpoch = (frame.resumeEpoch ?? 0) + 1;
          frame.pending = { method: 'return', value: new MultipleValues(exit.values) };
          return;
        }
        this.resume(scope.frame, exit.pc ?? (exit.label == null ? scope.resume : scope.frame.code.jump_tables[scope.jumps].targets[exit.label]), exit.clear ?? scope.clear);
        if (exit.label != null) this.scopes.push(scope);
        if (exit.conditionRegister != null) scope.frame.registers[exit.conditionRegister] = primary(this.values);
        return;
      }
      if (scope.kind === 'protect') {
        this.resume(scope.frame, scope.cleanup, scope.clear);
        this.pushScope(scope.frame, 'cleanup', { saved: this.values.slice(), exit }); this.values = []; return;
      }
      if((scope.kind==='native'||scope.kind==='native-cleanup')&&!(exit.kind==='return'&&scope.frame===exit.frame)) {
        const frame=scope.frame;this.frames.length=this.frames.indexOf(frame)+1;
        frame.resumeEpoch=(frame.resumeEpoch??0)+1;
        frame.closing=true;frame.pending={method:'return',value:undefined};
        this.pushScope(frame,'native-cleanup',{exit});this.values=[];return;
      }
      this.dispose(scope);
    }
    if (exit.kind === 'return') {
      if (this.currentFrame !== exit.frame) throw new Error('Invalid return activation');
      this.frames.pop(); this.finishContinuation(exit.frame.continuation); return;
    }
    this.frames.length = 0; this.done = true;
    if (exit.kind === 'error' || exit.kind === 'cancel') { this.failure = exit; return; }
    throw new Error('Control destination left its extent');
  }
  *signal(condition) {
    const clusters = this.scopes.filter(scope => scope.kind === 'handlers' && !scope.inactive).reverse();
    for (const cluster of clusters) {
      for (const handler of cluster.handlers) {
        const matched = this.runtime.conditionClassMatches(condition, handler.test, cluster.frame.constants);
        if (matched === false) continue;
        const disabled = this.scopes.slice(this.scopes.indexOf(cluster)).filter(s => s.kind === 'handlers').map(s=>[s,s.inactive]);
        disabled.forEach(([s]) => { s.inactive = true; });
        try {
          if (matched === undefined) {
            const typep = this.runtime.intern('TYPEP', 'COMMON-LISP');
            if (primary((yield new CallRequest(typep, [condition, cluster.frame.constants[handler.test.Named]])).values) === NIL) continue;
          }
          const [kind, target] = variant(handler.target);
          if (kind === 'Case') { this.unwind({ kind: 'handler', destination: cluster, pc: target.pc, clear: target.clear, conditionRegister: target.condition, values: [condition] }); return NIL; }
          yield new CallRequest(cluster.frame.registers[target], [condition]);
        } finally { disabled.forEach(([s,inactive]) => { s.inactive = inactive; }); }
      }
    }
    return NIL;
  }
  associateSignalingCall(condition) {
    const frame = this.currentFrame, scope = frame.continuation?.restartScope;
    if (scope) this.pushScope(frame, 'association', { condition, restarts: scope.bindings.map(b => b.object) });
  }
  enterNativeRestart(condition, name, report) {
    const frame = this.currentFrame;
    const object = new LispRestart(name, report);
    this.pushScope(frame, 'restarts', { bindings: [{ object, target: 'NativeReturn', test: null, interactive: null }] });
    this.pushScope(frame, 'association', { condition, restarts: [object] });
    return object;
  }
  *restartSteps(condition=NIL) {
    const result=[];
    for(const scope of this.scopes.toReversed())if(scope.kind==='restarts')for(const binding of scope.bindings){
      const associations=this.scopes.filter(s=>s.kind==='association'&&s.restarts.includes(binding.object));
      if(condition!==NIL&&associations.length&&!associations.some(s=>s.condition===condition))continue;
      if(binding.test!=null&&primary((yield new CallRequest(binding.test,[condition])).values)===NIL)continue;
      result.push(binding.object);
    }
    return result;
  }
  *invokeRestart(designator, args = [], interactive = false) {
    const object = designator instanceof LispRestart ? designator : (yield* this.restartSteps(NIL)).find(r => r.name === designator);
    const scope = this.scopes.findLast(s => s.kind === 'restarts' && s.bindings.some(b => b.object === object));
    const binding = scope?.bindings.find(b => b.object === object);
    if (!binding) fault('CONTROL-ERROR', 'Restart is not active');
    if (interactive && binding.interactive != null) args = toArray(primary((yield new CallRequest(binding.interactive)).values));
    const [kind, target] = variant(binding.target);
    if (kind === 'Function') return yield new CallRequest(scope.frame.registers[target], args);
    if (kind === 'NativeReturn') {
      if (args.length) fault('PROGRAM-ERROR', 'Wrong number of arguments to restart');
      this.unwind({ kind: 'native-restart', destination: scope, values: [NIL] });
      return NIL;
    }
    this.unwind({ kind: 'restart', destination: scope, pc: target.pc, clear: target.clear, values: args.slice() });
    return new MultipleValues(this.values.slice());
  }
}

export const operations = {
  Load(t, f, [out, value]) { f.registers[out] = immediate(value); },
  Constant(t, f, [out, index]) { f.registers[out] = f.constants[index]; },
  Copy(t, f, [out, src]) { f.registers[out] = f.registers[src]; },
  Clear(t, f, r) { const [a,b] = range(r); f.registers.fill(NIL,a,b); },
  Jump(t, f, pc) { f.pc = pc; },
  Unless(t, f, [reg, pc]) { if (f.registers[reg] === NIL) f.pc = pc; },
  IfPresent(t, f, [reg, pc]) { if (f.registers[reg] !== UNBOUND) f.pc = pc; },
  UnlessFixnum(t, f, [reg, pc]) { if (!fixnum(f.registers[reg])) f.pc = pc; },
  UnlessFixnumInRange(t, f, [reg, low, high, pc]) { const n = f.registers[reg]; if (!fixnum(n) || n < low || n > high) f.pc = pc; },
  TryFixnumArithmetic(t, f, { op, symbol: index, out, args }) {
    const fn = t.runtime.readFunction(f.constants[index]); const values = selected(f, args);
    const expected = { Add: '+', Sub: '-', Mul: '*', Inc: '1+', Dec: '1-' }[op];
    if (fn !== t.runtime.primitives.get(expected) || !values.every(fixnum)) { f.registers[out] = NIL; return; }
    const [a,b] = values; const n = op === 'Add' ? a+b : op === 'Sub' ? a-b : op === 'Mul' ? a*b : op === 'Inc' ? a+1 : a-1;
    f.registers[out] = fixnum(n) ? n : NIL;
  },
  SetValues(t, f, r) { t.values = selected(f,r); if(t.values.length>=65536) fault('PROGRAM-ERROR','MULTIPLE-VALUES-LIMIT exceeded'); },
  BindValues(t, f, r) { const [a,b]=range(r); for(let i=a;i<b;i++) f.registers[i] = t.values[i-a] ?? NIL; t.values=[]; },
  ReadValue(t, f, [out,index]) { f.registers[out] = t.values[index] ?? NIL; },
  SaveValues(t,f) { t.pushScope(f,'values',{ saved:t.values }); t.values=[]; },
  AppendValues(t) { const s=t.scopes.at(-1); if(s?.kind!=='values') throw new Error('No saved values'); if(s.saved.length+t.values.length>=65536) fault('PROGRAM-ERROR','MULTIPLE-VALUES-LIMIT exceeded'); s.saved.push(...t.values); t.values=[]; },
  RestoreValues(t) { t.values=t.popScope('values').saved; },
  ReadCell(t,f,{out,symbol:index,function:isFunction}) { f.registers[out]=isFunction?t.runtime.readFunction(f.constants[index]):t.runtime.readVariable(f.constants[index],t); },
  WriteCell(t,f,{symbol:index,src}) { t.runtime.writeVariable(f.constants[index],f.registers[src],t); },
  DefineFunction(t,f,{symbol:index,definition,is_macro}) { t.runtime.defineFunction(f.constants[index],f.registers[definition],is_macro); },
  ProclaimSpecial(t,f,{symbol:index}) { symbol(f.constants[index]).special=true; },
  ProclaimInline(t,f,{symbol:index,setf,notinline}) { const s=symbol(f.constants[index]); (s.inline ??= new Map()).set(Boolean(setf), !notinline); },
  SetVariableDocumentation(t,f,{symbol:index,doc}) { symbol(f.constants[index]).documentation=f.constants[doc]; },
  WhenBound(t,f,{symbol:index,jump}) { if(t.runtime.variableValue(f.constants[index],t)!==UNBOUND) f.pc=jump; },
  BindSpecial(t,f,{symbol:index,value}) { t.bind(f,[[symbol(f.constants[index]),f.registers[value]]]); },
  BindProgv(t,f,{symbols,values}) { const names=toArray(f.registers[symbols]), vals=toArray(f.registers[values]); t.bind(f,names.map((s,i)=>[symbol(s),vals[i]??UNBOUND])); },
  Unbind(t,f,count) { for(let i=0;i<count;i++) t.popScope('special'); },
  BoxLocal(t,f,{reg}) { f.registers[reg]=new Cell(f.registers[reg]); },
  ReadBox(t,f,{out,cell:location}) { f.registers[out]=cell(f,location).value; },
  WriteBox(t,f,{cell:location,src}) { cell(f,location).value=f.registers[src]; },
  Capture(t,f,[out,index]) { f.registers[out]=f.captures[index]; },
  Closure(t,f,{out,code,cells}) { const c=f.constants[code]; if(!(c instanceof LispCode)) fault('PROGRAM-ERROR','Closure requires function code'); f.registers[out]=new Closure(c,selected(f,cells)); },
  ExitToken(t,f,[out]) { f.registers[out]=new ExitToken(); },
  EnterBlock(t,f,{token,resume,clear}) { const value=f.registers[token]; if(!(value instanceof ExitToken)) fault('PROGRAM-ERROR','Expected exit token'); t.pushScope(f,'target',{targetKind:'block',value,resume,clear}); },
  EnterCatch(t,f,{tag,resume,clear}) { t.pushScope(f,'target',{targetKind:'catch',value:f.registers[tag],resume,clear}); },
  EnterTagbody(t,f,{token,jumps,clear}) { const value=f.registers[token]; if(!(value instanceof ExitToken)) fault('PROGRAM-ERROR','Expected exit token'); t.pushScope(f,'target',{targetKind:'tagbody',value,jumps,clear}); },
  LeaveTarget(t) { t.popScope('target'); },
  EnterProtect(t,f,{cleanup,clear}) { t.pushScope(f,'protect',{cleanup,clear}); },
  BeginCleanup(t,f) { const scope=t.popScope('protect'); t.pushScope(f,'cleanup',{saved:t.values.slice(),exit:null}); t.values=[]; f.pc=scope.cleanup; },
  EndCleanup(t) { const scope=t.popScope('cleanup'); t.values=scope.saved; if(scope.exit) t.unwind({...scope.exit,values:scope.saved}); },
  ReturnFrom(t,f,{token}) { t.transfer('block',f.registers[token]); },
  Throw(t,f,{tag}) { t.transfer('catch',f.registers[tag]); },
  Go(t,f,{token,label}) { t.transfer('tagbody',f.registers[token],label); },
  EnterHandlers(t,f,{table}) { t.pushScope(f,'handlers',{handlers:f.code.handlers[table].handlers}); },
  LeaveHandlers(t) { t.popScope('handlers'); },
  EnterRestarts(t,f,{table}) {
    const bindings=f.code.restarts[table].restarts.map(s=>{ const object=new LispRestart(f.constants[s.name],s.report==null?NIL:f.registers[s.report]); f.registers[s.object]=object; return {...s,object,test:s.test==null?null:f.registers[s.test],interactive:s.interactive==null?null:f.registers[s.interactive]}; });
    t.pushScope(f,'restarts',{bindings});
  },
  LeaveRestarts(t) { t.popScope('restarts'); },
  AssociateRestarts(t,f,{condition,list}) { t.pushScope(f,'association',{condition:f.registers[condition],restarts:toArray(f.registers[list])}); },
  LeaveRestartAssociation(t) { t.popScope('association'); },
  OptionalArgument(t,f,[index,out]) { f.registers[out]=f.arguments[index]??UNBOUND; },
  KeywordArgument(t,f,[index,out]) { if(index>=f.keywords.length) fault('PROGRAM-ERROR','Keyword argument index out of bounds'); f.registers[out]=f.keywords[index]; },
  RestArguments(t,f,{start,out}) { f.registers[out]=fromArray(f.arguments.slice(start)); },
  CheckKeywords(t,f,{start,table}) { f.keywords=keywordArguments(f.arguments,f.code.keywords[table],f.constants,start); },
  DestructureNext(t,f,{cursor,out,required,macro_call}) {
    const value=f.registers[cursor]; if(value instanceof Cons) { f.registers[out]=value.car; f.registers[cursor]=value.cdr; }
    else if(value===NIL&&!required) f.registers[out]=UNBOUND;
    else fault(macro_call?'PROGRAM-ERROR':'TYPE-ERROR','Invalid destructuring argument',{datum:value});
  },
  DestructureEnd(t,f,{cursor,macro_call}) { if(f.registers[cursor]!==NIL) fault(macro_call?'PROGRAM-ERROR':'TYPE-ERROR','Unexpected destructuring tail',{datum:f.registers[cursor]}); },
  DestructureKeywords(t,f,{cursor,table,out}) { const values=keywordArguments(toArray(f.registers[cursor]),f.code.keywords[table],f.constants,0); const [a,b]=range(out); for(let i=a;i<b;i++) f.registers[i]=values[i-a]; },
  ReleaseArguments(t,f) { f.arguments=[]; f.keywords=[]; },
  CallValues(t,f,{callee}) { const args=t.values; t.values=[]; t.invoke(f.registers[callee],args,{frame:f,out:null}); },
  Call(t,f,{callee,out,args,associate}) { const [kind,index]=variant(callee); const target=kind==='Named'?t.runtime.readFunction(f.constants[index]):f.registers[index]; const values=selected(f,args); const restartScope=associate?t.scopes.findLast(s=>s.kind==='restarts'&&s.frame===f):null; t.values=[]; t.invoke(target,values,{frame:f,out,restartScope}); },
};
