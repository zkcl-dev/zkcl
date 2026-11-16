import { NIL, UNBOUND, Closure, LispCode, LispString, fromArray } from './objects.mjs';
import { LispCondition, NativeFunction } from './execution.mjs';
import { arity } from './builtins_helpers.mjs';
import { requestedTypep } from './types.mjs';

export function installDebugger(rt, initialDebugger) {
  const cl = name => rt.intern(name, 'COMMON-LISP');
  const hook = cl('*DEBUGGER-HOOK*'), breakOn = cl('*BREAK-ON-SIGNALS*');
  for (const symbol of [hook, breakOn]) {
    symbol.value = NIL;
    symbol.special = true;
    rt.exportSymbol(symbol, 'COMMON-LISP');
  }
  const value = (symbol, task) => {
    const current = rt.variableValue(symbol, task);
    return current === UNBOUND ? NIL : current;
  };
  let frontend = NIL;
  // An embedding may install a Lisp function or a JavaScript callback. The
  // callback receives the condition, runtime, and active task, in that order.
  rt.setDebugger = (debuggerFunction = NIL) => {
    if (debuggerFunction == null || debuggerFunction === NIL) frontend = NIL;
    else if (typeof debuggerFunction === 'function') {
      frontend = new NativeFunction('HOST-DEBUGGER', ([condition], runtime, task) => debuggerFunction(condition, runtime, task));
    } else if (debuggerFunction instanceof Closure || debuggerFunction instanceof LispCode || typeof debuggerFunction.invoke === 'function') {
      frontend = debuggerFunction;
    } else throw new TypeError('Expected a debugger function');
  };
  rt.setDebugger(initialDebugger);

  const runHook = new NativeFunction('RUN-DEBUGGER-HOOK', function* ([condition, previous], _rt, task) {
    task.bind(task.currentFrame, [[hook, NIL]]);
    yield rt.requestCall(previous, [condition, previous]);
  });
  rt.enterDebuggerSteps = function* (condition, task) {
    const standard = frontend, previous = value(hook, task);
    if (previous !== NIL) yield rt.requestCall(runHook, [condition, previous]);
    if (standard !== NIL) yield rt.requestCall(standard, [condition]);
  };

  const conditionType = cl('CONDITION'), continueName = cl('CONTINUE');
  const beforeHandlers = new NativeFunction('BREAK-ON-SIGNALS', function* ([condition, type], _rt, task) {
    task.bind(task.currentFrame, [[breakOn, NIL]]);
    if (yield* requestedTypep(condition, type, rt)) {
      task.enterNativeRestart(condition, continueName, new LispString('Continue signaling.'));
      yield* task.invokeDebugger(condition);
    }
    return NIL;
  });
  rt.signalBreakRequest = (condition, task) => {
    const type = value(breakOn, task);
    return type === NIL ? null : rt.requestCall(beforeHandlers, [condition, type]);
  };

  rt.defineBuiltin('INVOKE-DEBUGGER', (args, _rt, task) => {
    arity(args, 1);
    const [condition] = args;
    if (!(condition instanceof LispCondition)) throw new LispCondition('TYPE-ERROR', 'INVOKE-DEBUGGER expects a condition', {
      datum: condition, expectedType: conditionType,
    });
    return task.invokeDebugger(condition);
  }, { primitive: 'INVOKE-DEBUGGER' });
  const simpleCondition = cl('SIMPLE-CONDITION');
  const formatControl = rt.intern('FORMAT-CONTROL', 'KEYWORD'), formatArguments = rt.intern('FORMAT-ARGUMENTS', 'KEYWORD');
  const defaultControl = new LispString('Break'), returnReport = new LispString('Return from BREAK.');
  rt.defineBuiltin('BREAK', function* ([control = defaultControl, ...args], _rt, task) {
    task.bind(task.currentFrame, [[hook, NIL]]);
    const condition = yield* rt.makeConditionSteps(simpleCondition, [formatControl, control, formatArguments, fromArray(args)], task);
    task.enterNativeRestart(condition, continueName, returnReport);
    return yield* task.invokeDebugger(condition);
  }, { primitive: 'BREAK' });
}
