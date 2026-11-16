//! Little-endian A32 leaves using the ARM procedure call standard.

use arm32_assembly::{
    Arm32Condition::{
        self as Condition, AlwaysUnconditional as Al, CarryClear as Lo, Equal as Eq,
        NotEqual as Ne, Overflow as Vs, SignedGreaterThan as Gt, SignedGreaterThanOrEqual as Ge,
        SignedLessThan as Lt, SignedLessThanOrEqual as Le,
    },
    Arm32GeneralPurposeRegister::{
        self as Register, R0, R1, R2, R3, R12 as Ip, R13 as Sp, R14 as Lr,
    },
    Arm32IndexMode::Offset,
    Arm32MemoryOffset,
    Arm32RegisterShift::{Asr, Lsl},
    ArmA32Instruction::{
        self as Instruction, Add_Immediate_A1 as AddImm, Add_Register_A1 as Add,
        And_Immediate_A1 as And, And_Register_A1 as AndReg, B_A1 as B, Bx_A1 as Bx,
        Cmp_Immediate_A1 as CmpImm, Cmp_Register_A1 as Cmp, Eor_Immediate_A1 as EorImm,
        Eor_Register_A1 as Eor, Ldr_A1 as Ldr, Mov_Immediate_A1 as MovImm, Mov_Register_A1 as Mov,
        Movt_A1 as Movt, Movw_A2 as Movw, Mul_A1 as Mul, Mvn_Register_A1 as Mvn,
        Orr_Immediate_A1 as OrrImm, Orr_Register_A1 as Orr, Smull_A1 as Smull, Str_A1 as Str,
        Sub_Immediate_A1 as SubImm, Sub_Register_A1 as Sub,
    },
};
use zkcl_bytecode::{
    FIXNUM_SHIFT, FixnumArithmetic, Immediate, TAG_FIXNUM, TAG_MASK, WORD_NIL, WORD_T,
    native_abi::{
        FixnumComparison, JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_ONE, JitReturnLayout,
        jit_return_layout,
    },
};

use super::{Assembler, BitwiseOp, LeafFunction, Operand, Operation, Test, emit_body};

const WORD_BYTES: i32 = 4;
// In A32 a read of PC observes the instruction address plus eight.
const PC_BIAS: i32 = 2 * WORD_BYTES;
const _: () = assert!(JIT_RETURN_TAG_ONE == 0);
const ARGUMENTS: [Register; 4] = [R0, R1, R2, R3];

fn offset(bytes: i32) -> Arm32MemoryOffset {
    Arm32MemoryOffset::Immediate {
        add: true,
        imm12: bytes as u16,
    }
}

struct LeafAssembler {
    instructions: Vec<Instruction>,
    // The instruction library encodes B's displacement. Our block layout
    // supplies the byte offsets once all variable-length blocks are emitted.
    branches: Vec<(usize, Condition, usize)>,
    labels: Vec<usize>,
    frame_bytes: i32,
    local_base: i32,
    runtime_entry: bool,
}

impl LeafAssembler {
    fn store_local(&mut self, destination: usize, register: Register) {
        let location = self.local_base + WORD_BYTES * destination as i32;
        self.instructions
            .push(Str(Al, register, Sp, offset(location), Offset));
    }

    fn branch(&mut self, condition: Condition, target: usize) {
        self.branches
            .push((self.instructions.len(), condition, target));
        self.instructions.push(B(condition, 0));
    }

    fn finish(mut self) -> Option<Vec<u8>> {
        for (site, condition, target) in self.branches {
            let distance = (self.labels[target] as i32 - site as i32) * WORD_BYTES - PC_BIAS;
            self.instructions[site] = B(condition, distance);
        }
        let mut bytes = Vec::with_capacity(self.instructions.len() * WORD_BYTES as usize);
        for instruction in self.instructions {
            bytes.extend(instruction.encode().ok()?);
        }
        Some(bytes)
    }

    fn load(&mut self, reg: Register, word: u32) {
        self.instructions.push(Movw(Al, reg, word as u16));
        let high = (word >> 16) as u16;
        if high != 0 {
            self.instructions.push(Movt(Al, reg, high));
        }
    }

    fn operand(&mut self, value: Operand, scratch: Register) -> Register {
        let location = match value {
            Operand::Arg(index) if self.frame_bytes == 0 => return ARGUMENTS[usize::from(index)],
            Operand::Arg(index) => WORD_BYTES * i32::from(index),
            Operand::Local(index) => self.local_base + WORD_BYTES * index as i32,
            Operand::Immediate(raw) => {
                self.load(scratch, raw as u32);
                return scratch;
            }
        };
        self.instructions
            .push(Ldr(Al, scratch, Sp, offset(location), Offset));
        scratch
    }

    fn compare(&mut self, reg: Register, word: u32) {
        if word <= u32::from(u8::MAX) {
            self.instructions.push(CmpImm(Al, reg, word));
        } else {
            self.load(Ip, word);
            self.instructions.push(Cmp(Al, reg, Ip, Lsl(0)));
        }
    }

    fn compare_tag(&mut self, reg: Register) {
        self.instructions.extend([
            And(Al, false, Ip, reg, TAG_MASK as u32),
            CmpImm(Al, Ip, TAG_FIXNUM as u32),
        ]);
    }

    fn branch_test(
        &mut self,
        reg: Register,
        test: Test,
        yes: usize,
        no: usize,
        next: Option<usize>,
    ) {
        let (success, failure) = match test {
            Test::Nil => {
                self.compare(reg, WORD_NIL as u32);
                (Eq, Ne)
            }
            Test::Fixnum => {
                self.compare_tag(reg);
                (Eq, Ne)
            }
            Test::FixnumInRange(low, high) => {
                let tagged = |n| {
                    Immediate::Fixnum(i64::from(n))
                        .encode_word(32)
                        .expect("selector proved target bounds") as u32
                };
                if low == high {
                    self.compare(reg, tagged(low));
                    (Eq, Ne)
                } else {
                    self.compare_tag(reg);
                    self.branch(Ne, no);
                    // Signed ordering is preserved by the tagged representation.
                    self.compare(reg, tagged(low));
                    self.branch(Lt, no);
                    self.compare(reg, tagged(high));
                    (Le, Gt)
                }
            }
        };
        if Some(yes) == next {
            self.branch(failure, no);
        } else {
            self.branch(success, yes);
            if Some(no) != next {
                self.branch(Al, no);
            }
        }
    }

    fn arithmetic(
        &mut self,
        op: FixnumArithmetic,
        lhs: Operand,
        rhs: Option<Operand>,
        miss: Option<usize>,
    ) {
        use FixnumArithmetic::{Add as Plus, Dec, Inc, Mul as Times, Sub as Minus};
        let lhs = self.operand(lhs, R0);
        let tag = TAG_FIXNUM as u32;
        let unit = 1 << FIXNUM_SHIFT;
        let checked = miss.is_some();
        match op {
            Inc => self.instructions.push(AddImm(Al, checked, R0, lhs, unit)),
            Dec => self.instructions.push(SubImm(Al, checked, R0, lhs, unit)),
            Plus | Minus => {
                let rhs = self.operand(rhs.expect("binary operation"), Ip);
                self.instructions.push(SubImm(Al, false, Ip, rhs, tag));
                self.instructions.push(if op == Plus {
                    Add(Al, checked, R0, lhs, Ip, Lsl(0))
                } else {
                    Sub(Al, checked, R0, lhs, Ip, Lsl(0))
                });
            }
            Times => {
                let rhs = self.operand(rhs.expect("binary operation"), Ip);
                self.instructions.extend([
                    Mov(Al, false, R0, lhs, Asr(FIXNUM_SHIFT as u8)),
                    SubImm(Al, false, Ip, rhs, tag),
                ]);
                if let Some(miss) = miss {
                    self.instructions
                        .extend([Smull(Al, false, R0, Ip, R0, Ip), Cmp(Al, Ip, R0, Asr(31))]);
                    self.branch(Ne, miss);
                } else {
                    self.instructions.push(Mul(Al, false, R0, R0, Ip));
                }
                self.instructions.push(AddImm(Al, false, R0, R0, tag));
                return;
            }
        }
        if let Some(miss) = miss {
            self.branch(Vs, miss);
        }
    }

    fn bitwise(&mut self, op: BitwiseOp, lhs: Operand, rhs: Operand) {
        let lhs = self.operand(lhs, R0);
        let rhs = self.operand(rhs, Ip);
        match op {
            BitwiseOp::And => self
                .instructions
                .push(AndReg(Al, false, R0, lhs, rhs, Lsl(0))),
            BitwiseOp::Or => self.instructions.push(Orr(Al, false, R0, lhs, rhs, Lsl(0))),
            BitwiseOp::Xor => self.instructions.extend([
                Eor(Al, false, R0, lhs, rhs, Lsl(0)),
                OrrImm(Al, false, R0, R0, TAG_FIXNUM as u32),
            ]),
        }
    }

    fn compare_fixnums(&mut self, relation: FixnumComparison, lhs: Operand, rhs: Operand) {
        let lhs = self.operand(lhs, R0);
        let rhs = self.operand(rhs, Ip);
        let condition = match relation {
            FixnumComparison::Eq => Eq,
            FixnumComparison::Ne => Ne,
            FixnumComparison::Lt => Lt,
            FixnumComparison::Le => Le,
            FixnumComparison::Gt => Gt,
            FixnumComparison::Ge => Ge,
        };
        self.instructions.extend([
            Cmp(Al, lhs, rhs, Lsl(0)),
            MovImm(Al, false, R0, WORD_NIL as u32),
            MovImm(condition, false, R0, WORD_T as u32),
        ]);
    }
}

impl Assembler for LeafAssembler {
    type Label = usize;

    fn label(&mut self) -> Self::Label {
        let index = self.labels.len();
        self.labels.push(0);
        index
    }

    fn bind(&mut self, label: Self::Label) {
        self.labels[label] = self.instructions.len();
    }

    fn jump(&mut self, label: Self::Label) {
        self.branch(Al, label);
    }

    fn initialize_loop_budget(&mut self, budget: u32) {
        self.instructions.push(MovImm(Al, false, R3, budget));
    }

    fn take_backedge(&mut self, miss: Self::Label) {
        self.instructions.push(SubImm(Al, true, R3, R3, 1));
        self.branch(Lo, miss);
    }

    fn test(
        &mut self,
        value: Operand,
        test: Test,
        yes: Self::Label,
        no: Self::Label,
        next: Option<Self::Label>,
    ) {
        let register = self.operand(value, R0);
        self.branch_test(register, test, yes, no, next);
    }

    fn copy(&mut self, destination: usize, value: Operand) {
        let register = self.operand(value, R0);
        self.store_local(destination, register);
    }

    fn compute(&mut self, destination: usize, operation: Operation, miss: Option<Self::Label>) {
        match operation {
            Operation::Arithmetic {
                op,
                lhs,
                rhs,
                checked,
            } => self.arithmetic(op, lhs, rhs, miss.filter(|_| checked)),
            Operation::Compare { relation, lhs, rhs } => self.compare_fixnums(relation, lhs, rhs),
            Operation::Bitwise { op, lhs, rhs } => self.bitwise(op, lhs, rhs),
            Operation::Complement(value) => {
                let src = self.operand(value, R0);
                self.instructions.extend([
                    Mvn(Al, false, R0, src, Lsl(0)),
                    EorImm(Al, false, R0, R0, TAG_MASK as u32),
                ]);
            }
        }
        self.store_local(destination, R0);
    }

    fn return_value(&mut self, value: Operand) {
        let reg = self.operand(value, R0);
        if reg != R0 {
            self.instructions.push(Mov(Al, false, R0, reg, Lsl(0)));
        }
        if self.frame_bytes != 0 {
            self.instructions
                .push(AddImm(Al, false, Sp, Sp, self.frame_bytes as u32));
        }
        if self.runtime_entry {
            let payload = jit_return_layout(WORD_BYTES).single_value;
            self.instructions.extend([
                Ldr(Al, Ip, Sp, offset(0), Offset),
                Str(Al, R0, Ip, offset(payload), Offset),
            ]);
        }
        self.instructions.push(Bx(Al, Lr));
    }
}

pub(super) fn emit(function: &LeafFunction, runtime_entry: bool) -> Option<Vec<u8>> {
    let arity = function.arity();
    let local_base = WORD_BYTES * arity as i32;
    let mut asm = LeafAssembler {
        instructions: Vec::new(),
        branches: Vec::new(),
        labels: Vec::new(),
        frame_bytes: 0,
        local_base,
        runtime_entry,
    };
    if runtime_entry {
        emit_entry_prologue(&mut asm, arity);
    }
    if function.locals() != 0 || (arity != 0 && function.loop_budget().is_some()) {
        // There are only five volatile integer registers on AAPCS32. Keep
        // the original argument registers in an aligned leaf frame when
        // the body needs them in addition to locals and arithmetic scratch.
        let bytes = WORD_BYTES as usize * (arity + function.locals());
        asm.frame_bytes = bytes.next_multiple_of(8) as i32;
        asm.instructions
            .push(SubImm(Al, false, Sp, Sp, asm.frame_bytes as u32));
        for (index, &reg) in ARGUMENTS[..arity].iter().enumerate() {
            asm.instructions
                .push(Str(Al, reg, Sp, offset(WORD_BYTES * index as i32), Offset));
        }
    }
    emit_body(&mut asm, function);
    asm.finish()
}

fn emit_entry_prologue(asm: &mut LeafAssembler, arity: usize) {
    let JitReturnLayout {
        tag,
        single_value: payload,
        target_frame: frame,
        state,
        ..
    } = jit_return_layout(WORD_BYTES);
    // R2 is the argument array, R3 the supplied arity. Validate before any load.
    asm.instructions.extend([
        Ldr(Al, Ip, Sp, offset(0), Offset),
        MovImm(Al, false, R0, 0),
        Str(Al, R0, Ip, offset(frame), Offset),
        Str(Al, R0, Ip, offset(state), Offset),
        CmpImm(Al, R3, arity as u32),
        Str(Ne, R0, Ip, offset(payload), Offset),
        MovImm(Ne, false, R0, u32::from(JIT_RETURN_TAG_FAULT)),
        Str(Al, R0, Ip, offset(tag), Offset),
        Bx(Ne, Lr),
    ]);
    if arity != 0 {
        asm.instructions.push(Mov(Al, false, Ip, R2, Lsl(0)));
        for (i, &reg) in ARGUMENTS[..arity].iter().enumerate() {
            asm.instructions
                .push(Ldr(Al, reg, Ip, offset(i as i32 * WORD_BYTES), Offset));
        }
    }
}

pub(super) fn dispatch_jump() -> Vec<u8> {
    // R_ARM_JUMP24 uses a REL relocation: B to itself embeds the -8 addend.
    B(Al, -PC_BIAS).encode().expect("A32 self branch").to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leaf::{Block, Exit, Step};

    #[test]
    fn conditional_branch_targets_the_correct_return_in_both_entry_abis() {
        let leaf = LeafFunction {
            loop_budget: None,
            arity: 4,
            guards: Vec::new(),
            word_bits: 32,
            blocks: vec![
                Block {
                    steps: vec![],
                    exit: Exit::Branch {
                        value: Operand::Arg(3),
                        test: Test::Nil,
                        yes: 1,
                        no: 2,
                    },
                },
                Block {
                    steps: vec![],
                    exit: Exit::Return(Operand::Arg(2)),
                },
                Block {
                    steps: vec![],
                    exit: Exit::Return(Operand::Arg(1)),
                },
            ],
            locals: 0,
        };
        for bytes in [emit(&leaf, false).unwrap(), emit(&leaf, true).unwrap()] {
            let mut instructions = bytes.iter();
            let mut pc = 0;
            let mut destination = None;
            let mut return_address = None;
            while let Some(instruction) = Instruction::decode(&mut instructions, &mut pc).unwrap() {
                let address = pc as i32 - WORD_BYTES;
                match instruction {
                    B(Ne, displacement) => destination = Some(address + PC_BIAS + displacement),
                    Mov(Al, false, R0, R1, Lsl(0)) => return_address = Some(address),
                    _ => {}
                }
            }
            assert_eq!(
                destination.expect("conditional branch"),
                return_address.expect("return r1")
            );
        }
    }

    #[test]
    fn multiply_preserves_a_right_operand_in_the_result_register() {
        let leaf = LeafFunction {
            loop_budget: None,
            arity: 4,
            guards: Vec::new(),
            word_bits: 32,
            blocks: vec![Block {
                steps: vec![Step::Compute {
                    destination: 0,
                    op: Operation::Arithmetic {
                        op: FixnumArithmetic::Mul,
                        lhs: Operand::Arg(3),
                        rhs: Some(Operand::Arg(0)),
                        checked: true,
                    },
                }],
                exit: Exit::Return(Operand::Local(0)),
            }],
            locals: 1,
        };
        let expected = [
            Ldr(Al, R0, Sp, offset(12), Offset),
            Ldr(Al, Ip, Sp, offset(0), Offset),
            Mov(Al, false, R0, R0, Asr(FIXNUM_SHIFT as u8)),
            SubImm(Al, false, Ip, Ip, TAG_FIXNUM as u32),
            Smull(Al, false, R0, Ip, R0, Ip),
            Cmp(Al, Ip, R0, Asr(31)),
        ];
        let expected: Vec<_> = expected
            .iter()
            .flat_map(|op| op.encode().unwrap())
            .collect();
        assert!(
            emit(&leaf, false)
                .unwrap()
                .windows(expected.len())
                .any(|window| window == expected)
        );
    }

    #[test]
    fn dispatcher_branch_to_self_carries_the_elf_rel_addend() {
        let bytes = dispatch_jump();
        assert!(matches!(
            Instruction::decode(&mut bytes.iter(), &mut 0).unwrap(),
            Some(B(Al, displacement)) if displacement == -PC_BIAS
        ));
    }
}
