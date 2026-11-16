import { NIL, T, UNBOUND, LispSymbol, Cons, Cell, LispCode, Closure, LispString, LispCharacter, LispArray, LispPathname, fromArray, toArray, eq, eql, equal } from './objects.mjs';
import { integer, SingleFloat, DoubleFloat, Ratio, Complex, makeRatio, makeComplex } from './numbers.mjs';
import { Task, MultipleValues, CallRequest, NativeFunction, ExitToken, LispCondition, SuspensionError } from './execution.mjs';
import { runCompiledProgram } from './loader.mjs';
import { installBuiltins, writeToString } from './builtins.mjs';
import { installFrontend } from './compiler.mjs';
import { readerBindings } from './lisp_files.mjs';
import { installClos } from './clos.mjs';
import { installPathnames } from './pathnames.mjs';
import { installAsync } from './async.mjs';
import { installDebugger } from './debugger.mjs';

export class LispPackage {
  constructor(name, { use = [], nicknames = [] } = {}) { this.name=name; this.symbols=new Map(); this.exports=new Set(); this.uses=use; this.nicknames=new Set(nicknames); this.localNicknames=new Map(); }
}
function classSpelling(name) { return name.replace(/([a-z])([A-Z])/g,'$1-$2').toUpperCase(); }

export class Runtime {
  get currentPackage() {
    const symbol=this.packages?.get('COMMON-LISP')?.symbols.get('*PACKAGE*');
    const stack=symbol && this.activeTask?.dynamic.get(symbol);
    return stack?.length?stack.at(-1):this._currentPackage;
  }
  set currentPackage(value) { this._currentPackage=value; }
  constructor(options = {}) {
    this.packages=new Map(); this.primitives=new Map(); this.installers=new Map(); this.specials=new Set(); this.values=[]; this.result=new MultipleValues(); this.setfFunctions=new Map(); this.constantFunctions=new Map();this.constantMacros=new Map();
    this.executionStrategy=options.executionStrategy??'javascript';
    this.objects=Object.freeze({NIL,T,UNBOUND,LispCharacter});
    if(!['javascript','bytecode'].includes(this.executionStrategy)) throw new TypeError('Unknown execution strategy');
    this.output=options.output ?? { write(text) { if(typeof process!=='undefined' && process.stdout) process.stdout.write(text); else console.log(text); } };
    this.readEval=options.readEval ?? true;
    const cl=this.makePackage('COMMON-LISP',{nicknames:['CL']}), keyword=this.makePackage('KEYWORD');
    cl.symbols.set('NIL',NIL); cl.symbols.set('T',T); cl.exports.add('NIL'); cl.exports.add('T');
    this.currentPackage=this.makePackage('CL-USER',{use:[cl],nicknames:['COMMON-LISP-USER']});
    for(const name of 'QUOTE FUNCTION LAMBDA IF PROGN LET LET* SETQ DEFUN DEFMACRO DEFVAR DEFPARAMETER BLOCK RETURN-FROM RETURN TAGBODY GO CATCH THROW UNWIND-PROTECT FLET LABELS MACROLET SYMBOL-MACROLET DECLARE SPECIAL INLINE NOTINLINE DYNAMIC-EXTENT IGNORE IGNORABLE TYPE FTYPE OPTIMIZE &OPTIONAL &REST &BODY &KEY &ALLOW-OTHER-KEYS &AUX &WHOLE &ENVIRONMENT MULTIPLE-VALUE-CALL MULTIPLE-VALUE-PROG1 MULTIPLE-VALUE-BIND MULTIPLE-VALUE-SETQ PROGV HANDLER-BIND HANDLER-CASE RESTART-BIND RESTART-CASE WITH-CONDITION-RESTARTS EVAL-WHEN LOAD-TIME-VALUE LOCALLY THE SETF IN-PACKAGE DEFPACKAGE DEFCONSTANT DEFTYPE DEFCLASS DEFMETHOD DEFGENERIC DEFSTRUCT'.split(' ')) this.exportSymbol(this.intern(name,cl),cl);
    this.features=fromArray(['ZKCL','JAVASCRIPT','COMMON-LISP','PACKAGE-LOCAL-NICKNAMES'].map(name=>this.intern(name,keyword)));
    this.intern('*PACKAGE*',cl).value=this.currentPackage; this.intern('*FEATURES*',cl).value=this.features;
    installBuiltins(this); installClos(this); installDebugger(this, options.debugger);
    installPathnames(this, options.pathnameHost); installFrontend(this); installAsync(this);
    options.configure?.(this);
  }
  findPackage(designator) {
    if(designator instanceof LispPackage) return designator;
    const name=designator instanceof LispSymbol?designator.name:designator instanceof LispString?designator.toString():String(designator);
    return this.currentPackage?.localNicknames.get(name) ?? this.packages.get(name) ?? null;
  }
  makePackage(name, options = {}) {
    name=String(name); if(this.packages.has(name)) throw new LispCondition('PACKAGE-ERROR',`Package ${name} already exists`);
    const pkg=new LispPackage(name,options); pkg.uses=pkg.uses.map(use=>this.findPackage(use)??use); this.packages.set(name,pkg);
    for(const nickname of pkg.nicknames) this.packages.set(nickname,pkg); return pkg;
  }
  findSymbol(name, pkg = this.currentPackage) {
    pkg=this.findPackage(pkg); if(!pkg) return null;
    if(pkg.symbols.has(name)) return pkg.symbols.get(name);
    for(const use of pkg.uses) { const source=this.findPackage(use); if(source?.exports.has(name)) return source.symbols.get(name); }
    return null;
  }
  findExternalSymbol(name,pkg) { pkg=this.findPackage(pkg); return pkg?.exports.has(name)?pkg.symbols.get(name):null; }
  intern(name, pkg = this.currentPackage) {
    name=String(name); pkg=this.findPackage(pkg); if(!pkg) throw new LispCondition('PACKAGE-ERROR','Package does not exist');
    const existing=this.findSymbol(name,pkg); if(existing) return existing;
    const sym=new LispSymbol(name,pkg); if(pkg.name==='KEYWORD') { sym.value=sym; sym.constant=true; pkg.exports.add(name); } pkg.symbols.set(name,sym); return sym;
  }
  exportSymbol(sym,pkg=sym.package) { pkg=this.findPackage(pkg); pkg.symbols.set(sym.name,sym); pkg.exports.add(sym.name); return sym; }
  checkMutableSymbol(sym) { if(!(sym instanceof LispSymbol)) throw new LispCondition('TYPE-ERROR','Expected a symbol',{datum:sym}); if(sym.constant) throw new LispCondition('PROGRAM-ERROR',`Cannot bind or assign constant ${sym.name}`); return sym; }
  variableValue(sym,task) { const stack=task?.dynamic.get(sym); return stack?.length?stack.at(-1):sym.value; }
  readVariable(sym,task) { const value=this.variableValue(sym,task); if(value===UNBOUND) throw new LispCondition('UNBOUND-VARIABLE',`Unbound variable ${sym.name}`,{datum:sym,cellName:sym}); return value; }
  writeVariable(sym,value,task=this.activeTask) { this.checkMutableSymbol(sym); const stack=task?.dynamic.get(sym); if(stack?.length) stack[stack.length-1]=value; else { sym.value=value; if(sym===this.packages.get('COMMON-LISP').symbols.get('*PACKAGE*')) this.currentPackage=value; } return value; }
  getSymbolValue(sym,task) { return this.readVariable(sym,task); }
  setSymbolValue(sym,value,task) { return this.writeVariable(sym,value,task); }
  functionName(name) {
    if(typeof name==='string') {
      const setf=/^\(SETF (.+)\)$/.exec(name);
      if(setf) return fromArray([this.intern('SETF','COMMON-LISP'),this.functionName(setf[1])]);
      const qualified=/^([^:]+)(::?)([^:]+)$/.exec(name);
      if(qualified) {
        const [,pkg,separator,symbolName]=qualified;
        if(separator===':') { const found=this.findExternalSymbol(symbolName,pkg); if(!found) throw new LispCondition('PACKAGE-ERROR',`No external symbol ${name}`); return found; }
        return this.intern(symbolName,pkg);
      }
      return this.findSymbol(name,this.currentPackage) ?? this.intern(name,'COMMON-LISP');
    }
    return name;
  }
  setfName(name) { return name instanceof Cons && name.car instanceof LispSymbol && name.car.name==='SETF' && name.cdr instanceof Cons && name.cdr.cdr===NIL?name.cdr.car:null; }
  readFunction(name) {
    name=this.functionName(name); const setf=this.setfName(name); const value=setf?this.setfFunctions.get(setf):(name===NIL||name===T)?this.constantFunctions.get(name):name?.function;
    if(value===undefined || value===UNBOUND) throw new LispCondition('UNDEFINED-FUNCTION',`Undefined function ${setf?.name ?? name?.name ?? name}`,{datum:name,cellName:name}); return value;
  }
  defineFunction(name,definition,isMacro=false) {
    name=this.functionName(name); const setf=this.setfName(name);
    if(setf) { this.setfFunctions.set(setf,definition); return definition; }
    if(!(name instanceof LispSymbol)) throw new LispCondition('TYPE-ERROR','Expected a function name',{datum:name});
    if(name===NIL||name===T) { if(isMacro){this.constantMacros.set(name,definition);this.constantFunctions.delete(name);}else{this.constantFunctions.set(name,definition);this.constantMacros.delete(name);} }
    else if(isMacro) {name.macro=definition;name.function=UNBOUND;} else {name.function=definition;name.macro=NIL;}
    return definition;
  }
  macroFunction(name) { return (name===NIL||name===T)?this.constantMacros.get(name)??NIL:name?.macro ?? NIL; }
  defineBuiltin(name,fn,metadata={}) {
    const setf=/^\(SETF (.+)\)$/.exec(name);
    const sym=setf?fromArray([this.intern('SETF','COMMON-LISP'),this.intern(setf[1],'COMMON-LISP')]):this.intern(name,'COMMON-LISP');
    const builtin=new NativeFunction(name,fn,metadata); if(sym instanceof LispSymbol) this.exportSymbol(sym,'COMMON-LISP'); this.defineFunction(sym,builtin);
    if(metadata.primitive && !this.primitives.has(metadata.primitive)) this.primitives.set(metadata.primitive,builtin); return builtin;
  }
  registerFunction(name,fn,metadata={}) { if(typeof name==='string'&&!name.includes(':')) return this.defineBuiltin(name,fn,metadata); const builtin=new NativeFunction(name,fn,metadata); this.defineFunction(name,builtin); return builtin; }
  primitive(name) { const direct=this.primitives.get(name); if(direct) return direct; const symbol=this.findSymbol(name,'COMMON-LISP'); if(symbol?.function!==UNBOUND && symbol?.function!==undefined) return symbol.function; throw new LispCondition('UNDEFINED-FUNCTION',`Unknown serialized primitive ${name}`,{cellName:symbol ?? new LispSymbol(name)}); }
  resolveFunction(designator) { return typeof designator==='string'||designator instanceof LispSymbol||designator instanceof Cons?this.readFunction(designator):designator; }
  multipleValues(...values) { return new MultipleValues(values); }
  requestCall(callee,args=[]) { return new CallRequest(callee,args); }
  makeCode(metadata,{constants=metadata.constants??[]}={}) { const code=new LispCode(metadata,{name:metadata.name,arity:metadata.arity}); code.constants=constants; return code; }
  makeFunction(metadata,{constants=metadata.constants??[],captures=[]}={}) { return new Closure(metadata instanceof LispCode?metadata:this.makeCode(metadata,{constants}),captures); }
  executeCode(metadata,constants=metadata.constants??[],args=[]) { return this.call(this.makeFunction(metadata,{constants}),args); }
  createTask(designator,args=[]) { return new Task(this,designator,args); }
  call(designator,args=[],task=this.activeTask ?? null) {
    if(task) return task.call(designator,args);
    const execution=this.createTask(designator,args); execution.run(); if(execution.suspended) throw new SuspensionError(execution);
    this.values=execution.values.slice(); return new MultipleValues(this.values.slice());
  }
  invoke(designator,args=[],task=null) { return this.call(designator,args,task); }
  funcall(designator,args=[],task=null) { return this.call(designator,args,task); }
  async callAsync(designator, args = [], options = {}) {
    if (options.signal?.aborted) throw options.signal.reason;
    const result = await this.createTask(designator, args).runAsync(options);
    this.values = result.values.slice();
    return result;
  }
  loadFunction(program) {
    return new NativeFunction('LOAD-COMPILED-FILE', function* (_args, runtime, task) {
      task.bind(task.currentFrame, readerBindings(runtime, task));
      return yield* runCompiledProgram(runtime, program);
    });
  }
  load(program) { return this.call(this.loadFunction(program)); }
  async loadAsync(program,options={}) { return this.callAsync(this.loadFunction(program),[],options); }
  run() { return new MultipleValues(this.result.values.slice()); }
  conditionClassMatches(condition,test,constants=[]) {
    if(test==='Any') return true; if(test==='Never') return false;
    const [kind, value] = Object.entries(test)[0];
    const expected=this.findClass(kind==='Named'?constants[value]:classSpelling(value),false);
    return expected === null ? (kind==='Named'?undefined:false) : this.isInstanceOf(condition,expected);
  }
  signal(condition,task) { if(!task) return this.call(new NativeFunction('SIGNAL',(_a,_r,t)=>t.signal(condition)),[]); return task.signal(condition); }
  *findRestartsSteps(condition=NIL,task) { return task?yield* task.restartSteps(condition):[]; }
  invokeRestart(designator,args=[],task,interactive=false) { if(!task) throw new LispCondition('CONTROL-ERROR','No active restart task'); return task.invokeRestart(designator,args,interactive); }
  fromJS(value) { if(value==null||value===false) return NIL; if(value===true) return T; if(typeof value==='string') return new LispString(value); if(typeof value==='number') return Number.isInteger(value)?integer(value):new DoubleFloat(value); if(typeof value==='bigint') return integer(value); if(Array.isArray(value)) return new LispArray([value.length],{initialContents:value.map(x=>this.fromJS(x))}); return value; }
  toJS(value) { if(value===NIL) return null; if(value===T) return true; if(value instanceof LispString||value instanceof LispCharacter) return value.toString(); if(value instanceof SingleFloat||value instanceof DoubleFloat) return value.value; return value; }
  print(value,options) { return writeToString(value,{runtime:this,...options}); }
}
export function createRuntime(options={}) { return new Runtime(options); }
export { NIL, T, UNBOUND, LispSymbol, Cons, Cell, LispCode, Closure, LispString, LispCharacter, LispArray, LispPathname, fromArray, toArray, eq, eql, equal, integer, SingleFloat, DoubleFloat, Ratio, Complex, makeRatio, makeComplex, Task, MultipleValues, CallRequest, NativeFunction, ExitToken, LispCondition, SuspensionError };
