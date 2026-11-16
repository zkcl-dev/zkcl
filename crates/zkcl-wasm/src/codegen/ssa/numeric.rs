use zkcl_bytecode::native_abi::FixnumPrimitive;
use zkcl_bytecode::{FIXNUM_SHIFT, FixnumArithmetic, TAG_FIXNUM, TAG_MASK, WORD_T};
use zkcl_opt::native::NativeOp;
use zkcl_opt::{CmpOp, RootSlot, SpecializationInputs, SsaFunction, SsaInst, SsaOp, ValueId};

use super::{BlockType, Emitter, Ins, RuntimeAbiFunction, ValType, WORD_NIL};

#[derive(Clone, Copy)]
pub(super) struct Binding {
    primitive: FixnumPrimitive,
    mask: i64,
}

pub(super) fn bindings(
    function: &SsaFunction,
    specialization: &dyn SpecializationInputs,
) -> Vec<Option<Binding>> {
    let mut bindings = vec![None; function.constant_count];
    let mut count = 0;
    for inst in function.blocks.iter().flat_map(|block| &block.insts) {
        let SsaOp::CallNamed {
            symbol,
            args,
            associate: false,
            notinline: false,
            ..
        } = &inst.op
        else {
            continue;
        };
        if bindings[*symbol].is_some() || count == 64 {
            continue;
        }
        let Some(("COMMON-LISP", name)) = specialization.named_symbol(*symbol) else {
            continue;
        };
        if let Some(primitive) = FixnumPrimitive::named(name)
            && primitive.arity() == args.len()
        {
            bindings[*symbol] = Some(Binding {
                primitive,
                mask: 1i64 << count,
            });
            count += 1;
        }
    }
    bindings
}

impl<F: Fn(RuntimeAbiFunction) -> Option<u32>> Emitter<'_, F> {
    pub(super) fn numeric_call(
        &mut self,
        operation: NativeOp<'_>,
        inst: &SsaInst,
        live: &[(ValueId, RootSlot)],
    ) -> bool {
        let SsaOp::CallNamed {
            symbol,
            args,
            has_result,
            notinline: false,
            associate: false,
            ..
        } = &inst.op
        else {
            return false;
        };
        let Some(binding) =
            self.primitives[*symbol].filter(|binding| binding.primitive.arity() == args.len())
        else {
            return false;
        };
        let lhs = args[0];
        let rhs = args.get(1).copied();
        self.fixnum_operands(lhs, rhs);
        self.scope(Ins::If(BlockType::Result(ValType::I32)));
        if let FixnumPrimitive::Arithmetic(op) = binding.primitive {
            self.arithmetic_fits(op, lhs, rhs);
            self.scope(Ins::If(BlockType::Result(ValType::I32)));
        }
        self.cached_binding(*symbol, binding);
        if matches!(binding.primitive, FixnumPrimitive::Arithmetic(_)) {
            self.ins(Ins::Else);
            self.word(0);
            self.end();
        }
        self.ins(Ins::Else);
        self.word(0);
        self.end();

        let block = if *has_result {
            BlockType::Result(ValType::I32)
        } else {
            BlockType::Empty
        };
        self.scope(Ins::If(block));
        if *has_result {
            self.emit_numeric_primary(binding.primitive, lhs, rhs);
        } else {
            self.publish_computed_primary(|emitter| {
                emitter.emit_numeric_primary(binding.primitive, lhs, rhs);
            });
        }
        self.ins(Ins::Else);
        self.spill(live);
        self.invalidate_bindings();
        self.emit_numeric_fallback(operation, inst, live);
        self.reload(live);
        self.end();
        if let Some(id) = inst.id {
            self.set(id);
        }
        true
    }

    fn emit_numeric_primary(
        &mut self,
        primitive: FixnumPrimitive,
        lhs: ValueId,
        rhs: Option<ValueId>,
    ) {
        match primitive {
            FixnumPrimitive::Arithmetic(_) => {
                self.ins(Ins::LocalGet(self.wide.unwrap()));
                self.box_fixnum();
            }
            FixnumPrimitive::Compare(op) => {
                self.word(WORD_T as usize);
                self.word(WORD_NIL as usize);
                self.get(lhs);
                self.get(rhs.unwrap());
                self.ins(match op {
                    CmpOp::Eq => Ins::I32Eq,
                    CmpOp::Ne => Ins::I32Ne,
                    CmpOp::Lt => Ins::I32LtS,
                    CmpOp::Le => Ins::I32LeS,
                    CmpOp::Gt => Ins::I32GtS,
                    CmpOp::Ge => Ins::I32GeS,
                });
                self.ins(Ins::Select);
            }
        }
    }

    pub(super) fn invalidate_bindings(&mut self) {
        if let Some(cache) = self.bindings {
            self.ins(Ins::I64Const(0));
            self.ins(Ins::LocalSet(cache));
        }
    }

    fn cached_binding(&mut self, symbol: usize, binding: Binding) {
        let cache = self.bindings.unwrap();
        self.ins(Ins::LocalGet(cache));
        self.ins(Ins::I64Const(binding.mask));
        self.ins(Ins::I64And);
        self.ins(Ins::I64Eqz);
        self.scope(Ins::If(BlockType::Result(ValType::I32)));
        self.check_binding(symbol, binding.primitive);
        self.scope(Ins::If(BlockType::Result(ValType::I32)));
        self.ins(Ins::LocalGet(cache));
        self.ins(Ins::I64Const(binding.mask));
        self.ins(Ins::I64Or);
        self.ins(Ins::LocalSet(cache));
        self.word(1);
        self.ins(Ins::Else);
        self.word(0);
        self.end();
        self.ins(Ins::Else);
        self.word(1);
        self.end();
    }

    fn check_binding(&mut self, symbol: usize, primitive: FixnumPrimitive) {
        self.check_binding_index(symbol, primitive.binding_index());
    }

    pub(super) fn check_binding_index(&mut self, symbol: usize, index: u8) {
        self.ctx();
        self.target_word(self.frame.roots().code_slot.0);
        self.word(symbol);
        self.word(usize::from(index));
        self.call(RuntimeAbiFunction::IsPrimitiveBinding);
    }

    pub(super) fn try_arithmetic(
        &mut self,
        op: FixnumArithmetic,
        symbol: usize,
        lhs: ValueId,
        rhs: ValueId,
    ) {
        self.fixnum_operands(lhs, Some(rhs));
        self.scope(Ins::If(BlockType::Result(ValType::I32)));
        self.arithmetic_fits(op, lhs, Some(rhs));
        self.check_binding(symbol, FixnumPrimitive::Arithmetic(op));
        self.ins(Ins::I32And);
        self.scope(Ins::If(BlockType::Result(ValType::I32)));
        self.ins(Ins::LocalGet(self.wide.unwrap()));
        self.box_fixnum();
        self.ins(Ins::Else);
        self.word(WORD_NIL as usize);
        self.end();
        self.ins(Ins::Else);
        self.word(WORD_NIL as usize);
        self.end();
    }

    fn arithmetic_fits(&mut self, op: FixnumArithmetic, lhs: ValueId, rhs: Option<ValueId>) {
        self.get(lhs);
        self.unbox_fixnum();
        if let Some(rhs) = rhs {
            self.get(rhs);
            self.unbox_fixnum();
        } else {
            self.ins(Ins::I64Const(1));
        }
        self.ins(match op {
            FixnumArithmetic::Add | FixnumArithmetic::Inc => Ins::I64Add,
            FixnumArithmetic::Sub | FixnumArithmetic::Dec => Ins::I64Sub,
            FixnumArithmetic::Mul => Ins::I64Mul,
        });
        self.ins(Ins::LocalTee(self.wide.unwrap()));
        let (min, max) = self.ssa.fixnum_bounds;
        self.ins(Ins::I64Const(min));
        self.ins(Ins::I64Sub);
        self.ins(Ins::I64Const(max - min));
        self.ins(Ins::I64LeU);
    }

    fn fixnum_operands(&mut self, lhs: ValueId, rhs: Option<ValueId>) {
        self.is_fixnum(lhs);
        if let Some(rhs) = rhs {
            self.is_fixnum(rhs);
            self.ins(Ins::I32And);
        }
    }

    pub(super) fn is_fixnum(&mut self, value: ValueId) {
        self.get(value);
        self.word(TAG_MASK as usize);
        self.ins(Ins::I32And);
        self.word(TAG_FIXNUM as usize);
        self.ins(Ins::I32Eq);
    }

    pub(super) fn unbox_fixnum(&mut self) {
        self.word(FIXNUM_SHIFT as usize);
        self.ins(Ins::I32ShrS);
        self.ins(Ins::I64ExtendI32S);
    }

    pub(super) fn box_fixnum(&mut self) {
        self.ins(Ins::I32WrapI64);
        self.word(FIXNUM_SHIFT as usize);
        self.ins(Ins::I32Shl);
        self.word(TAG_FIXNUM as usize);
        self.ins(Ins::I32Or);
    }
}
