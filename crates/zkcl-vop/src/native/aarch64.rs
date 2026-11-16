use dynasmrt::{
    DynamicLabel, DynasmApi, DynasmLabelApi, VecAssembler, aarch64::Aarch64Relocation, dynasm,
};
use zkcl_bytecode::native_abi::{
    FixnumComparison, JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_ONE, JitReturnLayout, jit_return_layout,
};
use zkcl_bytecode::{FIXNUM_SHIFT, FixnumArithmetic, TAG_FIXNUM, TAG_MASK, WORD_NIL, WORD_T};

use super::{Assembler, BitwiseOp, LeafFunction, Operand, Operation, Test, emit_body};

// The 64-bit C arguments and return value use x0–x3 and x0, respectively.
// x5–x8 store leaf locals, x9–x11 are scratch. All are caller-saved on
// AAPCS64, Apple, and Windows.
// Preserve x15 for the generic entry adapter's result pointer.
const RESULT: u8 = 0;
const FIRST_LOCAL: u8 = 5;
const SCRATCH: u8 = 9;
const IMMEDIATE: u8 = 10;

// The generic entry writes the repr(C) JitReturn with a whole word for the tag.
const _: () = {
    let JitReturnLayout {
        tag,
        single_value: payload,
        target_frame: frame,
        state,
        ..
    } = jit_return_layout(8);
    assert!(tag == 0 && payload == 8 && frame == 16 && state == 24);
    assert!(JIT_RETURN_TAG_ONE == 0);
    assert!(WORD_T == WORD_NIL + 1);
};

type MachineCode = VecAssembler<Aarch64Relocation>;

#[derive(Clone, Copy)]
enum Condition {
    Equal,
    NotEqual,
    UnsignedHigher,
    UnsignedLowerOrSame,
}

impl Condition {
    fn invert(self) -> Self {
        match self {
            Self::Equal => Self::NotEqual,
            Self::NotEqual => Self::Equal,
            Self::UnsignedHigher => Self::UnsignedLowerOrSame,
            Self::UnsignedLowerOrSame => Self::UnsignedHigher,
        }
    }
}

struct LeafAssembler {
    code: MachineCode,
    runtime_entry: bool,
}

pub(super) fn emit(leaf: &LeafFunction, runtime_entry: bool) -> Option<Vec<u8>> {
    let mut asm = LeafAssembler {
        code: MachineCode::new(0),
        runtime_entry,
    };
    if runtime_entry {
        emit_entry_prologue(&mut asm.code, leaf.arity());
    }
    emit_body(&mut asm, leaf);
    asm.code.finalize().ok()
}

fn emit_entry_prologue(assembler: &mut MachineCode, arity: usize) {
    let count = arity as u32;
    let fault = u32::from(JIT_RETURN_TAG_FAULT);
    // The incoming argument array and result pointer are x2 and x4.
    dynasm!(assembler
        ; .arch aarch64
        ; mov x15, x4
        ; mov x4, x2
        ; stp xzr, xzr, [x15, #16]
        ; str xzr, [x15]
        ; cmp x3, #count
        ; b.eq >load_args
        ; movz x9, #fault
        ; stp x9, xzr, [x15]
        ; ret
        ; load_args:
    );
    for arg in 0..arity as u8 {
        let offset = u32::from(arg) * 8;
        dynasm!(assembler ; .arch aarch64 ; ldr X(arg), [x4, #offset]);
    }
}

pub(super) fn dispatch_jump() -> Vec<u8> {
    let mut assembler = MachineCode::new(0);
    // The object writer relocates this branch to the external runtime dispatcher.
    dynasm!(assembler
        ; .arch aarch64
        ; dispatch:
        ; b <dispatch
    );
    assembler.finalize().expect("local dispatch placeholder")
}

impl Assembler for LeafAssembler {
    type Label = DynamicLabel;

    fn label(&mut self) -> Self::Label {
        self.code.new_dynamic_label()
    }

    fn bind(&mut self, label: Self::Label) {
        dynasm!(self.code ; .arch aarch64 ; =>label);
    }

    fn jump(&mut self, label: Self::Label) {
        dynasm!(self.code ; .arch aarch64 ; b =>label);
    }

    fn initialize_loop_budget(&mut self, budget: u32) {
        dynasm!(self.code ; .arch aarch64 ; movz w12, #budget);
    }

    fn take_backedge(&mut self, miss: Self::Label) {
        dynasm!(self.code ; .arch aarch64 ; subs w12, w12, #1 ; b.lo =>miss);
    }

    fn test(
        &mut self,
        value: Operand,
        test: Test,
        yes: Self::Label,
        no: Self::Label,
        next: Option<Self::Label>,
    ) {
        let register = operand_register(&mut self.code, value, IMMEDIATE);
        branch_test(&mut self.code, register, test, yes, no, next);
    }

    fn copy(&mut self, destination: usize, value: Operand) {
        let destination = local(destination);
        let source = operand_register(&mut self.code, value, destination);
        move_register(&mut self.code, destination, source);
    }

    fn compute(&mut self, destination: usize, operation: Operation, miss: Option<Self::Label>) {
        match operation {
            Operation::Arithmetic {
                op,
                lhs,
                rhs,
                checked,
            } => {
                arithmetic(&mut self.code, op, lhs, rhs, miss.filter(|_| checked));
            }
            Operation::Compare { relation, lhs, rhs } => {
                compare(&mut self.code, relation, lhs, rhs)
            }
            Operation::Bitwise { op, lhs, rhs } => bitwise(&mut self.code, op, lhs, rhs),
            Operation::Complement(value) => {
                let src = operand_register(&mut self.code, value, SCRATCH);
                const PAYLOAD: u64 = !TAG_MASK;
                dynasm!(self.code ; .arch aarch64 ; eor x9, X(src), #PAYLOAD);
            }
        }
        move_register(&mut self.code, local(destination), SCRATCH);
    }

    fn return_value(&mut self, value: Operand) {
        let register = operand_register(&mut self.code, value, RESULT);
        move_register(&mut self.code, RESULT, register);
        if self.runtime_entry {
            dynasm!(self.code ; .arch aarch64 ; str x0, [x15, #8]);
        }
        dynasm!(self.code ; .arch aarch64 ; ret);
    }
}

fn local(index: usize) -> u8 {
    FIRST_LOCAL + index as u8
}

fn operand_register(assembler: &mut MachineCode, operand: Operand, register: u8) -> u8 {
    match operand {
        Operand::Arg(arg) => arg,
        Operand::Local(index) => local(index),
        Operand::Immediate(value) => {
            load_immediate(assembler, register, value);
            register
        }
    }
}

fn move_register(assembler: &mut MachineCode, result: u8, source: u8) {
    if result != source {
        dynasm!(assembler ; .arch aarch64 ; mov X(result), X(source));
    }
}

fn arithmetic(
    assembler: &mut MachineCode,
    op: FixnumArithmetic,
    lhs: Operand,
    rhs: Option<Operand>,
    miss: Option<DynamicLabel>,
) {
    use FixnumArithmetic::{Add, Dec, Inc, Mul, Sub};

    let lhs = operand_register(assembler, lhs, SCRATCH);
    let tag = TAG_FIXNUM as u32;
    let unit = 1u32 << FIXNUM_SHIFT;
    match op {
        Inc | Dec => match (op, miss) {
            (Inc, Some(miss)) => {
                dynasm!(assembler ; .arch aarch64 ; adds x9, XSP(lhs), #unit ; b.vs =>miss)
            }
            (Dec, Some(miss)) => {
                dynasm!(assembler ; .arch aarch64 ; subs x9, XSP(lhs), #unit ; b.vs =>miss)
            }
            (Inc, None) => dynasm!(assembler ; .arch aarch64 ; add x9, XSP(lhs), #unit),
            _ => dynasm!(assembler ; .arch aarch64 ; sub x9, XSP(lhs), #unit),
        },
        Add | Sub => {
            let rhs = rhs.expect("binary arithmetic");
            if miss.is_none()
                && let Operand::Immediate(raw) = rhs
            {
                let delta = raw.wrapping_sub(TAG_FIXNUM);
                add_immediate(
                    assembler,
                    SCRATCH,
                    lhs,
                    if op == Add {
                        delta
                    } else {
                        delta.wrapping_neg()
                    },
                );
                return;
            }
            let rhs = operand_register(assembler, rhs, IMMEDIATE);
            // Remove the right tag before testing signed overflow on the final word.
            dynasm!(assembler ; .arch aarch64 ; sub x10, XSP(rhs), #tag);
            match (op, miss) {
                (Add, Some(miss)) => {
                    dynasm!(assembler ; .arch aarch64 ; adds x9, X(lhs), x10 ; b.vs =>miss)
                }
                (Sub, Some(miss)) => {
                    dynasm!(assembler ; .arch aarch64 ; subs x9, X(lhs), x10 ; b.vs =>miss)
                }
                (Add, None) => dynasm!(assembler ; .arch aarch64 ; add x9, X(lhs), x10),
                _ => dynasm!(assembler ; .arch aarch64 ; sub x9, X(lhs), x10),
            }
        }
        Mul => {
            let rhs = operand_register(assembler, rhs.expect("binary arithmetic"), IMMEDIATE);
            dynasm!(assembler
                ; .arch aarch64
                ; asr x9, X(lhs), #FIXNUM_SHIFT
                ; sub x10, XSP(rhs), #tag
            );
            if let Some(miss) = miss {
                dynasm!(assembler
                    ; .arch aarch64
                    ; smulh x11, x9, x10
                    ; mul x9, x9, x10
                    ; cmp x11, x9, ASR #63
                    ; b.ne =>miss
                );
            } else {
                dynasm!(assembler ; .arch aarch64 ; mul x9, x9, x10);
            }
            dynasm!(assembler ; .arch aarch64 ; add x9, x9, #tag);
        }
    }
}

fn bitwise(assembler: &mut MachineCode, op: BitwiseOp, lhs: Operand, rhs: Operand) {
    let lhs = operand_register(assembler, lhs, SCRATCH);
    let rhs = operand_register(assembler, rhs, IMMEDIATE);
    match op {
        BitwiseOp::And => dynasm!(assembler ; .arch aarch64 ; and x9, X(lhs), X(rhs)),
        BitwiseOp::Or => dynasm!(assembler ; .arch aarch64 ; orr x9, X(lhs), X(rhs)),
        BitwiseOp::Xor => dynasm!(assembler
            ; .arch aarch64
            ; eor x9, X(lhs), X(rhs)
            ; orr x9, x9, #TAG_FIXNUM
        ),
    }
}

fn compare(assembler: &mut MachineCode, relation: FixnumComparison, lhs: Operand, rhs: Operand) {
    let lhs = operand_register(assembler, lhs, SCRATCH);
    let rhs = operand_register(assembler, rhs, IMMEDIATE);
    dynasm!(assembler ; .arch aarch64 ; cmp X(lhs), X(rhs));
    match relation {
        FixnumComparison::Eq => dynasm!(assembler ; .arch aarch64 ; cset w9, eq),
        FixnumComparison::Ne => dynasm!(assembler ; .arch aarch64 ; cset w9, ne),
        FixnumComparison::Lt => dynasm!(assembler ; .arch aarch64 ; cset w9, lt),
        FixnumComparison::Le => dynasm!(assembler ; .arch aarch64 ; cset w9, le),
        FixnumComparison::Gt => dynasm!(assembler ; .arch aarch64 ; cset w9, gt),
        FixnumComparison::Ge => dynasm!(assembler ; .arch aarch64 ; cset w9, ge),
    }
    dynasm!(assembler ; .arch aarch64 ; add x9, x9, #WORD_NIL as u32);
}

fn branch_test(
    assembler: &mut MachineCode,
    arg: u8,
    test: Test,
    yes: DynamicLabel,
    no: DynamicLabel,
    next: Option<DynamicLabel>,
) {
    if yes == no {
        if Some(yes) != next {
            dynasm!(assembler ; .arch aarch64 ; b =>yes);
        }
        return;
    }
    let success = match test {
        Test::Nil => {
            compare_immediate(assembler, arg, WORD_NIL);
            Condition::Equal
        }
        Test::Fixnum => {
            compare_fixnum_tag(assembler, arg);
            Condition::Equal
        }
        Test::FixnumInRange(min, max) => {
            compare_fixnum_tag(assembler, arg);
            branch_if(assembler, no, Condition::NotEqual);

            let lower = tagged_fixnum(min);
            let upper = tagged_fixnum(max);
            // Subtract the lower bound modulo 2^64. An unsigned comparison
            // with the interval width rejects values below and above it.
            add_immediate(assembler, SCRATCH, arg, lower.wrapping_neg());
            compare_immediate(assembler, SCRATCH, upper.wrapping_sub(lower));
            Condition::UnsignedLowerOrSame
        }
    };
    if Some(yes) == next {
        branch_if(assembler, no, success.invert());
    } else {
        branch_if(assembler, yes, success);
        if Some(no) != next {
            dynasm!(assembler ; .arch aarch64 ; b =>no);
        }
    }
}

fn branch_if(assembler: &mut MachineCode, target: DynamicLabel, condition: Condition) {
    match condition {
        Condition::Equal => dynasm!(assembler ; .arch aarch64 ; b.eq =>target),
        Condition::NotEqual => dynasm!(assembler ; .arch aarch64 ; b.ne =>target),
        Condition::UnsignedHigher => dynasm!(assembler ; .arch aarch64 ; b.hi =>target),
        Condition::UnsignedLowerOrSame => dynasm!(assembler ; .arch aarch64 ; b.ls =>target),
    }
}

fn compare_fixnum_tag(assembler: &mut MachineCode, arg: u8) {
    let tag = TAG_FIXNUM as u32;
    dynasm!(assembler
        ; .arch aarch64
        ; and x9, X(arg), #TAG_MASK
        ; cmp x9, #tag
    );
}

fn compare_immediate(assembler: &mut MachineCode, register: u8, value: u64) {
    if let Some((immediate, shift)) = add_sub_immediate(value) {
        dynasm!(assembler ; .arch aarch64 ; cmp XSP(register), #immediate, LSL #shift);
    } else {
        load_immediate(assembler, IMMEDIATE, value);
        dynasm!(assembler ; .arch aarch64 ; cmp X(register), x10);
    }
}

fn add_immediate(assembler: &mut MachineCode, result: u8, register: u8, value: u64) {
    if value == 0 {
        move_register(assembler, result, register);
    } else if let Some((immediate, shift)) = add_sub_immediate(value) {
        dynasm!(assembler ; .arch aarch64 ; add XSP(result), XSP(register), #immediate, LSL #shift);
    } else if let Some((immediate, shift)) = add_sub_immediate(value.wrapping_neg()) {
        dynasm!(assembler ; .arch aarch64 ; sub XSP(result), XSP(register), #immediate, LSL #shift);
    } else {
        load_immediate(assembler, IMMEDIATE, value);
        dynasm!(assembler ; .arch aarch64 ; add X(result), X(register), x10);
    }
}

fn load_immediate(assembler: &mut MachineCode, register: u8, value: u64) {
    let chunks = [
        value as u16,
        (value >> 16) as u16,
        (value >> 32) as u16,
        (value >> 48) as u16,
    ];
    let zeros = chunks.iter().filter(|&&chunk| chunk == 0).count();
    let ones = chunks.iter().filter(|&&chunk| chunk == u16::MAX).count();
    let inverted = ones > zeros;
    let fill = if inverted { u16::MAX } else { 0 };
    let first = chunks.iter().position(|&chunk| chunk != fill).unwrap_or(0);
    let shift = first as u32 * 16;
    if inverted {
        let immediate = u32::from(!chunks[first]);
        dynasm!(assembler ; .arch aarch64 ; movn X(register), #immediate, LSL #shift);
    } else {
        let immediate = u32::from(chunks[first]);
        dynasm!(assembler ; .arch aarch64 ; movz X(register), #immediate, LSL #shift);
    }
    for (index, chunk) in chunks.into_iter().enumerate() {
        if index != first && chunk != fill {
            let immediate = u32::from(chunk);
            let shift = index as u32 * 16;
            dynasm!(assembler ; .arch aarch64 ; movk X(register), #immediate, LSL #shift);
        }
    }
}

fn tagged_fixnum(value: i32) -> u64 {
    ((i64::from(value) as u64) << FIXNUM_SHIFT) | TAG_FIXNUM
}

fn add_sub_immediate(value: u64) -> Option<(u32, u32)> {
    if value <= 0xfff {
        Some((value as u32, 0))
    } else if value & 0xfff == 0 && value >> 12 <= 0xfff {
        Some(((value >> 12) as u32, 12))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leaf::{Block, Exit};

    #[test]
    fn nil_branch_uses_fallthrough_and_resolves_the_other_label() {
        let leaf = LeafFunction {
            loop_budget: None,
            arity: 2,
            word_bits: 64,
            guards: vec![],
            blocks: vec![
                Block {
                    steps: vec![],
                    exit: Exit::Branch {
                        value: Operand::Arg(1),
                        test: Test::Nil,
                        yes: 1,
                        no: 2,
                    },
                },
                Block {
                    steps: vec![],
                    exit: Exit::Return(Operand::Arg(0)),
                },
                Block {
                    steps: vec![],
                    exit: Exit::Return(Operand::Arg(1)),
                },
            ],
            locals: 0,
        };
        let words: [u32; 5] = [
            0xf100_083f,
            0x5400_0041,
            0xd65f_03c0,
            0xaa01_03e0,
            0xd65f_03c0,
        ];
        assert_eq!(
            emit(&leaf, false).unwrap(),
            words.map(u32::to_le_bytes).concat()
        );
        assert_eq!(dispatch_jump(), 0x1400_0000u32.to_le_bytes());
    }

    #[test]
    fn arithmetic_immediates_use_twelve_bits_and_an_optional_shift() {
        assert_eq!(add_sub_immediate(0), Some((0, 0)));
        assert_eq!(add_sub_immediate(0xfff), Some((0xfff, 0)));
        assert_eq!(add_sub_immediate(0x1000), Some((1, 12)));
        assert_eq!(add_sub_immediate(0x1001), None);
        assert_eq!(add_sub_immediate(0xfff000), Some((0xfff, 12)));
        assert_eq!(add_sub_immediate(0x1000000), None);
    }
}
