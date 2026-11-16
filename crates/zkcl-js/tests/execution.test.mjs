import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, NIL, T, UNBOUND, Cell, Closure, MultipleValues, LispCondition, SuspensionError, toArray } from '../runtime/runtime.mjs';
import { compileJavaScript } from '../runtime/codegen.mjs';
import { CancellableWait, operations } from '../runtime/execution.mjs';

const metadata=(instructions,constants=[],registers=8,arity={required:0,maximum:0})=>({instructions,constants,registers,arity,captures:[],handlers:[],restarts:[],keywords:[],jump_tables:[]});
const values=(rt,source)=>rt.evalString(source).values;

test('every canonical instruction has an execution handler',()=>{
  const names='Load Constant Copy Clear Jump Unless IfPresent UnlessFixnum UnlessFixnumInRange TryFixnumArithmetic SetValues BindValues ReadValue SaveValues AppendValues RestoreValues ReadCell WriteCell DefineFunction ProclaimSpecial ProclaimInline SetVariableDocumentation WhenBound BindSpecial BindProgv Unbind BoxLocal ReadBox WriteBox Capture Closure ExitToken EnterBlock EnterCatch EnterTagbody LeaveTarget EnterProtect BeginCleanup EndCleanup ReturnFrom Throw Go EnterHandlers LeaveHandlers EnterRestarts LeaveRestarts AssociateRestarts LeaveRestartAssociation OptionalArgument KeywordArgument RestArguments CheckKeywords DestructureNext DestructureEnd DestructureKeywords ReleaseArguments CallValues Call'.split(' ');
  assert.deepEqual(Object.keys(operations).sort(),names.sort());
});

test('AOT steps and dynamic canonical frames use the same call and values protocol',()=>{
  const rt=createRuntime();
  const dynamic=rt.makeFunction(metadata([{Load:[0,{Fixnum:'42'}]},{SetValues:{start:0,end:1}}]));
  const aot=rt.makeFunction({registers:2,arity:{required:0,maximum:0},constants:[],step(t,f){
    while(f.pc<3) { switch(f.pc++) {
      case 0:t.ops.Constant(t,f,[0,0]);break;
      case 1:t.ops.Call(t,f,{callee:{Value:0},out:1,args:{start:0,end:0}});break;
      case 2:t.ops.SetValues(t,f,{start:1,end:2});break;
    } if(t.currentFrame!==f||t.suspended||t.done)return; }
    t.returnFrame(f);
  }},{constants:[dynamic]});
  assert.deepEqual(rt.call(aot).values,[42]);
  const sliced=rt.makeFunction({registers:1,arity:{required:0,maximum:0},step(t,f){while(f.pc<2){if(t.budget--<=0){t.yielded=true;return;}switch(f.pc++){case 0:t.ops.Load(t,f,[0,{Fixnum:'7'}]);break;case 1:t.ops.SetValues(t,f,{start:0,end:1});break;}}t.returnFrame(f);}});
  const task=rt.createTask(sliced);for(let i=0;i<4&&!task.done;i++)task.run({budget:1});assert.equal(task.done,true);assert.deepEqual(task.values,[7]);
});

test('synchronous host reentry can finish an exhausted instruction budget', () => {
  const rt = createRuntime();
  const innerCode = metadata([{Load:[0,{Fixnum:'42'}]},{SetValues:{start:0,end:1}}], [], 1);
  compileJavaScript(innerCode);
  const generatedStep = innerCode.step;
  innerCode.step = (task, frame) => {
    const pc = frame.pc;
    generatedStep(task, frame);
    assert.ok(task.currentFrame !== frame || frame.pc !== pc, 'synchronous reentry must advance');
  };
  const inner = rt.makeFunction(innerCode);
  let calls = 0;
  rt.defineBuiltin('REENTER-WITH-BUDGET', (_args, runtime, task) => {
    calls++;
    return runtime.call(inner, [], task);
  });
  const outerCode = metadata([
    {Call:{callee:{Named:0},out:0,args:{start:0,end:0},at:0}},
    {Copy:[1,0]},
    {SetValues:{start:1,end:2}},
  ], [rt.intern('REENTER-WITH-BUDGET', 'COMMON-LISP')], 2);
  const outer = rt.makeFunction(compileJavaScript(outerCode));

  for (const budget of [1, 3]) {
    const task = rt.createTask(outer);
    const frame = task.currentFrame;
    task.run({budget});
    assert.equal(task.currentFrame, frame);
    assert.equal(frame.pc, 1);
    assert.equal(frame.sourcePc, 0);
    assert.equal(frame.registers[0], 42);
    assert.ok(task.budget <= 0);
    task.run({budget:1});
    assert.equal(frame.pc, 2);
    assert.equal(frame.sourcePc, 1);
    task.run({budget:1});
    assert.equal(task.done, true);
    assert.deepEqual(task.values, [42]);
  }
  assert.equal(calls, 2);
});

test('code identity survives named function replacement and shared closures mutate cells',()=>{
  const rt=createRuntime();
  values(rt,'(defun remembered () 1) (defparameter *old-function* #\'remembered) (defun remembered () 2)');
  assert.deepEqual(values(rt,'(values (remembered) (funcall *old-function*))'),[2,1]);
  assert.deepEqual(values(rt,'(let ((x 4)) (let ((get (lambda () x)) (set (lambda (v) (setq x v)))) (funcall set 9) (funcall get)))'),[9]);
  assert.deepEqual(values(rt,'(labels ((descend (n) (if (= n 0) 42 (funcall #\'descend (1- n))))) (descend 12000))'),[42]);
});

test('literal loading preserves circular conses, code cycles, and LOAD-FORM patching',()=>{
  const rt=createRuntime();
  const entry=metadata([{Constant:[0,0]},{SetValues:{start:0,end:1}}],[0],1);
  const program={target:'js',source_files:[],nodes:[{Pair:[1,0]},{Fixnum:'7'}],entries:[entry]};
  const result=rt.load(program).values[0]; assert.equal(result.car,7);assert.equal(result.cdr,result);
  assert.equal(rt.run().values[0],result);
  const cyclic=metadata([],[0],0);
  const code=rt.load({target:'js',nodes:[{Function:cyclic}],entries:[entry]}).values[0];
  assert.equal(code.constants[0],code);
  const loadform={target:'js',nodes:[{LoadForm:{creation:0,initialization:null}},{Pair:[0,2]},'Nil'],entries:[metadata([{Load:[0,{Fixnum:'17'}]},{SetValues:{start:0,end:1}}],[],1),metadata([{Constant:[0,0]},{SetValues:{start:0,end:1}}],[1],1)]};
  assert.equal(rt.load(loadform).values[0].car,17);
  rt.defineBuiltin('CREATE-LITERAL-PACKAGE',()=>{rt.makePackage('LITERAL-PACKAGE');return NIL;});
  const packageProgram={target:'js',nodes:[{Primitive:'CREATE-LITERAL-PACKAGE'},{InternedSymbol:{package:'LITERAL-PACKAGE',name:'LATER'}}],entries:[metadata([{Constant:[0,0]},{Call:{callee:{Value:0},out:null,args:{start:0,end:0},at:0}}],[0],1),metadata([{Constant:[0,0]},{SetValues:{start:0,end:1}}],[1],1)]};
  assert.equal(rt.load(packageProgram).values[0],rt.intern('LATER','LITERAL-PACKAGE'));
  assert.throws(()=>rt.load({target:'js',nodes:[{InternedSymbol:{package:'MISSING-LITERAL-PACKAGE',name:'BAD'}}],entries:[metadata([{Constant:[0,0]}],[0],1)]}),e=>e.className==='PACKAGE-ERROR');
  const depth=20000,nodes=Array.from({length:depth},(_,i)=>({Pair:[depth,i+1]}));nodes.push('Nil');
  assert.equal(toArray(rt.load({target:'js',nodes,entries:[entry]}).values[0]).length,depth);
});

test('unwind runs cleanups in their dynamic scope and cleanup exits replace pending exits',()=>{
  const rt=createRuntime();
  assert.deepEqual(values(rt,'(defparameter *scope* 0) (defparameter *seen* nil) (values (block exit (let ((*scope* 3)) (unwind-protect (return-from exit (values 1 2)) (setq *seen* *scope*)))) *seen* *scope*)'),[1,3,0]);
  assert.deepEqual(values(rt,'(block exit (unwind-protect (return-from exit (values 1 2)) (return-from exit (values 8 9))))'),[8,9]);
  assert.throws(()=>rt.evalString('(funcall (block old (return-from old (lambda () (return-from old 1)))))'),e=>e.className==='CONTROL-ERROR');
});

test('handler cases receive numeric conditions after inner cleanup',()=>{
  const rt=createRuntime(); const seen=[];
  rt.defineBuiltin('RECORD-EXECUTION',(args)=>{seen.push(...args);return NIL;});
  const code=metadata([
    {EnterHandlers:{table:0}}, {EnterProtect:{cleanup:6,clear:3}},
    {ReadCell:{out:0,symbol:0,function:true,at:0}}, {Load:[1,{Fixnum:'0'}]},
    {Call:{callee:{Value:0},out:null,args:{start:1,end:2},at:0}},
    'BeginCleanup', {Constant:[0,1]}, {Call:{callee:{Named:2},out:2,args:{start:0,end:1},at:0}}, 'EndCleanup',
    'LeaveHandlers', {Jump:14},
    {Load:[1,{Fixnum:'12'}]}, {SetValues:{start:1,end:2}}, {Jump:14},
  ],[rt.intern('/','COMMON-LISP'),new Cell(1),rt.intern('RECORD-EXECUTION','COMMON-LISP')],4);
  code.handlers=[{handlers:[{test:{Class:'ArithmeticError'},target:{Case:{pc:11,condition:0,clear:0}}}]}];
  // 1/0 is a typed numeric failure and visits the active cleanup before handler-case.
  assert.deepEqual(rt.call(rt.makeFunction(code)).values,[12]);assert.equal(seen.length,1);
});

test('local transfers and nested dispatch in declining handlers preserve the original error', () => {
  for (const executionStrategy of ['javascript', 'bytecode']) {
    const rt = createRuntime({ executionStrategy });
    const original = new LispCondition('SIMPLE-ERROR', 'Original error');
    const seen = [];
    rt.defineBuiltin('DISPATCH-FAIL', () => { throw original; });
    rt.defineBuiltin('DISPATCH-RECORD', args => { seen.push(args); return NIL; });
    const fn = rt.compileString(`
      (handler-bind
          ((error (lambda (condition)
                    (dispatch-record condition
                                     (catch 'local (throw 'local 11))
                                     (handler-case (error "Nested error")
                                       (error () 22)))))
           (error (lambda (condition) (dispatch-record condition 33))))
        (unwind-protect
            (progn (dispatch-fail) (dispatch-record 99))
          (dispatch-record 44)))`);

    assert.throws(() => rt.call(fn), error => error === original);
    assert.deepEqual(seen, [[original, 11, 22], [original, 33], [44]]);
  }
});

test('resuming a suspended error handler can decline or transfer to a restart', async () => {
  const rt = createRuntime();
  const original = new LispCondition('SIMPLE-ERROR', 'Original error');
  const seen = [];
  let release;
  rt.defineBuiltin('DISPATCH-FAIL', () => { throw original; });
  rt.defineBuiltin('DISPATCH-WAIT', () => new Promise(resolve => { release = resolve; }));
  rt.defineBuiltin('DISPATCH-RECORD', ([value]) => { seen.push(value); return NIL; });
  for (const useRestart of [false, true]) {
    const fn = rt.compileString(`
      (restart-case
          (handler-bind ((error (lambda (condition)
                                  (dispatch-wait)
                                  (dispatch-record (catch 'local (throw 'local 12)))
                                  ${useRestart ? "(invoke-restart 'resume 7 8)" : 'nil'})))
            (unwind-protect (dispatch-fail) (dispatch-record 34)))
        (resume (a b) (values a b)))`);
    const task = rt.createTask(fn);
    task.run();
    assert.ok(task.suspended);
    assert.deepEqual(seen, []);
    release(NIL);

    if (useRestart) assert.deepEqual((await task.runAsync()).values, [7, 8]);
    else await assert.rejects(task.runAsync(), error => error === original);
    assert.deepEqual(seen.splice(0), [12, 34]);
    assert.equal(task.scopes.length, 0);
    assert.equal(task.frames.length, 0);
  }
});

test('cancelling a suspended error handler revokes its Promise and runs cleanup once', async () => {
  const rt = createRuntime();
  const original = new LispCondition('SIMPLE-ERROR', 'Original error');
  const reason = new LispCondition('SIMPLE-ERROR', 'Host cancellation');
  const seen = [];
  let rejectHandler, finishCleanup;
  rt.defineBuiltin('DISPATCH-FAIL', () => { throw original; });
  rt.defineBuiltin('DISPATCH-WAIT', ([condition]) => {
    seen.push(condition);
    return new Promise((_resolve, reject) => { rejectHandler = reject; });
  });
  rt.defineBuiltin('DISPATCH-CLEANUP', () => {
    seen.push('cleanup');
    return new Promise(resolve => { finishCleanup = resolve; });
  });
  rt.defineBuiltin('DISPATCH-RECORD', ([condition]) => { seen.push(condition); return NIL; });
  const fn = rt.compileString(`
    (handler-bind ((error #'dispatch-record))
      (handler-bind ((error #'dispatch-wait))
        (unwind-protect (dispatch-fail) (dispatch-cleanup))))`);
  const task = rt.createTask(fn);
  task.run();
  assert.ok(task.suspended);
  assert.deepEqual(seen, [original]);
  const controller = new AbortController();
  const completion = task.runAsync({ signal: controller.signal });
  controller.abort(reason);
  assert.deepEqual(seen, [original, 'cleanup']);
  const cleanupPending = task.suspended;
  assert.ok(cleanupPending);
  rejectHandler(new LispCondition('SIMPLE-ERROR', 'Abandoned result'));
  await Promise.resolve();
  assert.equal(task.suspended, cleanupPending);

  finishCleanup(NIL);
  await assert.rejects(completion, error => error === reason);
  assert.deepEqual(seen, [original, 'cleanup']);
  assert.equal(task.scopes.length, 0);
  assert.equal(task.frames.length, 0);
});

test('async host suspension retains dynamic values and isolates concurrent tasks',async()=>{
  const rt=createRuntime(); const scope=rt.intern('*ASYNC-SCOPE*');scope.value=0;
  let release; const pending=new Promise(resolve=>{release=resolve;});
  rt.defineBuiltin('AWAIT-EXECUTION',()=>pending);
  const code=metadata([{Load:[0,{Fixnum:'3'}]},{BindSpecial:{symbol:0,value:0}},{Call:{callee:{Named:1},out:null,args:{start:0,end:0},at:0}}, {ReadCell:{out:0,symbol:0,function:false,at:0}},{Unbind:1},{SetValues:{start:0,end:1}}],[scope,rt.intern('AWAIT-EXECUTION','COMMON-LISP')],1);
  const task=rt.createTask(rt.makeFunction(code)); task.run();assert.ok(task.suspended);assert.equal(rt.readVariable(scope),0);assert.equal(rt.readVariable(scope,task),3);
  release(new MultipleValues([99,100]));assert.deepEqual((await task.runAsync()).values,[3]);assert.equal(task.dynamic.size,0);
});

test('synchronous suspension exposes a resumable task instead of a Promise Lisp value',async()=>{
  const rt=createRuntime();rt.defineBuiltin('ASYNC-ROOT',async()=>42);
  let task;assert.throws(()=>rt.call('ASYNC-ROOT'),error=>{assert.ok(error instanceof SuspensionError);task=error.task;return true;});
  assert.deepEqual((await task.runAsync()).values,[42]);
  rt.defineBuiltin('SYNC-REENTER',([fn],r,t)=>r.call(fn,[],t));
  assert.throws(()=>rt.evalString('(funcall #\'sync-reenter (lambda () (async-root)))'),error=>error.className==='PROGRAM-ERROR');
});

test('a suspended task has one driver and subscribes to a host thenable once', async () => {
  const rt = createRuntime();
  let subscribe, getterReads = 0, subscriptions = 0, calls = 0;
  const other = rt.defineBuiltin('OVERLAP-OTHER', () => ++calls);
  const waiting = rt.defineBuiltin('OVERLAP-WAIT', () => ({
    get then() {
      getterReads++;
      return resolve => { subscriptions++; subscribe = resolve; };
    },
  }));
  let task;
  assert.throws(() => rt.call(waiting), error => {
    assert.ok(error instanceof SuspensionError);
    task = error.task;
    return true;
  });
  const suspended = task.suspended;
  assert.throws(() => rt.call(other, [], task), error => error.className === 'PROGRAM-ERROR');
  const completion = task.runAsync();
  assert.throws(() => task.run(), error => error.className === 'PROGRAM-ERROR');
  await assert.rejects(task.runAsync(), error => error.className === 'PROGRAM-ERROR');
  assert.equal(task.suspended, suspended);
  assert.deepEqual([getterReads, subscriptions, calls], [1, 1, 0]);
  subscribe(new MultipleValues([12, 34]));
  assert.deepEqual((await completion).values, [12, 34]);
  assert.equal(task.run(), task);
  assert.equal(task.suspended, null);
});

test('host rejection is dispatched when its task drives under its dynamic bindings', async () => {
  const rt = createRuntime(), seen = [];
  const previousTask = rt.activeTask;
  let reject;
  const reason = new LispCondition('SIMPLE-ERROR', 'Rejected by the host');
  rt.defineBuiltin('STAGED-REJECTION', () => new Promise((_resolve, fail) => { reject = fail; }));
  rt.defineBuiltin('STAGED-RECORD', ([condition], runtime, task) => {
    assert.equal(runtime.activeTask, task);
    const scope = runtime.intern('*STAGED-SCOPE*');
    seen.push([condition, runtime.readVariable(scope, runtime.activeTask)]);
    return NIL;
  });
  rt.evalString('(defparameter *staged-scope* 0)');
  const task = rt.createTask(rt.compileString(`
    (let ((*staged-scope* 7))
      (unwind-protect
          (handler-bind ((error #'staged-record)) (staged-rejection))
        (staged-record :cleanup)))`));
  task.run();
  const suspended = task.suspended, frame = task.currentFrame;
  reject(reason);
  await Promise.resolve();
  assert.equal(task.suspended, suspended);
  assert.equal(task.currentFrame, frame);
  assert.equal(rt.activeTask, previousTask);
  assert.deepEqual(seen, []);
  assert.throws(() => task.run(), error => error === reason);
  assert.deepEqual(seen, [[reason, 7], [rt.intern('CLEANUP', 'KEYWORD'), 7]]);
  assert.equal(task.dynamic.size, 0);
});

test('a host catch cannot retain a suspended synchronous reentry', async () => {
  const rt = createRuntime(), seen = [];
  let rejectBody, finishCleanup, caught;
  rt.defineBuiltin('REENTRY-BODY-WAIT', () => new CancellableWait(
    new Promise((_resolve, reject) => { rejectBody = reject; }),
    () => seen.push('withdraw body'),
  ));
  rt.defineBuiltin('REENTRY-CLEANUP-WAIT', () => {
    seen.push('inner cleanup');
    return new Promise(resolve => { finishCleanup = resolve; });
  });
  rt.defineBuiltin('REENTRY-OUTER-CLEANUP', () => { seen.push('outer cleanup'); return NIL; });
  rt.defineBuiltin('REENTRY-CONTINUE', () => { seen.push('unexpected continuation'); return NIL; });
  rt.defineBuiltin('REENTRY-CATCH', ([fn], runtime, task) => {
    try { return runtime.call(fn, [], task); }
    catch (error) {
      caught = error;
      assert.throws(() => runtime.call('REENTRY-CONTINUE', [], task), blocked => blocked === caught);
      return new CancellableWait(Promise.reject(new Error('Discarded host result')), () => seen.push('withdraw return'));
    }
  });
  const fn = rt.compileString(`
    (handler-case
        (unwind-protect
            (progn
              (reentry-catch (lambda () (unwind-protect (reentry-body-wait) (reentry-cleanup-wait))))
              (reentry-continue))
          (reentry-outer-cleanup))
      (program-error (condition) condition))`);
  const task = rt.createTask(fn);
  const completion = task.runAsync();
  assert.ok(caught instanceof LispCondition);
  assert.equal(caught.className, 'PROGRAM-ERROR');
  assert.deepEqual(seen, ['withdraw body', 'withdraw return', 'inner cleanup']);
  const suspended = task.suspended;
  rejectBody(new Error('Revoked reentry wait'));
  await Promise.resolve();
  assert.equal(task.suspended, suspended);
  finishCleanup(NIL);
  assert.deepEqual((await completion).values, [caught]);
  assert.deepEqual(seen, ['withdraw body', 'withdraw return', 'inner cleanup', 'outer cleanup']);
  assert.equal(task.frames.length, 0);
  assert.equal(task.scopes.length, 0);
});

test('cancelling a suspended task runs cleanup once and detaches the abandoned host result',async()=>{
  const rt=createRuntime(), scope=rt.intern('*CANCEL-SCOPE*');scope.value=0;
  const seen=[];let release;
  rt.defineBuiltin('CANCEL-WAIT',()=>new Promise(resolve=>{release=resolve;}));
  rt.defineBuiltin('CANCEL-RECORD',args=>{seen.push(args[0]);return NIL;});
  const code=metadata([{Load:[0,{Fixnum:'3'}]},{BindSpecial:{symbol:0,value:0}},{EnterProtect:{cleanup:5,clear:1}},{Call:{callee:{Named:1},out:null,args:{start:0,end:0},at:0}},'BeginCleanup',{ReadCell:{out:0,symbol:0,function:false,at:0}},{Call:{callee:{Named:2},out:null,args:{start:0,end:1},at:0}},'EndCleanup',{Unbind:1},{SetValues:{start:0,end:0}}],[scope,rt.intern('CANCEL-WAIT','COMMON-LISP'),rt.intern('CANCEL-RECORD','COMMON-LISP')],1);
  const task=rt.createTask(rt.makeFunction(code));task.run();
  const controller=new AbortController(), reason=new Error('cancel requested');
  const completion=task.runAsync({signal:controller.signal});controller.abort(reason);
  await assert.rejects(completion,error=>error===reason);assert.deepEqual(seen,[3]);assert.equal(task.dynamic.size,0);assert.equal(rt.readVariable(scope),0);
  release(999);await Promise.resolve();assert.equal(task.error,reason);assert.deepEqual(seen,[3]);
});

test('a timer can cancel a task across immediately settled host calls', async () => {
  const rt = createRuntime();
  const limit = 1000, slice = 8;
  let calls = 0;
  const immediate = rt.defineBuiltin('IMMEDIATE-SCHEDULER-WAIT', () => {
    calls++;
    return Promise.resolve(NIL);
  });
  const loop = rt.defineBuiltin('IMMEDIATE-SCHEDULER-LOOP', function* (_args, runtime) {
    for (let i = 0; i < limit; i++) yield runtime.requestCall(immediate);
    return NIL;
  });
  const controller = new AbortController();
  const reason = new Error('Timer cancelled task');
  const task = rt.createTask(loop);
  const completion = task.runAsync({ slice, signal: controller.signal });
  const timer = setTimeout(() => controller.abort(reason), 0);
  try {
    await assert.rejects(completion, error => error === reason);
    assert.ok(calls > 0 && calls <= slice);
    assert.equal(task.frames.length, 0);
  } finally { clearTimeout(timer); }
});

test('an abort inside a host callback discards its rejecting Promise', async () => {
  const rt = createRuntime();
  const controller = new AbortController();
  const reason = new Error('Host cancelled task');
  rt.defineBuiltin('ABORT-SYNC-HOST', () => {
    controller.abort(reason);
    return Promise.reject(new Error('Abandoned host result'));
  });
  const task = rt.createTask(rt.compileString('(abort-sync-host)'));
  await assert.rejects(task.runAsync({ signal: controller.signal }), error => error === reason);
  // An unhandled rejection from the abandoned call would fail the test on the next turn.
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(task.suspended, null);
});

test('native generators resume after asynchronous Lisp callbacks with all values',async()=>{
  const rt=createRuntime();let release;const seen=[];
  rt.defineBuiltin('NATIVE-AWAIT',()=>new Promise(resolve=>{release=resolve;}));
  rt.defineBuiltin('NATIVE-RELAY',function*([callback],r){seen.push('before');const result=yield r.requestCall(callback,[]);seen.push('after');return r.multipleValues(...result.values,99);});
  rt.evalString('(defparameter *native-scope* 0) (defun native-call () (let ((*native-scope* 7)) (native-relay (lambda () (multiple-value-call #\'values (native-await) (values *native-scope*))))))');
  const task=rt.createTask('NATIVE-CALL');task.run();assert.ok(task.suspended);assert.deepEqual(seen,['before']);
  release(new MultipleValues([10,11]));assert.deepEqual((await task.runAsync()).values,[10,11,7,99]);assert.deepEqual(seen,['before','after']);assert.equal(task.dynamic.size,0);
});

test('nonlocal exits close native generators and can suspend in their finally calls',async()=>{
  const rt=createRuntime(),seen=[];let finish;
  rt.defineBuiltin('NATIVE-CLOSE-WAIT',()=>{seen.push('native-cleanup');return new Promise(resolve=>{finish=resolve;});});
  rt.defineBuiltin('NATIVE-INNER-RECORD',()=>{seen.push('lisp-cleanup');return NIL;});
  rt.defineBuiltin('NATIVE-FINALLY',function*([callback],r){try{return yield r.requestCall(callback,[]);}finally{yield r.requestCall('NATIVE-CLOSE-WAIT',[]);seen.push('native-closed');}});
  const fn=rt.evalString('(lambda () (block escape (native-finally (lambda () (unwind-protect (return-from escape (values 1 2)) (native-inner-record))))))').values[0];
  const task=rt.createTask(fn);task.run();assert.ok(task.suspended);assert.deepEqual(seen,['lisp-cleanup','native-cleanup']);
  finish(100);assert.deepEqual((await task.runAsync()).values,[1,2]);assert.deepEqual(seen,['lisp-cleanup','native-cleanup','native-closed']);
});

test('cancellation closes native generator state while its cleanup may suspend',async()=>{
  const rt=createRuntime(),seen=[];let rejectBody,finishCleanup;
  rt.defineBuiltin('NATIVE-CANCEL-BODY',()=>new Promise((_resolve,reject)=>{rejectBody=reject;}));
  rt.defineBuiltin('NATIVE-CANCEL-CLEANUP',()=>{seen.push('cleanup');return new Promise(resolve=>{finishCleanup=resolve;});});
  rt.defineBuiltin('NATIVE-CANCEL-SCOPE',function*(_args,r){try{yield r.requestCall('NATIVE-CANCEL-BODY',[]);}finally{yield r.requestCall('NATIVE-CANCEL-CLEANUP',[]);seen.push('closed');}});
  const task=rt.createTask('NATIVE-CANCEL-SCOPE');task.run();const controller=new AbortController(),reason=new Error('cancel native');
  const completion=task.runAsync({signal:controller.signal});controller.abort(reason);assert.deepEqual(seen,['cleanup']);assert.ok(task.suspended);
  rejectBody(new Error('abandoned native body'));await Promise.resolve();assert.equal(task.done,false);
  finishCleanup(NIL);await assert.rejects(completion,error=>error===reason);assert.deepEqual(seen,['cleanup','closed']);assert.equal(task.frames.length,0);
});

test('finite runs step while async slices can enter budget-checked fast bodies',async()=>{
  const rt=createRuntime();
  const makeCode=(checks)=>{
    const calls={fast:0,step:0};
    const code={
      registers:1,
      arity:{required:0,maximum:0},
      step(task,frame){calls.step++;task.budget--;task.returnFrame(frame);},
      fast(task,frame){calls.fast++;task.returnFrame(frame);},
    };
    if(checks) code.fastChecksBudget=true;
    return {code,calls};
  };

  const sync=makeCode(false);
  rt.createTask(rt.makeFunction(sync.code)).run();
  assert.equal(sync.calls.fast,1,'an unbounded run always takes the fast body');

  const exact=makeCode(true);
  const stepped=rt.createTask(rt.makeFunction(exact.code)).run({budget:1});
  assert.deepEqual(exact.calls,{fast:0,step:1});
  assert.equal(stepped.budget,0);

  const unclaimed=makeCode(false);
  await rt.createTask(rt.makeFunction(unclaimed.code)).runAsync({slice:8});
  assert.deepEqual(unclaimed.calls,{fast:0,step:1});

  const claimed=makeCode(true);
  const task=rt.createTask(rt.makeFunction(claimed.code));
  await task.runAsync({slice:8});
  assert.deepEqual(claimed.calls,{fast:1,step:0});
  assert.equal(task.budget,7,'one budget unit is spent per fast entry');
});
