use wasmparser::{BlockType, FuncValidator, FunctionBody, Operator, WasmModuleResources};

use super::module::value_type;
use super::numeric::{Binary, Unary};
use super::{
    Branch, CompiledFunction, Error, MemoryOperand, Module, RegisterOp, Slot, unsupported,
};

#[derive(Clone, Copy)]
enum BlockKind {
    Function,
    Block,
    Loop,
    If { alternate: u32 },
}

#[derive(Clone, Copy)]
struct Block {
    kind: BlockKind,
    label: u32,
    first: Slot,
    branch_arity: u32,
}

#[derive(Default)]
struct Label {
    pc: Option<u32>,
    branched_to: bool,
}

#[derive(Default)]
struct Lowering {
    blocks: Vec<Block>,
    labels: Vec<Label>,
    instructions: Vec<RegisterOp>,
    offsets: Vec<usize>,
    branches: Vec<Branch>,
    reachable: bool,
    block_start: usize,
}

pub(super) fn function<R: WasmModuleResources>(
    module: &Module,
    validator: &mut FuncValidator<R>,
    body: FunctionBody<'_>,
) -> Result<CompiledFunction, Error> {
    let mut reader = body.get_binary_reader();
    validator.read_locals(&mut reader)?;
    let locals = validator.len_locals();
    for index in 0..locals {
        value_type(validator.get_local_type(index).unwrap(), body.range().start)?;
    }
    let mut lowering = Lowering {
        reachable: true,
        ..Lowering::default()
    };
    let label = lowering.new_label();
    lowering.blocks.push(Block {
        kind: BlockKind::Function,
        label,
        first: locals,
        branch_arity: module.function_type(validator.index()).results.len() as u32,
    });
    let mut instructions = wasmparser::OperatorsReader::new(reader);
    let mut max_height = 0;
    while !instructions.eof() {
        let (op, offset) = instructions.read_with_offset()?;
        let top = locals + validator.operand_stack_height();
        validator.op(offset, &op)?;
        max_height = max_height.max(validator.operand_stack_height());
        match op {
            Operator::Block { blockty } | Operator::Loop { blockty } | Operator::If { blockty } => {
                let (params, results) = match blockty {
                    BlockType::Empty => (0, 0),
                    BlockType::Type(ty) => {
                        value_type(ty, offset)?;
                        (0, 1)
                    }
                    BlockType::FuncType(index) => {
                        let ty = &module.types[index as usize];
                        (ty.params.len() as u32, ty.results.len() as u32)
                    }
                };
                let first = locals + validator.get_control_frame(0).unwrap().height as u32;
                lowering.enter_block(op, offset, top, first, params, results);
            }
            Operator::Else => lowering.alternate(offset),
            Operator::End => lowering.end(offset),
            _ if !lowering.reachable => {}
            Operator::Br { relative_depth } => {
                let branch = lowering.branch(relative_depth, top);
                lowering.push(offset, RegisterOp::Jump(branch));
                lowering.reachable = false;
            }
            Operator::BrIf { relative_depth } => {
                let branch = lowering.branch(relative_depth, top - 1);
                lowering.push(
                    offset,
                    RegisterOp::BranchIf {
                        test: top - 1,
                        branch,
                        when: true,
                    },
                );
            }
            Operator::BrTable { targets } => {
                let first = lowering.branches.len() as u32;
                for depth in targets
                    .targets()
                    .chain(std::iter::once(Ok(targets.default())))
                {
                    let branch = lowering.branch(depth?, top - 1);
                    lowering.branches.push(branch);
                }
                lowering.push(
                    offset,
                    RegisterOp::BranchTable {
                        test: top - 1,
                        first,
                        count: targets.len(),
                    },
                );
                lowering.reachable = false;
            }
            Operator::Return => {
                let branch = lowering.branch(lowering.blocks.len() as u32 - 1, top);
                lowering.push(offset, RegisterOp::Jump(branch));
                lowering.reachable = false;
            }
            Operator::ReturnCall { .. } | Operator::ReturnCallIndirect { .. } => {
                lowering.push(offset, lower_operator(module, offset, top, op)?);
                lowering.reachable = false;
            }
            Operator::Unreachable => {
                lowering.push(offset, RegisterOp::Unreachable);
                lowering.reachable = false;
            }
            Operator::LocalSet { local_index } => lowering.set_local(offset, top - 1, local_index),
            Operator::Nop
            | Operator::Drop
            | Operator::I32ReinterpretF32
            | Operator::F32ReinterpretI32
            | Operator::I64ReinterpretF64
            | Operator::F64ReinterpretI64 => {}
            op => lowering.push(offset, lower_operator(module, offset, top, op)?),
        }
    }
    instructions.finish()?;
    Ok(lowering.finish(body.range().start, locals, locals + max_height))
}

impl Lowering {
    fn push(&mut self, offset: usize, instruction: RegisterOp) {
        let instruction = match instruction {
            RegisterOp::Unary { op, from, to } => RegisterOp::Unary {
                op,
                from: self.fold_copy(from),
                to,
            },
            RegisterOp::Binary {
                op,
                left,
                right,
                to,
            } => {
                if let Some(RegisterOp::Const { to: slot, bits }) = self.last().copied()
                    && slot == right
                {
                    self.pop();
                    RegisterOp::BinaryImm {
                        op,
                        left: self.fold_copy(left),
                        right: bits,
                        to,
                    }
                } else {
                    let right = self.fold_copy(right);
                    RegisterOp::Binary {
                        op,
                        left: self.fold_copy(left),
                        right,
                        to,
                    }
                }
            }
            RegisterOp::BranchIf { test, branch, when } => {
                if let Some(RegisterOp::Unary {
                    op: Unary::I32Eqz | Unary::I64Eqz,
                    from,
                    to,
                }) = self.last().copied()
                    && to == test
                {
                    self.pop();
                    RegisterOp::BranchIf {
                        test: from,
                        branch,
                        when: !when,
                    }
                } else {
                    RegisterOp::BranchIf {
                        test: self.fold_copy(test),
                        branch,
                        when,
                    }
                }
            }
            other => other,
        };
        self.instructions.push(instruction);
        self.offsets.push(offset);
    }

    fn last(&mut self) -> Option<&mut RegisterOp> {
        self.instructions[self.block_start..].last_mut()
    }

    fn pop(&mut self) {
        self.instructions.pop();
        self.offsets.pop();
    }

    fn fold_copy(&mut self, slot: Slot) -> Slot {
        if let Some(RegisterOp::Copy { from, to }) = self.last().copied()
            && to == slot
        {
            self.pop();
            from
        } else {
            slot
        }
    }

    fn set_local(&mut self, offset: usize, from: Slot, local: Slot) {
        if let Some(
            RegisterOp::Const { to, .. }
            | RegisterOp::Unary { to, .. }
            | RegisterOp::Binary { to, .. }
            | RegisterOp::BinaryImm { to, .. },
        ) = self.last()
            && *to == from
        {
            *to = local;
            return;
        }
        let from = self.fold_copy(from);
        if from != local {
            self.push(offset, RegisterOp::Copy { from, to: local });
        }
    }

    fn new_label(&mut self) -> u32 {
        let index = self.labels.len() as u32;
        self.labels.push(Label::default());
        index
    }

    fn bind_label(&mut self, index: u32) -> bool {
        self.block_start = self.instructions.len();
        let label = &mut self.labels[index as usize];
        label.pc = Some(self.instructions.len() as u32);
        label.branched_to
    }

    fn branch(&mut self, depth: u32, top: Slot) -> Branch {
        let block = self.blocks[self.blocks.len() - 1 - depth as usize];
        self.labels[block.label as usize].branched_to = true;
        Branch {
            target: block.label,
            source: top - block.branch_arity,
            destination: block.first,
            count: block.branch_arity,
        }
    }

    fn enter_block(
        &mut self,
        op: Operator<'_>,
        offset: usize,
        top: Slot,
        first: Slot,
        params: u32,
        results: u32,
    ) {
        let label = self.new_label();
        let (kind, branch_arity) = match op {
            Operator::Block { .. } => (BlockKind::Block, results),
            Operator::Loop { .. } => {
                self.bind_label(label);
                (BlockKind::Loop, params)
            }
            Operator::If { .. } => {
                let alternate = self.new_label();
                if self.reachable {
                    self.labels[alternate as usize].branched_to = true;
                    self.push(
                        offset,
                        RegisterOp::BranchIf {
                            test: top - 1,
                            branch: jump(alternate),
                            when: false,
                        },
                    );
                }
                (BlockKind::If { alternate }, results)
            }
            _ => unreachable!(),
        };
        self.blocks.push(Block {
            kind,
            label,
            first,
            branch_arity,
        });
    }

    fn alternate(&mut self, offset: usize) {
        let block = self.blocks.last_mut().unwrap();
        let BlockKind::If { alternate } = block.kind else {
            unreachable!()
        };
        block.kind = BlockKind::Block;
        let label = block.label;
        if self.reachable {
            self.labels[label as usize].branched_to = true;
            self.push(offset, RegisterOp::Jump(jump(label)));
        }
        self.reachable = self.bind_label(alternate);
    }

    fn end(&mut self, offset: usize) {
        let block = self.blocks.pop().unwrap();
        if let BlockKind::If { alternate } = block.kind {
            self.reachable |= self.bind_label(alternate);
        }
        if !matches!(block.kind, BlockKind::Loop) {
            self.reachable |= self.bind_label(block.label);
        }
        if matches!(block.kind, BlockKind::Function) {
            self.push(offset, RegisterOp::Return);
        }
    }

    fn finish(mut self, body_offset: usize, locals: u32, slots: u32) -> CompiledFunction {
        let resolve =
            |branch: &mut Branch| branch.target = self.labels[branch.target as usize].pc.unwrap();
        for instruction in &mut self.instructions {
            match instruction {
                RegisterOp::Jump(branch) | RegisterOp::BranchIf { branch, .. } => resolve(branch),
                _ => {}
            }
        }
        for branch in &mut self.branches {
            resolve(branch);
        }
        CompiledFunction {
            body_offset,
            locals,
            slots,
            instructions: self.instructions.into_boxed_slice(),
            offsets: self.offsets.into_boxed_slice(),
            branches: self.branches.into_boxed_slice(),
        }
    }
}

fn jump(target: u32) -> Branch {
    Branch {
        target,
        source: 0,
        destination: 0,
        count: 0,
    }
}

fn lower_operator(
    module: &Module,
    offset: usize,
    top: Slot,
    op: Operator<'_>,
) -> Result<RegisterOp, Error> {
    if let Some(op) = Unary::lower(&op) {
        return Ok(RegisterOp::Unary {
            op,
            from: top - 1,
            to: top - 1,
        });
    }
    if let Some(op) = Binary::lower(&op) {
        return Ok(RegisterOp::Binary {
            op,
            left: top - 2,
            right: top - 1,
            to: top - 2,
        });
    }
    if let Some(op) = memory_access(&op, top) {
        return Ok(op);
    }
    Ok(match op {
        Operator::I32Const { value } => RegisterOp::Const {
            to: top,
            bits: u64::from(value as u32),
        },
        Operator::I64Const { value } => RegisterOp::Const {
            to: top,
            bits: value as u64,
        },
        Operator::F32Const { value } => RegisterOp::Const {
            to: top,
            bits: u64::from(value.bits()),
        },
        Operator::F64Const { value } => RegisterOp::Const {
            to: top,
            bits: value.bits(),
        },
        Operator::RefNull { hty } if hty == wasmparser::HeapType::FUNC => {
            RegisterOp::Const { to: top, bits: 0 }
        }
        Operator::RefFunc { function_index } => RegisterOp::Const {
            to: top,
            bits: u64::from(function_index) + 1,
        },
        Operator::LocalGet { local_index } => RegisterOp::Copy {
            from: local_index,
            to: top,
        },
        Operator::LocalTee { local_index } => RegisterOp::Copy {
            from: top - 1,
            to: local_index,
        },
        Operator::Select | Operator::TypedSelect { .. } => RegisterOp::Select {
            first: top - 3,
            second: top - 2,
            test: top - 1,
        },
        Operator::Call { function_index } | Operator::ReturnCall { function_index } => {
            RegisterOp::Call {
                function: function_index,
                first: top - module.function_type(function_index).params.len() as u32,
                tail: matches!(op, Operator::ReturnCall { .. }),
            }
        }
        Operator::CallIndirect {
            type_index,
            table_index,
        }
        | Operator::ReturnCallIndirect {
            type_index,
            table_index,
        } => RegisterOp::CallIndirect {
            table: table_index,
            signature: type_index,
            index: top - 1,
            first: top - 1 - module.types[type_index as usize].params.len() as u32,
            tail: matches!(op, Operator::ReturnCallIndirect { .. }),
        },
        Operator::GlobalGet { global_index } => RegisterOp::GlobalGet {
            global: global_index,
            to: top,
        },
        Operator::GlobalSet { global_index } => RegisterOp::GlobalSet {
            global: global_index,
            from: top - 1,
        },
        Operator::MemorySize { mem } => RegisterOp::MemorySize {
            memory: mem,
            to: top,
        },
        Operator::MemoryGrow { mem } => RegisterOp::MemoryGrow {
            memory: mem,
            pages: top - 1,
        },
        Operator::MemoryFill { mem } => RegisterOp::MemoryFill {
            memory: mem,
            first: top - 3,
        },
        Operator::MemoryCopy { dst_mem, src_mem } => RegisterOp::MemoryCopy {
            destination: dst_mem,
            source: src_mem,
            first: top - 3,
        },
        Operator::MemoryInit { mem, data_index } => RegisterOp::MemoryInit {
            memory: mem,
            data: data_index,
            first: top - 3,
        },
        Operator::DataDrop { data_index } => RegisterOp::DataDrop(data_index),
        Operator::TableGet { table } => RegisterOp::TableGet {
            table,
            slot: top - 1,
        },
        Operator::TableSet { table } => RegisterOp::TableSet {
            table,
            first: top - 2,
        },
        Operator::TableSize { table } => RegisterOp::TableSize { table, to: top },
        Operator::TableGrow { table } => RegisterOp::TableGrow {
            table,
            first: top - 2,
        },
        Operator::TableFill { table } => RegisterOp::TableFill {
            table,
            first: top - 3,
        },
        Operator::TableCopy {
            dst_table,
            src_table,
        } => RegisterOp::TableCopy {
            destination: dst_table,
            source: src_table,
            first: top - 3,
        },
        Operator::TableInit { table, elem_index } => RegisterOp::TableInit {
            table,
            element: elem_index,
            first: top - 3,
        },
        Operator::ElemDrop { elem_index } => RegisterOp::ElemDrop(elem_index),
        other => return Err(unsupported(offset, format!("instruction {other:?}"))),
    })
}

fn memory_access(operator: &Operator<'_>, top: Slot) -> Option<RegisterOp> {
    macro_rules! memory_ops {
        (load { $($load:ident: $load_width:expr, $sign:expr, $narrow:expr;)* }
         store { $($store:ident: $store_width:expr;)* }) => {
            match operator {
                $(Operator::$load { memarg } => Some(RegisterOp::Load(MemoryOperand {
                    memory: memarg.memory, offset: memarg.offset as u32, slot: top - 1,
                    width: $load_width, sign: $sign, narrow: $narrow,
                })),)*
                $(Operator::$store { memarg } => Some(RegisterOp::Store(MemoryOperand {
                    memory: memarg.memory, offset: memarg.offset as u32, slot: top - 2,
                    width: $store_width, sign: false, narrow: false,
                })),)*
                _ => None,
            }
        };
    }
    memory_ops! {
        load {
            I32Load: 4, false, true;
            I64Load: 8, false, false;
            F32Load: 4, false, true;
            F64Load: 8, false, false;
            I32Load8S: 1, true, true;
            I32Load8U: 1, false, true;
            I32Load16S: 2, true, true;
            I32Load16U: 2, false, true;
            I64Load8S: 1, true, false;
            I64Load8U: 1, false, false;
            I64Load16S: 2, true, false;
            I64Load16U: 2, false, false;
            I64Load32S: 4, true, false;
            I64Load32U: 4, false, false;
        }
        store {
            I32Store: 4;
            I64Store: 8;
            F32Store: 4;
            F64Store: 8;
            I32Store8: 1;
            I32Store16: 2;
            I64Store8: 1;
            I64Store16: 2;
            I64Store32: 4;
        }
    }
}
