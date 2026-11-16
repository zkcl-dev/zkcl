use std::ops::{ControlFlow, Range};
use std::sync::atomic::{AtomicU64, Ordering};

use zkcl_bytecode::dispatch::{DispatchFrame, DispatchOutcome};

use super::module::SegmentMode;
use super::{
    Branch, CompiledFunction, Error, FuncRef, Host, HostCall, Limits, MemoryOperand, Module,
    RegisterOp, Slot, Trap, Value, ValueType,
};

const PAGE_SIZE: u64 = 65_536;
const QUANTUM: usize = 16_384;
const MAX_TRAP_CALLERS: usize = 16;
static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(0);

struct Memory {
    bytes: Vec<u8>,
    max_pages: u64,
}

impl Memory {
    fn read(&self, offset: u32, len: usize) -> Result<&[u8], Trap> {
        Ok(&self.bytes
            [checked_range(u64::from(offset), len, self.bytes.len(), Trap::MemoryBounds)?])
    }

    fn write_buffer(&mut self, offset: u64, len: usize) -> Result<&mut [u8], Trap> {
        let range = checked_range(offset, len, self.bytes.len(), Trap::MemoryBounds)?;
        Ok(&mut self.bytes[range])
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Trap> {
        self.write_buffer(u64::from(offset), bytes.len())?
            .copy_from_slice(bytes);
        Ok(())
    }

    fn pages(&self) -> u32 {
        (self.bytes.len() as u64 / PAGE_SIZE) as u32
    }

    fn grow(&mut self, delta: u32, byte_limit: usize) -> Option<u32> {
        let previous = self.pages();
        let pages = u64::from(previous) + u64::from(delta);
        let len = usize::try_from(pages * PAGE_SIZE).ok()?;
        if pages > self.max_pages || len > byte_limit {
            return None;
        }
        self.bytes.try_reserve_exact(len - self.bytes.len()).ok()?;
        self.bytes.resize(len, 0);
        Some(previous)
    }

    fn load(&self, address: u64, operand: MemoryOperand) -> Result<u64, Trap> {
        let width = usize::from(operand.width);
        let range = checked_range(address, width, self.bytes.len(), Trap::MemoryBounds)?;
        let mut bytes = [0; 8];
        bytes[..width].copy_from_slice(&self.bytes[range]);
        if operand.sign && bytes[width - 1] >= 0x80 {
            bytes[width..].fill(0xff);
        }
        let bits = u64::from_le_bytes(bytes);
        Ok(if operand.narrow {
            u64::from(bits as u32)
        } else {
            bits
        })
    }

    fn store(&mut self, address: u64, width: u8, bits: u64) -> Result<(), Trap> {
        let width = usize::from(width);
        self.write_buffer(address, width)?
            .copy_from_slice(&bits.to_le_bytes()[..width]);
        Ok(())
    }
}

struct Table {
    entries: Vec<u64>,
    max_elements: usize,
}

impl Table {
    fn write(&mut self, offset: u64, entries: &[u64]) -> Result<(), Trap> {
        let range = checked_range(offset, entries.len(), self.entries.len(), Trap::TableBounds)?;
        self.entries[range].copy_from_slice(entries);
        Ok(())
    }
}

pub struct Caller<'a> {
    memories: &'a mut [Memory],
}

impl Caller<'_> {
    pub fn read_memory(&self, memory: u32, offset: u32, len: usize) -> Result<&[u8], Trap> {
        self.memories
            .get(memory as usize)
            .ok_or(Trap::MemoryBounds)?
            .read(offset, len)
    }

    pub fn write_memory(&mut self, memory: u32, offset: u32, bytes: &[u8]) -> Result<(), Trap> {
        self.write_buffer(memory, offset, bytes.len())?
            .copy_from_slice(bytes);
        Ok(())
    }

    pub fn write_buffer(
        &mut self,
        memory: u32,
        offset: u32,
        len: usize,
    ) -> Result<&mut [u8], Trap> {
        self.memories
            .get_mut(memory as usize)
            .ok_or(Trap::MemoryBounds)?
            .write_buffer(u64::from(offset), len)
    }
}

pub struct Instance<'module, H: Host> {
    id: u64,
    module: &'module Module,
    host: H,
    imports: Vec<H::Func>,
    state: InstanceState<'module>,
}

struct InstanceState<'module> {
    memories: Vec<Memory>,
    tables: Vec<Table>,
    globals: Vec<u64>,
    data: Vec<&'module [u8]>,
    elements: Vec<&'module [u64]>,
    limits: Limits,
    /// Instructions retired by this instance. Only present in `step-counter`
    /// builds, where `WasmFrame::execute` bumps it once per dispatch.
    #[cfg(feature = "step-counter")]
    executed: u64,
}

impl InstanceState<'_> {
    /// Bytes this memory may still grow by within the instance-wide budget.
    fn memory_limit(&self, index: usize) -> usize {
        let memory = &self.memories[index];
        let used_by_others = self
            .memories
            .iter()
            .map(|other| other.bytes.len())
            .sum::<usize>()
            - memory.bytes.len();
        self.limits.memory_bytes - used_by_others
    }
}

impl<'module, H: Host> Instance<'module, H> {
    pub async fn instantiate(
        module: &'module Module,
        mut host: H,
        limits: Limits,
    ) -> Result<Self, Error> {
        let imports = module
            .imports()
            .map(|import| host.link(import))
            .collect::<Result<_, _>>()?;
        let mut available = limits.memory_bytes;
        let mut memories = Vec::with_capacity(module.memories.len());
        for decl in &module.memories {
            let mut memory = Memory {
                bytes: Vec::new(),
                max_pages: decl.maximum.unwrap_or(65_536),
            };
            memory
                .grow(decl.initial as u32, available)
                .ok_or(Error::Initialize(Trap::ResourceLimit))?;
            available -= memory.bytes.len();
            memories.push(memory);
        }
        let mut available = limits.table_elements;
        let mut tables = Vec::with_capacity(module.tables.len());
        for (decl, initial) in &module.tables {
            let len = decl.initial as usize;
            if len > available {
                return Err(Error::Initialize(Trap::ResourceLimit));
            }
            let mut entries = Vec::new();
            entries
                .try_reserve_exact(len)
                .map_err(|_| Error::Initialize(Trap::ResourceLimit))?;
            entries.resize(len, *initial);
            available -= len;
            tables.push(Table {
                entries,
                max_elements: decl.maximum.unwrap_or(u64::from(u32::MAX)) as usize,
            });
        }

        let mut data = Vec::with_capacity(module.data.len());
        for segment in &module.data {
            let items: &[u8] = match segment.mode {
                SegmentMode::Active { index, offset } => {
                    memories[index as usize]
                        .write(offset, &segment.items)
                        .map_err(Error::Initialize)?;
                    &[]
                }
                SegmentMode::Passive => &segment.items,
                SegmentMode::Declarative => unreachable!(),
            };
            data.push(items);
        }
        let mut elements = Vec::with_capacity(module.elements.len());
        for segment in &module.elements {
            let items: &[u64] = match segment.mode {
                SegmentMode::Active { index, offset } => {
                    tables[index as usize]
                        .write(u64::from(offset), &segment.items)
                        .map_err(Error::Initialize)?;
                    &[]
                }
                SegmentMode::Passive => &segment.items,
                SegmentMode::Declarative => &[],
            };
            elements.push(items);
        }

        let state = InstanceState {
            memories,
            tables,
            globals: module.globals.clone(),
            data,
            elements,
            limits,
            #[cfg(feature = "step-counter")]
            executed: 0,
        };
        let mut instance = Self {
            id: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            module,
            host,
            imports,
            state,
        };
        if let Some(start) = module.start {
            instance.execute(start, &[]).await?;
        }
        Ok(instance)
    }

    pub fn func_ref(&self, export: &str) -> Option<FuncRef> {
        self.module.export_index(export).map(|index| FuncRef {
            instance: self.id,
            index,
        })
    }

    /// Instructions this instance has executed so far.
    ///
    /// Part of the `step-counter` instrument: the tally is initialized here and
    /// bumped by `WasmFrame::execute`, so it measures dispatch volume rather
    /// than wall time. Reading it never changes execution.
    #[cfg(feature = "step-counter")]
    pub fn executed_instructions(&self) -> u64 {
        self.state.executed
    }

    pub async fn invoke(&mut self, export: &str, arguments: &[Value]) -> Result<Vec<Value>, Error> {
        let function = self
            .module
            .export_index(export)
            .ok_or_else(|| Error::Export(export.into()))?;
        if !self.accepts(&self.module.function_type(function).params, arguments) {
            return Err(Error::Signature(export.into()));
        }
        self.execute(function, arguments).await
    }

    /// Whether `values` match `types` and every function reference belongs to
    /// this instance.
    fn accepts(&self, types: &[ValueType], values: &[Value]) -> bool {
        values.len() == types.len()
            && types.iter().zip(values).all(|(ty, value)| {
                value.value_type() == *ty
                    && match value {
                        Value::FuncRef(Some(reference)) => reference.instance == self.id,
                        _ => true,
                    }
            })
    }

    async fn call_import(
        &mut self,
        function: u32,
        arguments: &[Value],
        result_types: &[ValueType],
    ) -> Result<Vec<Value>, Trap> {
        let caller = Caller {
            memories: &mut self.state.memories,
        };
        let results = match self
            .host
            .call(&self.imports[function as usize], arguments, caller)?
        {
            HostCall::Ready(values) => values,
            HostCall::Pending(future) => future.await?,
        };
        if self.accepts(result_types, &results) {
            Ok(results)
        } else {
            Err(Trap::HostResults)
        }
    }

    async fn execute(&mut self, function: u32, arguments: &[Value]) -> Result<Vec<Value>, Error> {
        let module = self.module;
        let imports = self.imports.len();
        if (function as usize) < imports {
            return self
                .call_import(function, arguments, &module.function_type(function).results)
                .await
                .map_err(|source| Error::Execution {
                    frame: module.trap_frame(function, None),
                    callers: Box::new([]),
                    source,
                });
        }

        let mut stack = CallStack::default();
        let code = &module.functions[function as usize - imports];
        stack
            .push(function, code.slots, 0..0, &self.state.limits)
            .map_err(|source| Error::Execution {
                frame: module.trap_frame(function, Some(code.offsets[0])),
                callers: Box::new([]),
                source,
            })?;
        for (slot, argument) in stack.registers.iter_mut().zip(arguments) {
            *slot = argument.bits();
        }
        let mut import_arguments = Vec::new();
        let mut remaining = QUANTUM;
        loop {
            if remaining == 0 {
                zkcl_bytecode::dispatch::yield_now().await;
                remaining = QUANTUM;
            }
            let frame = stack.frames.last_mut().unwrap();
            let code = &module.functions[frame.function as usize - imports];
            let dispatch = zkcl_bytecode::dispatch::run_frame(&mut WasmFrame {
                module,
                state: &mut self.state,
                code,
                pc: &mut frame.pc,
                slots: &mut stack.registers[frame.base..],
                remaining: &mut remaining,
            });
            let base = frame.base;
            let DispatchOutcome::Exit(exit) = dispatch else {
                unreachable!("validated functions end in a return operation")
            };
            match exit.map_err(|source| stack.trap(module, imports, source))? {
                FrameExit::Yield => {}
                FrameExit::Call {
                    function,
                    first,
                    tail,
                } => {
                    if let Some(results) = self
                        .enter_call(
                            &mut stack,
                            &mut import_arguments,
                            function,
                            base,
                            first,
                            tail,
                        )
                        .await?
                    {
                        return Ok(results);
                    }
                }
                FrameExit::Return => {
                    let frame = stack.frames.pop().unwrap();
                    let types = &module.function_type(frame.function).results;
                    let start = frame.base + code.locals as usize;
                    let results = start..start + types.len();
                    if stack.frames.is_empty() {
                        return Ok(
                            decode_values(types, &stack.registers[results], self.id).collect()
                        );
                    }
                    stack.registers.copy_within(results, frame.result);
                    stack.registers.truncate(frame.base);
                }
            }
        }
    }

    /// Enter a call instruction. A tail call to an import from the outermost
    /// frame hands this invocation's results back; every other call pushes or
    /// replaces a frame and lets the dispatch loop resume.
    async fn enter_call(
        &mut self,
        stack: &mut CallStack,
        import_arguments: &mut Vec<Value>,
        function: u32,
        base: usize,
        first: Slot,
        tail: bool,
    ) -> Result<Option<Vec<Value>>, Error> {
        let module = self.module;
        let imports = self.imports.len();
        let signature = module.function_type(function);
        let params = &signature.params;
        let start = base + first as usize;
        let end = start + params.len();
        if (function as usize) < imports {
            import_arguments.clear();
            // The arguments belong to the current frame, which a tail call removes.
            import_arguments.extend(decode_values(params, &stack.registers[start..end], self.id));
            let result_slot = if tail {
                let frame = stack.frames.pop().unwrap();
                stack.registers.truncate(frame.base);
                (!stack.frames.is_empty()).then_some(frame.result)
            } else {
                Some(start)
            };
            let results = self
                .call_import(function, import_arguments.as_slice(), &signature.results)
                .await
                .map_err(|source| {
                    if tail {
                        Error::Execution {
                            frame: module.trap_frame(function, None),
                            callers: stack
                                .trap_frames(module, imports)
                                .take(MAX_TRAP_CALLERS)
                                .collect(),
                            source,
                        }
                    } else {
                        stack.trap(module, imports, source)
                    }
                })?;
            let Some(result_slot) = result_slot else {
                return Ok(Some(results));
            };
            for (index, result) in results.into_iter().enumerate() {
                stack.registers[result_slot + index] = result.bits();
            }
            return Ok(None);
        }
        let callee = &module.functions[function as usize - imports];
        if tail {
            stack
                .replace(function, callee.slots, start..end, &self.state.limits)
                .map_err(|source| stack.trap(module, imports, source))?;
        } else {
            stack
                .push(function, callee.slots, start..end, &self.state.limits)
                .map_err(|source| stack.trap(module, imports, source))?;
        }
        Ok(None)
    }
}

/// Decode raw register words into typed Wasm values.
fn decode_values(types: &[ValueType], bits: &[u64], instance: u64) -> impl Iterator<Item = Value> {
    types
        .iter()
        .zip(bits)
        .map(move |(&ty, &bits)| Value::from_bits(ty, bits, instance))
}

#[derive(Default)]
struct CallStack {
    frames: Vec<CallFrame>,
    registers: Vec<u64>,
}

struct CallFrame {
    function: u32,
    pc: usize,
    base: usize,
    result: usize,
}

impl CallStack {
    fn trap_frames<'a>(
        &'a self,
        module: &'a Module,
        imports: usize,
    ) -> impl Iterator<Item = super::TrapFrame> + 'a {
        self.frames.iter().rev().map(move |frame| {
            let code = &module.functions[frame.function as usize - imports];
            module.trap_frame(frame.function, Some(code.offsets[frame.pc - 1]))
        })
    }

    #[cold]
    fn trap(&self, module: &Module, imports: usize, source: Trap) -> Error {
        let mut frames = self.trap_frames(module, imports);
        Error::Execution {
            frame: frames.next().unwrap(),
            callers: frames.take(MAX_TRAP_CALLERS).collect(),
            source,
        }
    }

    fn replace(
        &mut self,
        function: u32,
        slots: u32,
        args: Range<usize>,
        limits: &Limits,
    ) -> Result<(), Trap> {
        let frame = self.frames.last_mut().unwrap();
        let end = frame
            .base
            .checked_add(slots as usize)
            .ok_or(Trap::ResourceLimit)?;
        if end > limits.stack_slots {
            return Err(Trap::ResourceLimit);
        }
        self.registers
            .try_reserve(end.saturating_sub(self.registers.len()))
            .map_err(|_| Trap::ResourceLimit)?;
        let first_local = frame.base + args.len();
        self.registers.copy_within(args, frame.base);
        self.registers.truncate(first_local);
        self.registers.resize(end, 0);
        frame.function = function;
        frame.pc = 0;
        Ok(())
    }

    fn push(
        &mut self,
        function: u32,
        slots: u32,
        args: Range<usize>,
        limits: &Limits,
    ) -> Result<(), Trap> {
        let base = self.registers.len();
        let end = base
            .checked_add(slots as usize)
            .ok_or(Trap::ResourceLimit)?;
        if self.frames.len() >= limits.call_depth || end > limits.stack_slots {
            return Err(Trap::ResourceLimit);
        }
        self.registers
            .try_reserve(slots as usize)
            .map_err(|_| Trap::ResourceLimit)?;
        self.frames
            .try_reserve(1)
            .map_err(|_| Trap::ResourceLimit)?;
        let result = args.start;
        self.registers.extend_from_within(args);
        self.registers.resize(end, 0);
        self.frames.push(CallFrame {
            function,
            pc: 0,
            base,
            result,
        });
        Ok(())
    }
}

enum FrameExit {
    Call {
        function: u32,
        first: Slot,
        tail: bool,
    },
    Return,
    Yield,
}

struct WasmFrame<'frame, 'module> {
    module: &'module Module,
    state: &'frame mut InstanceState<'module>,
    code: &'module CompiledFunction,
    pc: &'frame mut usize,
    slots: &'frame mut [u64],
    remaining: &'frame mut usize,
}

impl DispatchFrame for WasmFrame<'_, '_> {
    type Instruction = RegisterOp;
    type Exit = Result<FrameExit, Trap>;

    #[inline]
    fn instructions(&self) -> &[RegisterOp] {
        &self.code.instructions
    }

    #[inline]
    fn pc(&mut self) -> &mut usize {
        self.pc
    }

    #[inline]
    fn execute(&mut self, _pc: usize, op: RegisterOp) -> ControlFlow<Self::Exit> {
        *self.remaining -= 1;
        #[cfg(feature = "step-counter")]
        {
            self.state.executed += 1;
        }
        match self.step(op) {
            Ok(None) => ControlFlow::Continue(()),
            Ok(Some(exit)) => ControlFlow::Break(Ok(exit)),
            Err(trap) => ControlFlow::Break(Err(trap)),
        }
    }

    #[inline]
    fn checkpoint(&mut self) -> ControlFlow<Self::Exit> {
        if *self.remaining == 0 {
            ControlFlow::Break(Ok(FrameExit::Yield))
        } else {
            ControlFlow::Continue(())
        }
    }
}

impl WasmFrame<'_, '_> {
    #[inline(always)]
    fn jump(&mut self, branch: Branch) {
        let start = branch.source as usize;
        let end = start + branch.count as usize;
        if branch.count != 0 && branch.source != branch.destination {
            self.slots
                .copy_within(start..end, branch.destination as usize);
        }
        *self.pc = branch.target as usize;
    }

    #[inline(always)]
    fn step(&mut self, op: RegisterOp) -> Result<Option<FrameExit>, Trap> {
        match op {
            RegisterOp::Copy { from, to } => self.slots[to as usize] = self.slots[from as usize],
            RegisterOp::Const { to, bits } => self.slots[to as usize] = bits,
            RegisterOp::Unary { op, from, to } => {
                self.slots[to as usize] = op.eval(self.slots[from as usize])?;
            }
            RegisterOp::Binary {
                op,
                left,
                right,
                to,
            } => {
                self.slots[to as usize] =
                    op.eval(self.slots[left as usize], self.slots[right as usize])?;
            }
            RegisterOp::BinaryImm {
                op,
                left,
                right,
                to,
            } => {
                self.slots[to as usize] = op.eval(self.slots[left as usize], right)?;
            }
            RegisterOp::Select {
                first,
                second,
                test,
            } => {
                if self.slots[test as usize] == 0 {
                    self.slots[first as usize] = self.slots[second as usize];
                }
            }
            RegisterOp::Jump(branch) => self.jump(branch),
            RegisterOp::BranchIf { test, branch, when } => {
                if (self.slots[test as usize] != 0) == when {
                    self.jump(branch);
                }
            }
            RegisterOp::BranchTable { test, first, count } => {
                let index = self.slots[test as usize] as u32;
                self.jump(self.code.branches[(first + index.min(count)) as usize]);
            }
            RegisterOp::Call {
                function,
                first,
                tail,
            } => {
                return Ok(Some(FrameExit::Call {
                    function,
                    first,
                    tail,
                }));
            }
            RegisterOp::CallIndirect {
                table,
                signature,
                index,
                first,
                tail,
            } => {
                let table = &self.state.tables[table as usize];
                let encoded = *table
                    .entries
                    .get(self.slots[index as usize] as usize)
                    .ok_or(Trap::TableBounds)?;
                let function = encoded.checked_sub(1).ok_or(Trap::NullFunction)? as u32;
                if !self.module.matches_signature(function, signature) {
                    return Err(Trap::IndirectSignature);
                }
                return Ok(Some(FrameExit::Call {
                    function,
                    first,
                    tail,
                }));
            }
            RegisterOp::Return => return Ok(Some(FrameExit::Return)),
            RegisterOp::Unreachable => return Err(Trap::Unreachable),
            RegisterOp::GlobalGet { global, to } => {
                self.slots[to as usize] = self.state.globals[global as usize]
            }
            RegisterOp::GlobalSet { global, from } => {
                self.state.globals[global as usize] = self.slots[from as usize]
            }
            RegisterOp::Load(operand) => {
                let address = self.slots[operand.slot as usize] + u64::from(operand.offset);
                self.slots[operand.slot as usize] =
                    self.state.memories[operand.memory as usize].load(address, operand)?;
            }
            RegisterOp::Store(operand) => {
                let first = operand.slot as usize;
                let [base, bits]: [u64; 2] = self.slots[first..first + 2].try_into().unwrap();
                let address = base + u64::from(operand.offset);
                self.state.memories[operand.memory as usize].store(address, operand.width, bits)?;
            }
            RegisterOp::MemorySize { memory, to } => {
                self.slots[to as usize] = u64::from(self.state.memories[memory as usize].pages())
            }
            RegisterOp::MemoryGrow { memory, pages } => {
                let memory = memory as usize;
                let limit = self.state.memory_limit(memory);
                let delta = self.slots[pages as usize] as u32;
                let previous = self.state.memories[memory]
                    .grow(delta, limit)
                    .unwrap_or(u32::MAX);
                self.slots[pages as usize] = u64::from(previous);
            }
            RegisterOp::MemoryFill { memory, first } => {
                let [offset, value, len] = self.operands(first);
                self.state.memories[memory as usize]
                    .write_buffer(offset, len as usize)?
                    .fill(value as u8);
            }
            RegisterOp::MemoryCopy {
                destination,
                source,
                first,
            } => {
                let [target, origin, len] = self.operands(first);
                copy_range(
                    &mut self.state.memories,
                    (destination, target),
                    (source, origin),
                    len as usize,
                    |memory| &mut memory.bytes,
                    Trap::MemoryBounds,
                )?;
            }
            RegisterOp::MemoryInit {
                memory,
                data,
                first,
            } => {
                let [target, origin, len] = self.operands(first);
                let data = self.state.data[data as usize];
                let range = checked_range(origin, len as usize, data.len(), Trap::MemoryBounds)?;
                self.state.memories[memory as usize].write(target as u32, &data[range])?;
            }
            RegisterOp::DataDrop(data) => self.state.data[data as usize] = &[],
            RegisterOp::TableGet { table, slot } => {
                let slot = &mut self.slots[slot as usize];
                *slot = *self.state.tables[table as usize]
                    .entries
                    .get(*slot as usize)
                    .ok_or(Trap::TableBounds)?;
            }
            RegisterOp::TableSet { table, first } => {
                let position = self.slots[first as usize] as usize;
                *self.state.tables[table as usize]
                    .entries
                    .get_mut(position)
                    .ok_or(Trap::TableBounds)? = self.slots[first as usize + 1];
            }
            RegisterOp::TableSize { table, to } => {
                self.slots[to as usize] = self.state.tables[table as usize].entries.len() as u64
            }
            RegisterOp::TableGrow { table, first } => {
                let initial = self.slots[first as usize];
                let delta = self.slots[first as usize + 1] as usize;
                let available = self.state.limits.table_elements
                    - self
                        .state
                        .tables
                        .iter()
                        .map(|table| table.entries.len())
                        .sum::<usize>();
                let table = &mut self.state.tables[table as usize];
                let previous = table.entries.len();
                let result = if delta <= available
                    && delta <= table.max_elements - previous
                    && table.entries.try_reserve_exact(delta).is_ok()
                {
                    table.entries.resize(previous + delta, initial);
                    previous as u32
                } else {
                    u32::MAX
                };
                self.slots[first as usize] = u64::from(result);
            }
            RegisterOp::TableFill { table, first } => {
                let [offset, value, len] = self.operands(first);
                let entries = &mut self.state.tables[table as usize].entries;
                let range = checked_range(offset, len as usize, entries.len(), Trap::TableBounds)?;
                entries[range].fill(value);
            }
            RegisterOp::TableCopy {
                destination,
                source,
                first,
            } => {
                let [target, origin, len] = self.operands(first);
                copy_range(
                    &mut self.state.tables,
                    (destination, target),
                    (source, origin),
                    len as usize,
                    |table| &mut table.entries,
                    Trap::TableBounds,
                )?;
            }
            RegisterOp::TableInit {
                table,
                element,
                first,
            } => {
                let [target, origin, len] = self.operands(first);
                let elements = self.state.elements[element as usize];
                let source =
                    checked_range(origin, len as usize, elements.len(), Trap::TableBounds)?;
                self.state.tables[table as usize].write(target, &elements[source])?;
            }
            RegisterOp::ElemDrop(element) => self.state.elements[element as usize] = &[],
        }
        Ok(None)
    }

    fn operands(&self, first: Slot) -> [u64; 3] {
        let first = first as usize;
        self.slots[first..first + 3].try_into().unwrap()
    }
}

fn checked_range(offset: u64, len: usize, size: usize, trap: Trap) -> Result<Range<usize>, Trap> {
    let Ok(start) = usize::try_from(offset) else {
        return Err(trap);
    };
    let end = start
        .checked_add(len)
        .filter(|end| *end <= size)
        .ok_or(trap)?;
    Ok(start..end)
}

fn copy_range<S, T: Copy>(
    storage: &mut [S],
    (destination, target): (u32, u64),
    (source, origin): (u32, u64),
    len: usize,
    items: impl Fn(&mut S) -> &mut [T],
    trap: Trap,
) -> Result<(), Trap> {
    let (destination, source) = (destination as usize, source as usize);
    if destination == source {
        let items = items(&mut storage[source]);
        let from = checked_range(origin, len, items.len(), trap.clone())?;
        let to = checked_range(target, len, items.len(), trap)?;
        items.copy_within(from, to.start);
    } else {
        let (low, high) = storage.split_at_mut(destination.max(source));
        let (from, to) = if destination < source {
            (items(&mut high[0]), items(&mut low[destination]))
        } else {
            (items(&mut low[source]), items(&mut high[0]))
        };
        let from = &from[checked_range(origin, len, from.len(), trap.clone())?];
        let range = checked_range(target, len, to.len(), trap)?;
        to[range].copy_from_slice(from);
    }
    Ok(())
}
