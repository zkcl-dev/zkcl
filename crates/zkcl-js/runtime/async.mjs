import { NIL, T, LispString, LispSymbol, toArray } from './objects.mjs';
import { isReal, toNumber } from './numbers.mjs';
import { CANCELLED_BOUNDARY, CallRequest, LispCondition, MultipleValues, NativeFunction } from './execution.mjs';

const bool = value => value ? T : NIL;
const taskCancellation = new Error('Task cancelled');
const wait = new NativeFunction('WAIT-FOR-ASYNC-OPERATION', ([pending]) => pending);

class Completion {
  constructor() { this.listeners = new Set(); this.outcome = null; }
  settle(outcome) {
    if (this.outcome) return false;
    this.outcome = outcome;
    for (const notify of this.listeners) notify(outcome);
    this.listeners.clear();
    return true;
  }
}

class TaskHandle {
  constructor(task, name, child) {
    this.task = task; this.name = name; this.child = child;
    this.completion = new Completion(); this.groups = new Set(); this.dependencies = null;
  }
  get status() {
    if (!this.task.done) return 'running';
    return this.task.failure?.kind === 'cancel' ? 'cancelled' : this.task.failure ? 'failed' : 'completed';
  }
  cancel() { return this.task.requestCancellation(taskCancellation); }
}

class TaskGroup {
  constructor(owner, boundary) {
    this.owner = owner; this.boundary = boundary; this.members = new Map();
    this.phase = 'open'; this.failure = null; this.join = new Completion();
  }
  finish(cancelAll) {
    if (this.phase === 'closed' || this.phase === 'cancelling' || (this.phase === 'joining' && !cancelAll)) return false;
    this.phase = cancelAll ? 'cancelling' : 'joining';
    for (const [child, noWait] of this.members) if (cancelAll || noWait) child.cancel();
    if (!this.members.size) this.join.settle({});
    return true;
  }
  childFinished(child, allowFailure) {
    this.members.delete(child);
    if (child.status === 'failed' && !allowFailure && !this.failure) {
      this.failure = child.task.failure;
      this.finish(true);
      this.owner.task.triggerCancelBoundary(this.boundary);
    }
    if (this.phase !== 'open' && !this.members.size) this.join.settle({});
  }
}

class CancelScope {
  constructor(task, boundary) { this.task = task; this.boundary = boundary; }
}

class AsyncPromise extends Completion {}

class AsyncEvent {
  constructor(runtime, signaled) { this.runtime = runtime; this.signaled = signaled; this.waiters = new Set(); }
}

// Register a single operation on each source and remove it when cancellation
// abandons the call. A promise held by the JS host cannot elect stale waiters.
function* awaitNotification(sources) {
  let notify;
  const pending = new Promise(resolve => { notify = resolve; });
  for (const listeners of sources) listeners.add(notify);
  try { return (yield new CallRequest(wait, [pending])).values[0]; }
  finally { for (const listeners of sources) listeners.delete(notify); }
}

function localDeadline(task) {
  for (const scope of task.scopes.toReversed()) {
    if (['cancel-shield', 'cleanup', 'native-cleanup'].includes(scope.kind)) break;
    if (scope.deadline || scope.triggered) return true;
  }
  return false;
}

// Task waits may be alternatives. A group owner's implicit joins still
// require every non-cancelled child, even before it reaches its cleanup.
function canFinish(targets) {
  const nodes = new Map(), todo = [...targets], ready = [];
  for (const handle of todo) {
    if (nodes.has(handle)) continue;
    const externallyReady = handle.status !== 'running' || localDeadline(handle.task);
    const required = [], alternatives = externallyReady ? [] : handle.dependencies ?? [];
    if (!externallyReady) for (const group of handle.groups) {
      for (const [member, noWait] of group.members) {
        if (group.phase !== 'cancelling' && !noWait && !member.task.cancellationRequested) required.push(member);
      }
    }
    const node = { handle, required, alternatives, needs: required.length, alternative: alternatives.length > 0, users: [] };
    nodes.set(handle, node);
    for (const handle of required) todo.push(handle);
    for (const handle of alternatives) todo.push(handle);
  }
  for (const node of nodes.values()) {
    for (const dep of node.required) nodes.get(dep).users.push([node, true]);
    for (const dep of node.alternatives) nodes.get(dep).users.push([node, false]);
    if (!node.needs && !node.alternative) ready.push(node);
  }
  const wanted = new Set(targets);
  for (const node of ready) {
    if (wanted.has(node.handle)) return true;
    for (const [user, required] of node.users) {
      if (!user.needs && !user.alternative) continue;
      if (required) user.needs--;
      else user.alternative = false;
      if (!user.needs && !user.alternative) ready.push(user);
    }
  }
  return false;
}

function duration(value) {
  if (!isReal(value) || toNumber(value) < 0) throw new LispCondition('TYPE-ERROR', 'Expected nonnegative seconds', { datum: value });
  const milliseconds = toNumber(value) * 1000;
  if (!Number.isFinite(milliseconds) || milliseconds > Number.MAX_SAFE_INTEGER) throw new LispCondition('CONTROL-ERROR', 'Duration exceeds the host clock range');
  return milliseconds;
}

function schedule(milliseconds, notify) {
  const now = () => typeof performance !== 'undefined' ? performance.now() : Date.now();
  const stop = now() + milliseconds;
  let timer;
  const check = () => {
    const remaining = stop - now();
    if (remaining <= 0) notify();
    else timer = setTimeout(check, Math.min(2_147_483_647, Math.ceil(remaining)));
  };
  timer = setTimeout(check, Math.min(2_147_483_647, Math.ceil(milliseconds)));
  return () => clearTimeout(timer);
}

export function installAsync(rt) {
  const handles = new WeakMap();
  const zk = rt.findPackage('ZK'), pkg = rt.makePackage('ZK-ASYNC', { use: ['COMMON-LISP'] });
  const as = name => rt.intern(name, pkg);
  const cl = name => rt.intern(name, 'COMMON-LISP');
  const keyword = name => rt.intern(name, 'KEYWORD');
  const current = task => {
    if (!handles.has(task)) handles.set(task, new TaskHandle(task, new LispString('main'), false));
    return handles.get(task);
  };
  const owns = (type, value) => value instanceof type &&
    (type === TaskGroup ? value.owner.task.runtime : value.task?.runtime ?? value.runtime) === rt;
  const expect = (type, value, name) => {
    if (owns(type, value)) return value;
    throw new LispCondition('TYPE-ERROR', `Expected a ${name}`, { datum: value, expectedType: as(name) });
  };
  const getTask = value => expect(TaskHandle, value, 'TASK');
  const getGroup = value => expect(TaskGroup, value, 'TASK-GROUP');
  const getScope = value => expect(CancelScope, value, 'CANCEL-SCOPE');
  const getPromise = value => expect(AsyncPromise, value, 'PROMISE');
  const getEvent = value => expect(AsyncEvent, value, 'EVENT');
  const error = message => new CallRequest(cl('ERROR'), [as('ASYNC-ERROR'), keyword('MESSAGE'), new LispString(message)]);
  const raise = value => value instanceof LispCondition ? new CallRequest(cl('ERROR'), [value]) : error(value?.message ?? String(value));

  const primitive = (name, min, max, body) => {
    // The function keeps its generator identity when the VM scheduler parks it.
    const qualified = `ZK::%${name}`;
    const fn = new NativeFunction(qualified, (args, runtime, task) => {
      if (args.length < min || args.length > max) throw new LispCondition('PROGRAM-ERROR', `Wrong number of arguments to ${qualified}`);
      return body(args, runtime, task);
    });
    rt.defineFunction(rt.intern(`%${name}`, zk), fn);
    rt.primitives.set(qualified, fn);
  };

  primitive('CALL-WITH-TASK-GROUP', 1, 1, function* ([body], _r, task) {
    const owner = current(task), boundary = task.pushCancelBoundary(), group = new TaskGroup(owner, boundary);
    owner.groups.add(group);
    let result;
    try { result = yield new CallRequest(body, [group]); }
    finally {
      if (boundary.active) task.popScope('cancel-boundary');
      group.finish(result === undefined || result === CANCELLED_BOUNDARY);
      try { if (!group.join.outcome) yield* awaitNotification([group.join.listeners]); }
      finally { group.phase = 'closed'; owner.groups.delete(group); }
    }
    if (group.failure) return yield raise(group.failure.error);
    return result;
  });
  primitive('FINISH-TASK-GROUP', 2, 2, function* ([value, unwinding], _r, task) {
    const group = getGroup(value);
    if (group.owner.task !== task) throw new LispCondition('CONTROL-ERROR', 'Only the owner can finish a task group');
    group.finish(unwinding !== NIL);
    if (!group.join.outcome) yield* awaitNotification([group.join.listeners]);
    return NIL;
  });
  primitive('SPAWN', 5, 5, ([value, body, noWait, allowFailure, name], runtime, task) => {
    const group = getGroup(value);
    if (group.phase !== 'open') return error('Task group is closed');
    if (name !== NIL && !(name instanceof LispString || name instanceof LispSymbol)) {
      throw new LispCondition('TYPE-ERROR', 'Task name must be a string or symbol', { datum: name });
    }
    const target = runtime.resolveFunction(body);
    const specials = Array.from(task.dynamic, ([symbol, stack]) => [symbol, stack.at(-1)]);
    const runner = new NativeFunction('START-ASYNC-TASK', function* (_args, _runtime, child) {
      child.bind(child.currentFrame, specials);
      return yield new CallRequest(target);
    });
    const child = new TaskHandle(runtime.createTask(runner), name instanceof LispSymbol ? new LispString(name.name) : name, true);
    handles.set(child.task, child);
    group.members.set(child, noWait !== NIL);
    queueMicrotask(async () => {
      try { await child.task.runAsync(); } catch { /* The task owns its terminal outcome. */ }
      group.childFinished(child, allowFailure !== NIL);
      child.completion.settle(child);
    });
    return child;
  });
  const taskResult = handle => handle.status === 'failed' ? raise(handle.task.error) :
    handle.status === 'cancelled' ? new CallRequest(cl('ERROR'), [as('WAITED-TASK-CANCELLED'), keyword('TASK'), handle]) :
    new MultipleValues(handle.task.values.slice());
  function* awaitTasks(task, targets) {
    const done = targets.find(handle => handle.status !== 'running');
    if (done) return done;
    const owner = current(task);
    owner.dependencies = targets;
    try {
      if (!localDeadline(task) && !canFinish(targets)) return null;
      return yield* awaitNotification(targets.map(handle => handle.completion.listeners));
    } finally { owner.dependencies = null; }
  }
  primitive('AWAIT', 1, 1, function* ([value], _r, task) {
    const result = yield* awaitTasks(task, [getTask(value)]);
    const outcome = result ? taskResult(result) : error('Waiting would create a task dependency cycle');
    return outcome instanceof CallRequest ? yield outcome : outcome;
  });
  primitive('AWAIT-ANY', 1, 1, function* ([list], _r, task) {
    let targets;
    try { targets = toArray(list).map(getTask); }
    catch (cause) {
      if (cause instanceof LispCondition) throw cause;
      throw new LispCondition('TYPE-ERROR', 'Expected a proper list of tasks', { datum: list, expectedType: cl('LIST') });
    }
    if (!targets.length) return yield error('AWAIT-ANY requires at least one task');
    const result = yield* awaitTasks(task, targets);
    return result ?? (yield error('Waiting would create a task dependency cycle'));
  });
  primitive('TASKP', 1, 1, ([value]) => bool(owns(TaskHandle, value)));
  primitive('TASK-NAME', 1, 1, ([value]) => getTask(value).name);
  primitive('TASK-DONE-P', 1, 1, ([value]) => bool(getTask(value).status !== 'running'));
  primitive('TASK-RUNNING-P', 1, 1, ([value]) => bool(getTask(value).status === 'running'));
  primitive('TASK-COMPLETED-P', 1, 1, ([value]) => bool(getTask(value).status === 'completed'));
  primitive('TASK-FAILED-P', 1, 1, ([value]) => bool(getTask(value).status === 'failed'));
  primitive('TASK-CANCELLED-P', 0, 1, ([value = NIL], _r, task) =>
    bool(value === NIL ? task.cancellationRequested || task.cancelBoundaries.size : getTask(value).task.cancellationRequested));
  primitive('CANCEL-TASK', 1, 1, ([value]) => bool(getTask(value).cancel()));
  primitive('CURRENT-TASK', 0, 0, (_a, _r, task) => current(task));
  primitive('IN-SCHEDULER-P', 0, 0, () => T);
  primitive('CHILD-TASK-P', 0, 0, (_a, _r, task) => bool(current(task).child));
  primitive('YIELD', 0, 0, () => new Promise(resolve => setTimeout(() => resolve(NIL), 0)));
  primitive('TASK-GROUP-P', 1, 1, ([value]) => bool(owns(TaskGroup, value)));
  primitive('TASK-GROUP-OPEN-P', 1, 1, ([value]) => bool(getGroup(value).phase === 'open'));
  primitive('TASK-GROUP-CLOSED-P', 1, 1, ([value]) => bool(getGroup(value).phase !== 'open'));
  primitive('TASK-GROUP-CANCELLED-P', 1, 1, ([value]) => bool(getGroup(value).phase === 'cancelling'));
  primitive('CANCEL-TASK-GROUP', 1, 1, ([value]) => bool(getGroup(value).finish(true)));
  primitive('CANCEL-SCOPE-P', 1, 1, ([value]) => bool(owns(CancelScope, value)));
  primitive('CANCEL-SCOPE', 1, 1, ([value]) => {
    const scope = getScope(value);
    return bool(scope.task.triggerCancelBoundary(scope.boundary));
  });
  primitive('CANCEL-SCOPE-CANCELLED-P', 1, 1, ([value]) => bool(getScope(value).boundary.triggered));
  primitive('PROTECT-FROM-CANCEL', 1, 1, function* ([body], _r, task) {
    task.pushScope(task.currentFrame, 'cancel-shield');
    return yield new CallRequest(body);
  });
  primitive('WITH-CANCEL-SCOPE', 2, 2, function* ([body, fallback], _r, task) {
    const boundary = task.pushCancelBoundary();
    const result = yield new CallRequest(body, [new CancelScope(task, boundary)]);
    if (result !== CANCELLED_BOUNDARY) return result;
    return fallback === NIL ? NIL : yield new CallRequest(fallback);
  });
  primitive('WITH-TIMEOUT', 3, 3, function* ([seconds, body, fallback], _r, task) {
    const milliseconds = duration(seconds);
    if (milliseconds === 0) return yield new CallRequest(fallback);
    const boundary = task.pushCancelBoundary({ deadline: true });
    const clear = schedule(milliseconds, () => task.triggerCancelBoundary(boundary));
    let result;
    try { result = yield new CallRequest(body); }
    finally { clear(); }
    return result === CANCELLED_BOUNDARY ? yield new CallRequest(fallback) : result;
  });

  primitive('MAKE-PROMISE', 0, 0, () => Object.assign(new AsyncPromise(), { runtime: rt }));
  primitive('PROMISEP', 1, 1, ([value]) => bool(owns(AsyncPromise, value)));
  primitive('PROMISE-DONE-P', 1, 1, ([value]) => bool(getPromise(value).outcome));
  primitive('FULFILL-PROMISE', 1, Infinity, ([value, ...values]) => bool(getPromise(value).settle({ values: new MultipleValues(values) })));
  primitive('FAIL-PROMISE', 2, 2, ([value, reason]) => bool(getPromise(value).settle({ reason })));
  primitive('AWAIT-PROMISE', 1, 1, function* ([value]) {
    const promise = getPromise(value);
    const outcome = promise.outcome ?? (yield* awaitNotification([promise.listeners]));
    return outcome.values ?? (yield raise(outcome.reason));
  });
  primitive('MAKE-EVENT', 1, 1, ([signaled]) => new AsyncEvent(rt, signaled !== NIL));
  primitive('EVENTP', 1, 1, ([value]) => bool(owns(AsyncEvent, value)));
  primitive('EVENT-SET-P', 1, 1, ([value]) => bool(getEvent(value).signaled));
  primitive('SET-EVENT', 1, 1, ([value]) => {
    const event = getEvent(value);
    if (event.signaled) return NIL;
    event.signaled = true;
    for (const notify of event.waiters) notify(T);
    event.waiters.clear();
    return T;
  });
  primitive('RESET-EVENT', 1, 1, ([value]) => {
    const event = getEvent(value), previous = event.signaled;
    event.signaled = false;
    return bool(previous);
  });
  primitive('WAIT-EVENT', 1, 1, function* ([value]) {
    const event = getEvent(value);
    return event.signaled ? T : yield* awaitNotification([event.waiters]);
  });

  const sleep = rt.defineBuiltin('SLEEP', function* (args) {
    if (args.length !== 1) throw new LispCondition('PROGRAM-ERROR', 'Wrong number of arguments to SLEEP');
    const milliseconds = duration(args[0]);
    let clear;
    const pending = new Promise(resolve => { clear = schedule(milliseconds, () => resolve(NIL)); });
    try { return yield new CallRequest(wait, [pending]); }
    finally { clear(); }
  }, { primitive: 'SLEEP' });
  rt.primitives.set('COMMON-LISP:SLEEP', sleep);

  // Keep ordinary forwarding functions on the same ABI as compiled Lisp.
  for (const name of ['TASKP', 'TASK-NAME', 'TASK-DONE-P', 'TASK-RUNNING-P', 'TASK-COMPLETED-P', 'TASK-FAILED-P',
    'TASK-CANCELLED-P', 'CANCEL-TASK', 'CURRENT-TASK', 'YIELD', 'TASK-GROUP-P', 'TASK-GROUP-CLOSED-P',
    'TASK-GROUP-CANCELLED-P', 'CANCEL-TASK-GROUP', 'CANCEL-SCOPE-P', 'CANCEL-SCOPE', 'CANCEL-SCOPE-CANCELLED-P',
    'AWAIT', 'AWAIT-ANY', 'MAKE-PROMISE', 'PROMISEP', 'PROMISE-DONE-P', 'FULFILL-PROMISE', 'AWAIT-PROMISE',
    'EVENTP', 'EVENT-SET-P', 'SET-EVENT', 'RESET-EVENT', 'WAIT-EVENT']) {
    const symbol = as(name);
    rt.exportSymbol(symbol, pkg);
    rt.defineFunction(symbol, rt.readFunction(rt.intern(`%${name}`, zk)));
  }
  for (const name of ['ASYNC-ERROR', 'ASYNC-ERROR-MESSAGE', 'WAITED-TASK-CANCELLED', 'WAITED-TASK-CANCELLED-TASK',
    'TIMEOUT-ERROR', 'TIMEOUT-ERROR-SECONDS', 'WITH-TASK-GROUP', 'SPAWN', 'SPAWN-TASK', 'AWAIT-ALL', 'RACE', 'RACE-CALLS',
    'PROTECT-FROM-CANCEL', 'WITH-CANCEL-SCOPE', 'WITH-TIMEOUT', 'WITH-TIMEOUT-OPT', 'FAIL-PROMISE', 'MAKE-EVENT']) rt.exportSymbol(as(name), pkg);
  const values = rt.values;
  rt.evalString(asyncBootstrap);
  rt.values = values;
}

// Lisp-level syntax mirrors the core forms in lisp/runtime/async/async.lisp.
const asyncBootstrap = `
  (define-condition zk-async:async-error (error)
    ((message :initarg :message :initform "Async operation failed" :reader zk-async:async-error-message))
    (:report (lambda (condition stream) (write-string (zk-async:async-error-message condition) stream))))
  (define-condition zk-async:timeout-error (zk-async:async-error)
    ((seconds :initarg :seconds :reader zk-async:timeout-error-seconds))
    (:report (lambda (condition stream)
               (format stream "Operation timed out after ~S seconds" (zk-async:timeout-error-seconds condition)))))
  (define-condition zk-async:waited-task-cancelled (zk-async:async-error)
    ((task :initarg :task :reader zk-async:waited-task-cancelled-task))
    (:report (lambda (condition stream)
               (format stream "Waited task ~S was cancelled" (zk-async:waited-task-cancelled-task condition)))))
  (defmacro zk-async:with-task-group ((group-var) &body body)
    (let ((group (gensym "GROUP")))
      \`(zk::%call-with-task-group
        (lambda (,group)
          (unwind-protect
               (multiple-value-prog1
                   (let ((,group-var ,group)) ,@body)
                 (zk::%finish-task-group ,group nil))
            (zk::%finish-task-group ,group t))))))
  (defun zk-async:spawn (group fn &key no-wait allow-failure name)
    (zk::%spawn group (if (functionp fn) fn (fdefinition fn)) no-wait allow-failure name))
  (defmacro zk-async:spawn-task ((group &key no-wait allow-failure name) &body body)
    \`(zk::%spawn ,group (lambda () ,@body) ,no-wait ,allow-failure ,name))
  (defmacro zk-async:protect-from-cancel (&body body)
    \`(zk::%protect-from-cancel (lambda () ,@body)))
  (defmacro zk-async:with-cancel-scope ((scope-var &key (on-cancel nil on-cancel-p)) &body body)
    \`(zk::%with-cancel-scope (lambda (,scope-var) ,@body) ,(when on-cancel-p \`(lambda () ,on-cancel))))
  (defmacro zk-async:with-timeout ((seconds &key (on-timeout nil on-timeout-p)) &body body)
    (let ((secs (gensym "SECONDS")))
      \`(let ((,secs ,seconds))
         (zk::%with-timeout ,secs (lambda () ,@body)
           (lambda () ,(if on-timeout-p on-timeout \`(error 'zk-async:timeout-error :seconds ,secs)))))))
  (defmacro zk-async:with-timeout-opt ((seconds) &body body)
    \`(zk-async:with-timeout (,seconds :on-timeout (values nil nil)) (values (locally ,@body) t)))
  (defun zk-async:await-all (tasks) (mapcar #'zk-async:await tasks))
  (defun zk-async:race-calls (thunks)
    (when (null thunks) (error 'program-error :format-control "RACE requires at least one branch."))
    (if (null (rest thunks)) (funcall (first thunks))
        (let (results)
          (zk-async:with-task-group (group)
            (let* ((tasks (mapcar (lambda (fn) (zk-async:spawn group fn :no-wait t)) thunks))
                   (winner (zk-async:await-any tasks)))
              (zk-async:cancel-task-group group)
              (setq results (multiple-value-list (zk-async:await winner)))))
          (values-list results))))
  (defmacro zk-async:race (&body forms)
    (when (null forms) (error 'program-error :format-control "RACE requires at least one branch."))
    (if (null (rest forms)) (first forms)
        \`(zk-async:race-calls (list ,@(mapcar (lambda (form) \`(lambda () ,form)) forms)))))
  (defun zk-async:fail-promise (promise datum &rest arguments)
    (let ((reason (cond ((stringp datum) (if arguments (apply #'format nil datum arguments) datum))
                        ((typep datum 'condition) datum)
                        ((symbolp datum) (apply #'make-condition datum arguments))
                        (t (error 'type-error :datum datum :expected-type '(or string symbol condition))))))
      (zk::%fail-promise promise reason)))
  (defun zk-async:make-event (&key signaled) (zk::%make-event signaled))`;
