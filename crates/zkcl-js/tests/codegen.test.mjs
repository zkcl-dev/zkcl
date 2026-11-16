import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, NIL, T, LispCode } from '../runtime/runtime.mjs';
import { compileJavaScript, CodegenError, generateStep } from '../runtime/codegen.mjs';

const metadata = instructions => ({instructions,constants:[],registers:2,arity:{required:0,maximum:0},captures:[]});
function compiled(rt, source) {
  const fn=rt.compile(rt.read(source));
  compileJavaScript(fn.code.entry);
  return fn;
}

test('canonical code gets a real entrypoint with stable identity and no instruction decoding', () => {
  const rt=createRuntime(), code=metadata([{Load:[0,{Fixnum:'42'}]},{SetValues:{start:0,end:1}}]);
  code.name = 'LOAD-a+b';
  const { source } = generateStep(code);
  assert.match(source, /^      r\[0\] = 42;/m);
  assert.doesNotMatch(source, /const operands|task\.runtime\.objects/);
  const fn=rt.makeFunction(code), identity=fn.code;
  assert.equal(compileJavaScript(code),code);
  let entered=0; const generated=code.step;
  assert.equal(generated.name, 'step_LOAD_a_b');
  code.step=(task,frame)=>{entered++;return generated(task,frame);};
  code.instructions=[{UnknownAfterCompilation:null}];
  assert.deepEqual(rt.call(fn).values,[42]);
  assert.equal(fn.code,identity);
  assert(entered>0);
  assert.deepEqual(code.source_pcs,[null,null]);
  assert.deepEqual(code.operands,[]);
});

test('compiled steps preserve budgets of one instruction and exact resume PCs', () => {
  const rt=createRuntime(), code=metadata([{Load:[0,{Fixnum:'7'}]},{Copy:[1,0]},{Clear:{start:0,end:1}},{SetValues:{start:1,end:2}}]);
  compileJavaScript(code);const task=rt.createTask(rt.makeFunction(code));
  const pcs=[];
  while(!task.done && pcs.length<8){task.run({budget:1});pcs.push(task.currentFrame?.pc??null);}
  assert.equal(task.done,true);assert.deepEqual(task.values,[7]);assert.deepEqual(pcs,[1,2,3,null]);
});

test('nested functions compile recursively and preserve shared mutable captures', () => {
  const rt=createRuntime(),fn=compiled(rt,'(lambda () (let ((counter 0)) (labels ((next () (incf counter))) (values (next) (next)))))');
  assert.deepEqual(rt.call(fn).values,[1,2]);
  const nested=fn.code.entry.constants.filter(value=>value instanceof LispCode);
  assert(nested.length>0);
  for(const code of nested)assert.equal(typeof code.entry.step,'function');
});

test('compiled branches, argument presence and multiple values agree with canonical execution', () => {
  const rt=createRuntime();
  for(const [source,args]of [
    ['(lambda (x) (if x (values 1 2) (values 3 4)))',[NIL]],
    ['(lambda (x) (if x (values 1 2) (values 3 4)))',[T]],
    ['(lambda (&optional (x 9 supplied)) (values x supplied))',[]],
    ['(lambda (&optional (x 9 supplied)) (values x supplied))',[NIL]],
    ['(lambda (&key (x 9 supplied)) (values x supplied))',[]],
    ['(lambda () (multiple-value-bind (a b c) (values 3 4) (values a b c)))',[]],
    ['(lambda () (block done (return-from done (values 6 7))))',[]],
    ['(lambda () (catch (quote end) (unwind-protect (throw (quote end) (values 4 5)) (+ 1 2))))',[]],
    ['(lambda () (handler-case (error "expected") (error (c) (declare (ignore c)) 42)))',[]],
  ]) {
    const fn=rt.compile(rt.read(source)),code=fn.code.entry;
    const old=code.step;delete code.step;
    const expected=rt.call(fn,args).values;
    if(old)code.step=old;else compileJavaScript(code);
    assert.deepEqual(rt.call(fn,args).values,expected,source);
  }
});

test('compiled SetValues and KeywordArgument enforce the shared runtime boundaries', () => {
  const rt = createRuntime();
  const limit = rt.evalString('multiple-values-limit').values[0];
  const cases = [
    [{...metadata([{SetValues: {start: 0, end: limit - 1}}]), registers: limit}, limit - 1],
    [{...metadata([{SetValues: {start: 0, end: limit}}]), registers: limit}, /MULTIPLE-VALUES-LIMIT/],
    [metadata([{KeywordArgument: [0, 0]}, {SetValues: {start: 0, end: 1}}]), /Keyword argument index/],
  ];
  for (const [code, expected] of cases) {
    const fn = rt.makeFunction(code);
    for (const compile of [false, true]) {
      if (compile) compileJavaScript(code);
      if (typeof expected === 'number') assert.equal(rt.call(fn).values.length, expected);
      else assert.throws(() => rt.call(fn), error => error.className === 'PROGRAM-ERROR' && expected.test(error.message));
    }
  }
});

test('unknown instructions and unavailable code generation fail explicitly and atomically', () => {
  const child=metadata([{NoSuchInstruction:[]}]),parent=metadata([{SetValues:{start:0,end:0}}]);
  parent.constants=[new LispCode(child)];
  assert.throws(()=>compileJavaScript(parent),CodegenError);
  assert.equal(parent.step,undefined);assert.equal(child.step,undefined);
  const code=metadata([{SetValues:{start:0,end:0}}]),original=globalThis.Function;
  try {
    globalThis.Function=function(){throw new EvalError('dynamic code disabled');};
    assert.throws(()=>compileJavaScript(code),error=>error instanceof CodegenError&&/JavaScript code generation failed/.test(error.message));
    assert.equal(code.step,undefined);
  } finally {globalThis.Function=original;}
  assert.throws(()=>compileJavaScript(metadata([{Copy:['0];throw 1;//',0]}])),CodegenError);
});

test('Common Lisp COMPILE installs and executes generated code; bytecode strategy is explicit', () => {
  const rt=createRuntime();
  const [fn]=rt.evalString("(compile nil '(lambda (x) (+ x 1)))").values;
  const code=fn.code.entry;assert.equal(typeof code.step,'function');
  let entered=0;const step=code.step;code.step=(task,frame)=>{entered++;return step(task,frame);};
  assert.deepEqual(rt.call(fn,[41]).values,[42]);assert(entered>0);
  const bytecode=createRuntime({executionStrategy:'bytecode'}),plain=bytecode.compile(bytecode.read('(lambda (x) (+ x 1))'));
  assert.equal(plain.code.entry.step,undefined);assert.deepEqual(bytecode.call(plain,[41]).values,[42]);
});
