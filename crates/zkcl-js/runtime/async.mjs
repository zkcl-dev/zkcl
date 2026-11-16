import { NIL, T, Cons, LispArray, LispString, LispSymbol, fromArray, toArray } from './objects.mjs';
import { integer, isInteger, isReal, toNumber } from './numbers.mjs';
import { CANCELLED_BOUNDARY, CallRequest, CancellableWait, LispCondition, MultipleValues, NativeFunction, yieldToHost } from './execution.mjs';
import { readFileOctets } from './file_streams.mjs';

const bool = value => value ? T : NIL;
const taskCancellation = new Error('Task cancelled');
const wait = new NativeFunction('WAIT-FOR-ASYNC-OPERATION', ([pending]) => pending);
const park = new NativeFunction('PARK-ASYNC-OPERATION', ([operation]) =>
  new CancellableWait(operation.promise, () => operation.withdraw()));

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
  takeFailure() {
    const failure = this.failure;
    this.failure = null;
    return failure;
  }
}

class CancelScope {
  constructor(task, boundary) { this.task = task; this.boundary = boundary; }
}

class AsyncPromise extends Completion {}

class AsyncEvent {
  constructor(runtime, signaled) { this.runtime = runtime; this.signaled = signaled; this.waiters = new Set(); }
}

// Every parked operation owns its registrations. A winning branch withdraws
// all its siblings in the same turn, before another task can offer a transfer.
class AsyncWait {
  constructor() {
    this.registrations = [];
    this.promise = new Promise(resolve => { this.resolve = resolve; });
  }
  defer(remove) { this.registrations.push(remove); }
  register(queue, entry) {
    queue.add(entry);
    this.defer(() => queue.delete(entry));
  }
  trackTasks(owner, targets) {
    owner.dependencies = targets;
    this.defer(() => {
      if (owner.dependencies === targets) owner.dependencies = null;
    });
  }
  withdraw() {
    const registrations = this.registrations;
    this.registrations = null;
    if (registrations) for (const remove of registrations) remove();
  }
  finish(outcome) {
    if (!this.registrations) return;
    this.withdraw();
    this.resolve(outcome);
  }
  *wait() {
    try { return (yield new CallRequest(park, [this])).values[0]; }
    finally { this.withdraw(); }
  }
}

function* awaitNotification(sources, operation = new AsyncWait()) {
  const notify = outcome => operation.finish(outcome);
  for (const listeners of sources) operation.register(listeners, notify);
  return yield* operation.wait();
}

const closedChannel = Symbol('closed channel');

class AsyncChannel {
  constructor(runtime, capacity) {
    this.runtime = runtime;
    this.capacity = capacity;
    this.closed = false;
    // Entry objects preserve repeated values while providing FIFO insertion
    // and removal without moving the rest of the buffer.
    this.buffer = new Set();
    this.senders = new Set();
    this.receivers = new Set();
  }
  trySend(value) {
    if (this.closed) return closedChannel;
    const receiver = this.receivers.values().next().value;
    if (receiver) receiver([value, T]);
    else {
      if (this.buffer.size >= this.capacity) return null;
      this.buffer.add({ value });
    }
    return T;
  }
  tryReceive() {
    const entry = this.buffer.values().next().value;
    if (entry) this.buffer.delete(entry);
    const sender = this.senders.values().next().value;
    if (sender) {
      sender.notify(T);
      if (entry) this.buffer.add({ value: sender.value });
    }
    if (entry || sender) return [(entry ?? sender).value, T];
    return this.closed ? [NIL, NIL] : null;
  }
  close() {
    if (this.closed) return false;
    this.closed = true;
    for (const notify of this.receivers) notify([NIL, NIL]);
    for (const sender of this.senders) sender.notify(closedChannel);
    return true;
  }
}

class AsyncSemaphore {
  constructor(runtime, permits) {
    this.runtime = runtime;
    this.permits = permits;
    this.waiters = new Set();
  }
  tryAcquire() {
    if (this.permits === 0n) return false;
    this.permits--;
    return true;
  }
  release() {
    const notify = this.waiters.values().next().value;
    if (notify) notify();
    else this.permits++;
    return integer(this.permits);
  }
}

const shieldsCancellation = scope =>
  scope.kind === 'cancel-shield' || scope.kind === 'cleanup' || scope.kind === 'native-cleanup';

function localDeadline(task) {
  for (let i = task.scopes.length - 1; i >= 0; i--) {
    const scope = task.scopes[i];
    if (shieldsCancellation(scope)) break;
    if (scope.deadline || scope.triggered) return true;
  }
  return false;
}

// Cancellation can release an unshielded wait, but the task must still
// cancel and join its nested groups before it finishes.
function canFinish(owner, targets) {
  const waits = new Map(), cancelledJoins = new Map(), todo = [], ready = [];
  function intern(handle, cancelBeforeJoin) {
    const nodes = cancelBeforeJoin ? cancelledJoins : waits;
    let node = nodes.get(handle);
    if (!node) {
      node = { handle, cancelBeforeJoin, required: 0, alternative: false, users: [], target: false };
      nodes.set(handle, node);
      todo.push(node);
    }
    return node;
  }
  for (const handle of targets) intern(handle, false).target = true;
  for (const node of todo) {
    const { handle, cancelBeforeJoin } = node;
    const { task } = handle;
    const cancelled = (cancelBeforeJoin || task.cancelRequest !== null) && !task.scopes.some(shieldsCancellation);
    if (handle === owner && !cancelled) continue;
    if (handle.status === 'running') {
      if (!cancelled && !localDeadline(task) && handle.dependencies) {
        node.alternative = true;
        for (const target of handle.dependencies) intern(target, false).users.push([node, false]);
      }
      for (const group of handle.groups) {
        const cancelAll = cancelled || group.phase === 'cancelling';
        for (const [member, noWait] of group.members) {
          node.required++;
          intern(member, Boolean(cancelAll || noWait)).users.push([node, true]);
        }
      }
    }
    if (!node.required && !node.alternative) {
      if (node.target) return true;
      ready.push(node);
    }
  }
  for (const node of ready) {
    if (node.target) return true;
    for (const [user, required] of node.users) {
      if (!user.required && !user.alternative) continue;
      if (required) user.required--;
      else user.alternative = false;
      if (!user.required && !user.alternative) ready.push(user);
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

// Indexed in the same order as the native %SELECT-PACKED instruction ABI.
const selectOperators = [
  ['RECV', 1, 2], ['SEND', 2, 1], ['EVENT', 1, 0],
  ['TASK', 1, 1], ['PROMISE', 1, 1], ['TIMEOUT', 1, 0],
];

function selectMacro(cl, keyword, select) {
  const operators = new Map(selectOperators.map(operator => [keyword(operator[0]), operator]));
  const list = (...items) => fromArray(items);
  const form = (name, ...args) => list(cl(name), ...args);
  const invalid = message => { throw new LispCondition('PROGRAM-ERROR', message); };
  const parts = value => {
    try { return toArray(value); }
    catch { return invalid('Expected a proper SELECT clause list'); }
  };
  return ([whole]) => {
    const clauses = parts(whole.cdr), bindings = [], specs = [], cases = [];
    const index = new LispSymbol('SELECT-INDEX'), results = [new LispSymbol('SELECT-VALUE'), new LispSymbol('SELECT-OK')];
    let biased = NIL, fallback;
    if (clauses[0] instanceof Cons && clauses[0].car === keyword('BIASED')) {
      const option = parts(clauses.shift());
      if (option.length !== 2) invalid('Invalid (:BIASED ...) option in SELECT');
      biased = option[1];
    }
    if (!clauses.length) invalid('SELECT requires at least one clause');
    for (const clause of clauses) {
      if (!(clause instanceof Cons)) invalid('Malformed SELECT clause');
      const [head, ...body] = parts(clause);
      if (head === cl('OTHERWISE') || head === keyword('DEFAULT') ||
          (head instanceof Cons && head.car === keyword('DEFAULT') && head.cdr === NIL)) {
        if (fallback) invalid('Duplicate default clause in SELECT');
        fallback = list(list(NIL), ...body);
        continue;
      }
      const [tag, ...args] = parts(head), operator = operators.get(tag);
      if (!operator) invalid('Unknown SELECT clause operator');
      const [name, inputs, outputs] = operator;
      if (args.length < inputs || args.length > inputs + outputs) invalid(`Invalid (:${name} ...) clause in SELECT`);
      const operands = args.slice(0, inputs).map((value, i) => {
        const temp = new LispSymbol(`${name}-${i + 1}`);
        bindings.push(list(temp, value));
        return temp;
      });
      const locals = [];
      args.slice(inputs).forEach((name, i) => {
        if (name === NIL) return;
        if (!(name instanceof LispSymbol) || name.constant) invalid('Invalid SELECT binding variable');
        locals.push(list(name, results[i]));
      });
      cases.push(list(specs.length, locals.length ? form('LET', fromArray(locals), ...body) : form('PROGN', ...body)));
      specs.push(form('LIST', tag, ...operands));
    }
    if (fallback) cases.push(fallback);
    return form('LET', fromArray(bindings),
      form('MULTIPLE-VALUE-BIND', list(index, ...results), list(select, form('LIST', ...specs), fallback ? T : NIL, biased),
        form('CASE', index, ...cases)));
  };
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
  const getChannel = value => expect(AsyncChannel, value, 'CHANNEL');
  const getSemaphore = value => expect(AsyncSemaphore, value, 'SEMAPHORE');
  const error = message => new CallRequest(cl('ERROR'), [as('ASYNC-ERROR'), keyword('MESSAGE'), new LispString(message)]);
  const raise = value => value instanceof LispCondition ? new CallRequest(cl('ERROR'), [value]) : error(value?.message ?? String(value));
  const nonnegative = value => {
    if (!isInteger(value) || value < 0) throw new LispCondition('TYPE-ERROR', 'Expected a nonnegative integer', { datum: value, expectedType: fromArray([cl('INTEGER'), 0]) });
    return BigInt(value);
  };

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
    let result, failure;
    try {
      result = yield new CallRequest(body, [group]);
      if (boundary.active) task.popScope('cancel-boundary');
      if (result !== CANCELLED_BOUNDARY) {
        group.finish(false);
        if (!group.join.outcome) yield* awaitNotification([group.join.listeners]);
      }
    }
    finally {
      if (boundary.active) task.popScope('cancel-boundary');
      try {
        if (!group.join.outcome) {
          group.finish(true);
          task.pushScope(task.currentFrame, 'cancel-shield');
          try { if (!group.join.outcome) yield* awaitNotification([group.join.listeners]); }
          finally { task.popScope('cancel-shield'); }
        }
      } finally {
        failure = group.takeFailure();
        group.phase = 'closed';
        owner.groups.delete(group);
      }
    }
    if (failure) return yield raise(failure.error);
    return result;
  });
  primitive('FINISH-TASK-GROUP', 2, 2, function* ([value, unwinding], _r, task) {
    const group = getGroup(value);
    if (group.owner.task !== task) throw new LispCondition('CONTROL-ERROR', 'Only the owner can finish a task group');
    group.finish(unwinding !== NIL);
    if (!group.join.outcome) yield* awaitNotification([group.join.listeners]);
    if (unwinding !== NIL && group.failure) {
      const exit = task.scopes.findLast(scope => scope.kind === 'cleanup')?.exit;
      // A transfer past the group skips the wrapper's normal failure report.
      if (exit && task.scopes.indexOf(exit.destination) < task.scopes.indexOf(group.boundary)) {
        return yield raise(group.takeFailure().error);
      }
    }
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
    if (!localDeadline(task) && !canFinish(owner, targets)) return null;
    const operation = new AsyncWait();
    operation.trackTasks(owner, targets);
    return yield* awaitNotification(targets.map(handle => handle.completion.listeners), operation);
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
  primitive('YIELD', 0, 0, async () => { await yieldToHost(); return NIL; });
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

  primitive('MAKE-CHANNEL', 1, 1, ([capacity]) => new AsyncChannel(rt, capacity === NIL ? Infinity : nonnegative(capacity)));
  primitive('CHANNELP', 1, 1, ([value]) => bool(owns(AsyncChannel, value)));
  primitive('CHANNEL-CLOSED-P', 1, 1, ([value]) => bool(getChannel(value).closed));
  primitive('CLOSE-CHANNEL', 1, 1, ([value]) => bool(getChannel(value).close()));
  const sendError = () => error('Cannot send to a closed channel');
  primitive('CHANNEL-TRY-SEND', 2, 2, ([value, message]) => {
    const result = getChannel(value).trySend(message);
    return result === closedChannel ? sendError() : result ?? NIL;
  });
  primitive('CHANNEL-SEND', 2, 2, function* ([value, message]) {
    const channel = getChannel(value);
    let result = channel.trySend(message);
    if (result === null) {
      const operation = new AsyncWait();
      operation.register(channel.senders, { value: message, notify: outcome => operation.finish(outcome) });
      result = yield* operation.wait();
    }
    return result === closedChannel ? yield sendError() : message;
  });
  primitive('CHANNEL-TRY-RECV', 1, 1, ([value]) => new MultipleValues(getChannel(value).tryReceive() ?? [NIL, NIL]));
  primitive('CHANNEL-RECV', 1, 1, function* ([value]) {
    const channel = getChannel(value);
    const result = channel.tryReceive() ?? (yield* awaitNotification([channel.receivers]));
    return new MultipleValues(result);
  });

  primitive('MAKE-SEMAPHORE', 1, 1, ([permits]) => new AsyncSemaphore(rt, nonnegative(permits)));
  primitive('SEMAPHOREP', 1, 1, ([value]) => bool(owns(AsyncSemaphore, value)));
  primitive('SEMAPHORE-COUNT', 1, 1, ([value]) => integer(getSemaphore(value).permits));
  primitive('RELEASE-SEMAPHORE', 1, 1, ([value]) => getSemaphore(value).release());
  const tryAcquire = (semaphore, marker) => {
    if (marker !== undefined && !(marker instanceof Cons)) {
      throw new LispCondition('TYPE-ERROR', 'Semaphore ownership marker must be a cons', { datum: marker, expectedType: cl('CONS') });
    }
    if (!semaphore.tryAcquire()) return NIL;
    if (marker) marker.car = T;
    return T;
  };
  primitive('TRY-ACQUIRE-SEMAPHORE', 1, 2, ([value, marker]) => tryAcquire(getSemaphore(value), marker));
  primitive('ACQUIRE-SEMAPHORE', 1, 2, function* ([value, marker]) {
    const semaphore = getSemaphore(value);
    if (tryAcquire(semaphore, marker) === T) return T;
    const operation = new AsyncWait();
    let reserved = false;
    operation.register(semaphore.waiters, () => {
      reserved = true;
      operation.finish(T);
    });
    try {
      yield* operation.wait();
      if (marker) marker.car = T;
      reserved = false;
      return T;
    } finally {
      // The permit was removed from the queue, even if this task was
      // cancelled before it could record ownership and enter the body.
      if (reserved) semaphore.release();
    }
  });

  let selectCursor = 0;
  const selectTags = new Map(selectOperators.map(([name], index) => [keyword(name), index]));
  const selectSources = [getChannel, getChannel, getEvent, getTask, getPromise, duration];
  const invalidSelect = () => { throw new LispCondition('CONTROL-ERROR', 'Invalid SELECT descriptor'); };
  const selectList = value => {
    try { return toArray(value); }
    catch { return invalidSelect(); }
  };
  function branch(index, opcode, operands) {
    const operator = Number.isInteger(opcode) ? selectOperators[opcode] : null;
    if (!operator || operands.length !== operator[1]) return invalidSelect();
    return { index, name: operator[0], source: selectSources[opcode](operands[0]), value: operands[1] };
  }
  function probe({ name, source, value }) {
    switch (name) {
      case 'RECV': return source.tryReceive();
      case 'SEND': {
        const result = source.trySend(value);
        return result === T ? [value, T] : result;
      }
      case 'EVENT': return source.signaled ? [T, T] : null;
      case 'TASK': return source.status === 'running' ? null : [source, T];
      case 'PROMISE': return source.outcome ? [source, T] : null;
      case 'TIMEOUT': return null;
    }
  }
  function subscribe(operation, { index, name, source, value }) {
    const finish = outcome => operation.finish(outcome === closedChannel ? outcome : [index, ...outcome]);
    switch (name) {
      case 'RECV': operation.register(source.receivers, finish); break;
      case 'SEND': operation.register(source.senders, { value, notify: result => finish(result === T ? [value, T] : result) }); break;
      case 'EVENT': operation.register(source.waiters, () => finish([T, T])); break;
      case 'TASK': operation.register(source.completion.listeners, () => finish([source, T])); break;
      case 'PROMISE': operation.register(source.listeners, () => finish([source, T])); break;
    }
  }
  function* select(branches, fallback, biased, task) {
    if (!branches.length && !fallback) return yield error('SELECT requires at least one branch');
    const start = biased || branches.length < 2 ? 0 : selectCursor++ % branches.length;
    const ordered = [...branches.slice(start), ...branches.slice(0, start)];
    for (const branch of ordered) {
      const result = probe(branch);
      if (result === closedChannel) return yield sendError();
      if (result) return rt.multipleValues(branch.index, ...result);
    }
    let timeout;
    for (const branch of branches) {
      if (branch.name === 'TIMEOUT' && (!timeout || branch.source < timeout.source)) timeout = branch;
    }
    if (timeout?.source === 0) return rt.multipleValues(timeout.index, NIL, NIL);
    if (fallback) return rt.multipleValues(NIL, NIL, NIL);
    const owner = current(task);
    const targets = branches.every(branch => branch.name === 'TASK') ? branches.map(branch => branch.source) : null;
    if (targets && !localDeadline(task) && !canFinish(owner, targets)) return yield error('Waiting would create a task dependency cycle');
    const operation = new AsyncWait();
    if (targets) operation.trackTasks(owner, targets);
    for (const branch of ordered) subscribe(operation, branch);
    if (timeout) operation.defer(schedule(timeout.source, () => operation.finish([timeout.index, NIL, NIL])));
    const result = yield* operation.wait();
    return result === closedChannel ? yield sendError() : new MultipleValues(result);
  }
  primitive('SELECT', 3, 3, ([specs, fallback, biased], _r, task) => {
    const branches = selectList(specs).map((spec, index) => {
      const [tag, ...operands] = selectList(spec);
      return branch(index, selectTags.get(tag), operands);
    });
    return select(branches, fallback !== NIL, biased !== NIL, task);
  });
  primitive('SELECT-PACKED', 3, Infinity, ([opcodes, fallback, biased, ...operands], _r, task) => {
    if (!(opcodes instanceof LispArray) || opcodes.rank !== 1) return invalidSelect();
    let offset = 0;
    const branches = [];
    for (let i = 0; i < (opcodes.fillPointer ?? opcodes.totalSize); i++) {
      const opcode = opcodes.get(i), count = opcode === 1 ? 2 : 1;
      branches.push(branch(i, opcode, operands.slice(offset, offset + count)));
      offset += count;
    }
    if (offset !== operands.length) return invalidSelect();
    return select(branches, fallback !== NIL, biased !== NIL, task);
  });
  const selectSymbol = rt.exportSymbol(as('SELECT'), pkg);
  rt.defineFunction(selectSymbol, new NativeFunction('ZK-ASYNC:SELECT', selectMacro(cl, keyword, rt.intern('%SELECT', zk))), true);
  primitive('READ-FILE-OCTETS', 3, 3, readFileOctets);

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
    'EVENTP', 'EVENT-SET-P', 'SET-EVENT', 'RESET-EVENT', 'WAIT-EVENT',
    'CHANNELP', 'CHANNEL-CLOSED-P', 'CLOSE-CHANNEL', 'CHANNEL-SEND', 'CHANNEL-RECV', 'CHANNEL-TRY-SEND', 'CHANNEL-TRY-RECV',
    'SEMAPHOREP', 'SEMAPHORE-COUNT', 'ACQUIRE-SEMAPHORE', 'TRY-ACQUIRE-SEMAPHORE', 'RELEASE-SEMAPHORE']) {
    const symbol = as(name);
    rt.exportSymbol(symbol, pkg);
    rt.defineFunction(symbol, rt.readFunction(rt.intern(`%${name}`, zk)));
  }
  for (const name of ['ASYNC-ERROR', 'ASYNC-ERROR-MESSAGE', 'WAITED-TASK-CANCELLED', 'WAITED-TASK-CANCELLED-TASK',
    'TIMEOUT-ERROR', 'TIMEOUT-ERROR-SECONDS', 'WITH-TASK-GROUP', 'SPAWN', 'SPAWN-TASK', 'AWAIT-ALL', 'RACE', 'RACE-CALLS',
    'PROTECT-FROM-CANCEL', 'WITH-CANCEL-SCOPE', 'WITH-TIMEOUT', 'WITH-TIMEOUT-OPT', 'FAIL-PROMISE', 'MAKE-EVENT',
    'MAKE-CHANNEL', 'MAKE-SEMAPHORE', 'WITH-SEMAPHORE', 'READ-FILE-OCTETS']) rt.exportSymbol(as(name), pkg);
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
  (defun zk-async:make-event (&key signaled) (zk::%make-event signaled))
  (defun zk-async:make-channel (&key capacity) (zk::%make-channel capacity))
  (defun zk-async:make-semaphore (&optional (permits 1)) (zk::%make-semaphore permits))
  (defmacro zk-async:with-semaphore ((semaphore) &body body)
    (let ((sem (gensym "SEMAPHORE")) (acquired (gensym "ACQUIRED")))
      \`(let ((,sem ,semaphore) (,acquired (cons nil nil)))
          (unwind-protect
               (progn (zk::%acquire-semaphore ,sem ,acquired) ,@body)
            (when (car ,acquired) (zk-async:release-semaphore ,sem))))))
  (defun zk-async:read-file-octets (pathname &key (start 0) end)
    (zk::%read-file-octets pathname start end))`;
