import { NIL, T, UNBOUND, LispSymbol, Cons, Cell, LispCode, LispString, LispCharacter, LispArray, LispPathname, Closure } from './objects.mjs';
import { integer, Ratio, Complex, SingleFloat, DoubleFloat, RandomState, makeRatio, makeComplex, singleFromBits, doubleFromBits, toNumber } from './numbers.mjs';
import { NativeFunction, LispCondition, variant } from './execution.mjs';
import { installBuiltinNative } from './builtins.mjs';
import { LispHashTable, hashTests } from './hash_tables.mjs';
import { generateStep, stepIdentifier } from './codegen.mjs';

/** Materialize one shared literal graph. References retain object identity. */
export function loadProgram(runtime, program) {
  if (!program || !Array.isArray(program.nodes) || !Array.isArray(program.entries)) throw new TypeError('Expected a CompiledFile program');
  if (program.target !== 'js') throw new LispCondition('PROGRAM-ERROR', `Compiled file target ${program.target} is not JavaScript`);
  const nodes = program.nodes, empty=Symbol('unmaterialized'), values = Array(nodes.length).fill(empty), filling = new Set(), pending = new Map(), loadForms = new Map(), work=[];
  const linkedHashTables = [];
  const rehashHint=value=>{const bits=BigInt(value);return bits>>63n?doubleFromBits(bits&0x7fffffffffffffffn):integer(bits);};
  function check(index) { if (!Number.isSafeInteger(index) || index < 0 || index >= nodes.length) throw new TypeError(`Invalid literal reference ${index}`); }
  function reference(index, assign) {
    check(index);
    const [kind] = variant(nodes[index]);
    if (kind === 'LoadForm' && values[index] === empty) { let callbacks = pending.get(index); if (!callbacks) pending.set(index, callbacks = []); callbacks.push(assign); return; }
    work.push({index,assign});
  }
  function drain() { while(work.length) { const {index,assign}=work.pop();assign(materialize(index)); } }
  function code(metadata, publish = () => {}) {
    if (!metadata || !Array.isArray(metadata.constants) || !Number.isSafeInteger(metadata.registers)) throw new TypeError('Invalid function metadata');
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
      case 'LoadForm': {
        const list=loadForms.get(p.creation) ?? []; list.push(index); loadForms.set(p.creation,list); filling.delete(index); return UNBOUND;
      }
      default: throw new TypeError(`Unknown literal node ${kind}`);
    }
    values[index]=value; filling.delete(index); return value;
  }
  // Like the Rust FASL loader, link only the entry about to run. Earlier
  // DEFPACKAGE entries may establish packages used by later literal symbols.
  for(let i=0;i<nodes.length;i++) {
    const [kind,p]=variant(nodes[i]);
    if(kind==='LoadForm') { const list=loadForms.get(p.creation)??[]; if(!list.includes(i)) list.push(i); loadForms.set(p.creation,list); }
  }
  const entries=Array(program.entries.length);
  program.entries.forEach((metadata,index)=>{
    let entry;
    Object.defineProperty(entries,index,{get(){if(!entry){entry=new Closure(code(metadata),[]);drain();}return entry;},enumerable:true});
  });
  return {
    program, nodes:values, entries,
    afterEntry(index, result) {
      for(const node of loadForms.get(index) ?? []) {
        const value=result.values[0] ?? NIL; values[node]=value; const patches = pending.get(node);
        if (patches) {
          for (const assign of patches) assign(value);
          for (const table of linkedHashTables) table.keysPatched();
          pending.delete(node);
        }
      }
    },
    finish() { if(pending.size) throw new TypeError('Unresolved LOAD-FORM literal'); },
  };
}

const arrayElements = {
  general: 'General', character: 'Character', bit: 'Bit', octet: 'Octet', single: 'SingleFloat', double: 'DoubleFloat',
};
export const compiledModuleHeader = '// Generated by zkcl-js.';

/** Emit the same portable program graph consumed by loadProgram and exported
 * by the Rust emitter. Receiving runtimes own every materialized Lisp object.
 */
export function emitResidentModule(runtime, { entries, loadTimeCells }, sourcePath) {
  const nodes = [], indices = new Map(), pending = [];
  const ref = value => {
    if (!indices.has(value)) { indices.set(value, pending.length); pending.push(value); }
    return indices.get(value);
  };
  const codeSource = (code, constants) => {
    const { instructions, ...metadata } = code;
    const { source, ...step } = generateStep(code);
    const encoded = JSON.stringify({ ...metadata, ...step, constants: constants.map(ref) });
    return `${encoded.slice(0, -1)},"step": function ${stepIdentifier(code.name)}(task, frame) {\n${source}\n}}`;
  };
  const unsupported = value => { throw new LispCondition('PROGRAM-ERROR', `Cannot store ${value?.constructor?.name ?? typeof value} in a JavaScript compiled file`, { datum: value }); };

  function encode(value) {
    if (value === NIL) return 'Nil';
    if (value === T) return 'True';
    if (value === UNBOUND) return 'Unbound';
    if (Number.isInteger(value) || typeof value === 'bigint') {
      if (typeof value === 'number' && value >= -268435456 && value <= 268435455) return { Fixnum: String(value) };
      if (typeof value === 'number' && !Number.isSafeInteger(value)) return unsupported(value);
      const n = BigInt(value), magnitude = [];
      for (let remaining = n < 0n ? -n : n; remaining > 0n; remaining >>= 8n) magnitude.push(Number(remaining & 255n));
      return { Bignum: { negative: n < 0n, magnitude } };
    }
    if (value instanceof LispCharacter) return { Character: value.toString() };
    if (value instanceof LispString) return { String: value.toString() };
    if (value instanceof LispSymbol) return value.package ? { InternedSymbol: { package: value.package.name, name: value.name } } : { UninternedSymbol: value.name };
    if (typeof value?.name === 'string' && runtime.packages.get(value.name) === value) return { Package: value.name };
    if (value instanceof Cons) return { Pair: [ref(value.car), ref(value.cdr)] };
    if (value instanceof Cell) return { LexicalCell: ref(loadTimeCells.has(value) ? UNBOUND : value.value) };
    if (value instanceof Closure) return { Closure: { code: ref(value.code), captures: value.environment.map(ref), documentation: ref(value.documentation ?? NIL) } };
    if (value instanceof LispPathname) return { Pathname: value.components.map(ref) };
    if (value instanceof SingleFloat) return { SingleFloat: value.bits };
    if (value instanceof DoubleFloat) return { DoubleFloat: String(value.bits) };
    if (value instanceof RandomState) return { RandomState: value.words.map(String) };
    if (value instanceof Ratio) return { Ratio: [ref(value.numerator), ref(value.denominator)] };
    if (value instanceof Complex) return { Complex: [ref(value.real), ref(value.imaginary)] };
    if (value instanceof LispHashTable) {
      const hint = value => typeof value === 'number' || typeof value === 'bigint'
        ? String(BigInt(value) < 0x8000000000000000n ? value : 0x7fffffffffffffffn)
        : String(0x8000000000000000n | new DoubleFloat(toNumber(value)).bits);
      return { HashTable: { test: { EQ: 'Eq', EQL: 'Eql', EQUAL: 'Equal', EQUALP: 'Equalp' }[value.testName],
        weakness: null, rehash_size: hint(value.rehashSize), rehash_threshold: hint(value.rehashThreshold),
        entries: Array.from(value.entries(), ([k, v]) => [ref(k), ref(v)]) } };
    }
    if (value instanceof LispArray) {
      let contents;
      if (value.displacedTo) contents = { Displaced: { target: ref(value.displacedTo), offset: value.displacedIndexOffset } };
      else if (value.elementKind === 'octet') contents = { Octets: value.storage.map(Number) };
      else if (value.elementKind === 'single') contents = { SingleFloats: value.storage.map(x => x.bits) };
      else if (value.elementKind === 'double') contents = { DoubleFloats: value.storage.map(x => String(x.bits)) };
      else contents = { Elements: value.storage.map(ref) };
      return { Array: { dimensions: value.dimensions, element: arrayElements[value.elementKind], adjustable: value.adjustable, fill_pointer: value.fillPointer, contents } };
    }
    return unsupported(value);
  }

  const entrySources = entries.map(code => codeSource(code, code.constants));
  for (let i = 0; i < pending.length; i++) {
    const value = pending[i];
    nodes.push(value instanceof LispCode ? `{"Function":${codeSource(value.entry, value.constants)}}` : JSON.stringify(encode(value)));
  }
  const sourceFiles = sourcePath ? [{ uri: null, version: null, base_offset: 0, path: sourcePath, text: null }] : [];
  return `${compiledModuleHeader} This module contains compiled Lisp code.\nexport const program = {"target":"js","source_files":${JSON.stringify(sourceFiles)},"nodes":[\n${nodes.join(',\n')}\n],"entries":[\n${entrySources.join(',\n')}\n]};\nexport const programs = [program];\n`;
}
