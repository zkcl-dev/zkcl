import { NIL, T, UNBOUND, LispSymbol, Cons, Cell, LispCode, LispString, LispCharacter, LispArray, LispPathname, Closure } from './objects.mjs';
import { integer, RandomState, makeRatio, makeComplex, singleFromBits, doubleFromBits } from './numbers.mjs';
import { NativeFunction, LispCondition, MultipleValues, variant } from './execution.mjs';
import { installBuiltinNative } from './installers.mjs';
import { LispHashTable, hashTests } from './hash_tables.mjs';

/** Materialize one shared literal graph for this execution of the program. */
function linkProgram(runtime, program) {
  if (!program || !Array.isArray(program.nodes) || !Array.isArray(program.entries)) throw new TypeError('Expected a CompiledFile program');
  if (program.target !== 'js') throw new LispCondition('PROGRAM-ERROR', `Compiled file target ${program.target} is not JavaScript`);
  const { nodes, entries } = program;
  const empty = Symbol('unmaterialized');
  const values = Array(nodes.length).fill(empty), filling = new Set(), pending = new Map(), work = [];
  const creations = new Map(), loadFormNodes = new Set(), loadFormEntries = new Set();
  const linkedHashTables = [];
  const rehashHint=value=>{const bits=BigInt(value);return bits>>63n?doubleFromBits(bits&0x7fffffffffffffffn):integer(bits);};
  for (let index = 0; index < nodes.length; index++) {
    const descriptor = nodes[index]?.LoadForm;
    if (!descriptor) continue;
    const { creation, initialization } = descriptor;
    if (!Number.isSafeInteger(creation) || creation < 0 || creation >= entries.length || loadFormEntries.has(creation)) {
      throw new TypeError(`Invalid LOAD-FORM creation entry ${creation}`);
    }
    creations.set(creation, index);
    loadFormNodes.add(index);
    loadFormEntries.add(creation);
    if (initialization != null) {
      if (!Number.isSafeInteger(initialization) || initialization <= creation || initialization >= entries.length || loadFormEntries.has(initialization)) {
        throw new TypeError(`Invalid LOAD-FORM initialization entry ${initialization}`);
      }
      loadFormEntries.add(initialization);
    }
  }
  function check(index) { if (!Number.isSafeInteger(index) || index < 0 || index >= nodes.length) throw new TypeError(`Invalid literal reference ${index}`); }
  function reference(index, assign) {
    check(index);
    if (values[index] !== empty) assign(values[index]);
    else if (loadFormNodes.has(index)) {
      let callbacks = pending.get(index);
      if (!callbacks) pending.set(index, callbacks = []);
      callbacks.push(assign);
    } else work.push({ index, assign });
  }
  function drain() { while(work.length) { const {index,assign}=work.pop();assign(materialize(index)); } }
  function code(metadata, publish = () => {}) {
    if (!metadata || !Array.isArray(metadata.constants) || !Number.isSafeInteger(metadata.registers) || metadata.registers < 0) throw new TypeError('Invalid function metadata');
    if (typeof metadata.step !== 'function' && !Array.isArray(metadata.instructions)) throw new TypeError('Function has no executable body');
    const constants = Array(metadata.constants.length).fill(UNBOUND);
    const result = runtime.makeCode(metadata, { constants });
    publish(result);
    metadata.constants.forEach((index, n) => reference(index, value => { constants[n] = value; }));
    return result;
  }
  function materialize(index) {
    check(index);
    if (values[index] !== empty) return values[index];
    if (filling.has(index)) throw new TypeError('Cyclic immutable literal');
    filling.add(index);
    const [kind, p] = variant(nodes[index]);
    let value;
    switch (kind) {
      case 'Unbound': value=UNBOUND; break;
      case 'Nil': value = NIL; break;
      case 'True': value = T; break;
      case 'Fixnum': value = integer(p); break;
      case 'Character': value = new LispCharacter(p); break;
      case 'String': value = new LispString(p); break;
      case 'InternedSymbol': {
        const pkg=runtime.packages.get(p.package);
        if(!pkg) throw new LispCondition('PACKAGE-ERROR',`Compiled file references missing package ${p.package}`);
        value=runtime.intern(p.name,pkg);
        if(value===NIL||value===T||value.package!==pkg) throw new LispCondition('PACKAGE-ERROR',`Compiled symbol ${p.name} has an incompatible home package`);
        break;
      }
      case 'UninternedSymbol': value = new LispSymbol(p); break;
      case 'Package': value = runtime.packages.get(p); if (!value) throw new LispCondition('PACKAGE-ERROR', `No package ${p}`); break;
      case 'Pair':
        value = new Cons(NIL); values[index] = value;
        reference(p[0], v => { value.car = v; }); reference(p[1], v => { value.cdr = v; }); break;
      case 'LexicalCell':
        value = new Cell(); values[index] = value; reference(p, v => { value.value = v; }); break;
      case 'Function': value = code(p, result => { values[index]=result; }); break;
      case 'Closure': {
        const c = materialize(p.code); value = new Closure(c, Array(p.captures.length).fill(UNBOUND)); values[index] = value;
        p.captures.forEach((i,n) => reference(i,v=>{value.environment[n]=v;})); reference(p.documentation,v=>{value.documentation=v;}); break;
      }
      case 'Primitive': value = runtime.primitive(p); break;
      case 'BuiltinNative': value = new NativeFunction(`installer:${p}`, (args, rt, task) => installBuiltinNative(p,args,rt,task)); break;
      case 'Bignum': { let magnitude=0n; for(let i=p.magnitude.length-1;i>=0;i--) magnitude=(magnitude<<8n)|BigInt(p.magnitude[i]); value=integer(p.negative?-magnitude:magnitude); break; }
      case 'Ratio': value = makeRatio(materialize(p[0]),materialize(p[1])); break;
      case 'SingleFloat': value = singleFromBits(p); break;
      case 'DoubleFloat': value = doubleFromBits(p); break;
      case 'Complex': value = makeComplex(materialize(p[0]),materialize(p[1])); break;
      case 'Pathname': {
        // Validated FASL pathname components contain only scalar literals and
        // a proper directory list, whose cons cells the graph worklist fills.
        value = new LispPathname(p.map(materialize));
        break;
      }
      case 'HashTable': {
        const test = p.test.toUpperCase();
        if (!Object.hasOwn(hashTests, test)) throw new TypeError(`Invalid hash table test ${p.test}`);
        if (p.weakness != null) throw new LispCondition('PROGRAM-ERROR', 'Weak hash tables cannot be loaded into the JavaScript runtime');
        value = new LispHashTable(test);
        value.rehashSize = rehashHint(p.rehash_size); value.rehashThreshold = rehashHint(p.rehash_threshold);
        values[index] = value; linkedHashTables.push(value);
        value.restoreEntries(p.entries.map(([k, v]) => {
          const pair = [UNBOUND, UNBOUND];
          reference(k, key => { pair[0] = key; }); reference(v, val => { pair[1] = val; }); return pair;
        }));
        break;
      }
      case 'Array': {
        const [contents, data]=variant(p.contents);
        if(contents==='Displaced') value=new LispArray(p.dimensions,{elementType:p.element,adjustable:p.adjustable,fillPointer:p.fill_pointer,displacedTo:materialize(data.target),displacedIndexOffset:data.offset});
        else {
          value=new LispArray(p.dimensions,{elementType:p.element,adjustable:p.adjustable,fillPointer:p.fill_pointer}); values[index]=value;
          if(data.length!==value.totalSize) throw new TypeError('Literal array size does not match dimensions');
          data.forEach((item,n)=>{ if(contents==='Elements') reference(item,v=>{value.set(n,v);}); else if(contents==='Octets') value.set(n,item); else if(contents==='SingleFloats') value.set(n,singleFromBits(item)); else if(contents==='DoubleFloats') value.set(n,doubleFromBits(item)); else throw new TypeError(`Invalid array contents ${contents}`); });
        }
        break;
      }
      case 'RandomState': value=new RandomState(p.map(BigInt)); break;
      case 'SimdPack': { const [width,bytes]=variant(p.bits); value={kind:'simd-pack',elementKind:p.kind,width,bytes:Uint8Array.from(bytes.flat(Infinity))}; break; }
      case 'SimdMask': value={kind:'simd-mask',lanes:p.lanes,bits:BigInt(p.bits)}; break;
      case 'LoadForm': throw new TypeError(`Unresolved LOAD-FORM literal ${index}`);
      default: throw new TypeError(`Unknown literal node ${kind}`);
    }
    values[index]=value; filling.delete(index); return value;
  }
  return {
    entryCount: entries.length,
    linkEntry(index) {
      // Earlier DEFPACKAGE entries can establish packages used by this code.
      const entry = new Closure(code(entries[index]), []);
      drain();
      return entry;
    },
    afterEntry(index, result) {
      const node = creations.get(index);
      if (node === undefined) return;
      const value = result.values[0] ?? NIL;
      values[node] = value;
      const patches = pending.get(node);
      if (!patches) return;
      pending.delete(node);
      for (const assign of patches) assign(value);
      for (const table of linkedHashTables) table.keysPatched();
    },
    isLoadForm: index => loadFormEntries.has(index),
    finish() { if (pending.size) throw new TypeError('Unresolved LOAD-FORM literal'); },
  };
}

// The caller binds the reader environment for its entire load operation.
export function* runCompiledProgram(runtime, program, report) {
  const linked = linkProgram(runtime, program);
  let result = new MultipleValues();
  for (let index = 0; index < linked.entryCount; index++) {
    result = yield runtime.requestCall(linked.linkEntry(index));
    linked.afterEntry(index, result);
    if (report && !linked.isLoadForm(index)) yield* report(result);
  }
  linked.finish();
  runtime.result = result;
  runtime.values = result.values.slice();
  return result;
}
