//! Integer leaf bodies for the RV64GC Linux LP64D ABI.

use dynasmrt::{
    DynamicLabel, DynasmApi, DynasmLabelApi, VecAssembler, dynasm, riscv::RiscvRelocation,
};
use zkcl_bytecode::native_abi::{
    FixnumComparison, JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_ONE, JitReturnLayout, jit_return_layout,
};
use zkcl_bytecode::{FIXNUM_SHIFT, FixnumArithmetic, TAG_FIXNUM, TAG_MASK, WORD_NIL, WORD_T};

use super::{Assembler, BitwiseOp, LeafFunction, Operand, Operation, Test, emit_body};

type MachineCode = VecAssembler<RiscvRelocation>;

const ZERO: u8 = 0;
const RESULT: u8 = 5; // t0
const RIGHT: u8 = 6; // t1
const LEFT: u8 = 7; // t2
const FIRST_ARG: u8 = 10; // a0
const FIRST_LOCAL: u8 = 14; // a4
const _: () = {
    let JitReturnLayout {
        tag,
        single_value: payload,
        target_frame: frame,
        state,
        ..
    } = jit_return_layout(8);
    assert!(tag == 0 && payload == 8 && frame == 16 && state == 24 && JIT_RETURN_TAG_ONE == 0);
    assert!(WORD_T == WORD_NIL + 1);
};

struct LeafAssembler {
    code: MachineCode,
    runtime_entry: bool,
}

pub(super) fn emit(function: &LeafFunction, runtime_entry: bool) -> Option<Vec<u8>> {
    let mut asm = LeafAssembler {
        code: MachineCode::new(0),
        runtime_entry,
    };
    if runtime_entry {
        let argc = function.arity() as i32;
        let fault = i32::from(JIT_RETURN_TAG_FAULT);
        // Keep the argument vector in t0 and the result record in t6.
        dynasm!(asm.code
            ; .arch riscv64
            ; mv t6, a4
            ; mv t0, a2
            ; sd zero, [t6, 16]
            ; sd zero, [t6, 24]
            ; addi t1, zero, argc
            ; beq a3, t1, >load_args
            ; addi t1, zero, fault
            ; sd t1, [t6]
            ; sd zero, [t6, 8]
            ; ret
            ; load_args:
            ; sd zero, [t6]
        );
        for index in 0..function.arity() {
            let reg = FIRST_ARG + index as u8;
            let offset = index as i32 * 8;
            dynasm!(asm.code ; .arch riscv64 ; ld X(reg), [t0, offset]);
        }
    }
    emit_body(&mut asm, function);
    asm.code.finalize().ok()
}

pub(super) fn dispatch_jump() -> Vec<u8> {
    let mut code = MachineCode::new(0);
    // R_RISCV_CALL_PLT relocates both instructions. Preserve ra and the arguments.
    dynasm!(code ; .arch riscv64 ; auipc t1, 0 ; jalr zero, t1, 0);
    code.finalize()
        .expect("dispatch placeholder has no internal relocations")
}

#[derive(Clone, Copy)]
enum Condition {
    Eq,
    Ne,
    Lt,
    Ge,
}

impl Condition {
    fn invert(self) -> Self {
        match self {
            Self::Eq => Self::Ne,
            Self::Ne => Self::Eq,
            Self::Lt => Self::Ge,
            Self::Ge => Self::Lt,
        }
    }
}

impl LeafAssembler {
    fn branch(&mut self, condition: Condition, lhs: u8, rhs: u8, target: DynamicLabel) {
        // The body can exceed the 12-bit conditional branch range. Branch
        // over a JAL to reach any block within the selection budget.
        let skip = self.label();
        match condition.invert() {
            Condition::Eq => dynasm!(self.code ; .arch riscv64 ; beq X(lhs), X(rhs), =>skip),
            Condition::Ne => dynasm!(self.code ; .arch riscv64 ; bne X(lhs), X(rhs), =>skip),
            Condition::Lt => dynasm!(self.code ; .arch riscv64 ; blt X(lhs), X(rhs), =>skip),
            Condition::Ge => dynasm!(self.code ; .arch riscv64 ; bge X(lhs), X(rhs), =>skip),
        }
        self.jump(target);
        self.bind(skip);
    }

    fn immediate(&mut self, reg: u8, value: i64) {
        if (-2048..=2047).contains(&value) {
            dynasm!(self.code ; .arch riscv64 ; addi X(reg), zero, value as i32);
        } else {
            dynasm!(self.code ; .arch riscv64 ; li X(reg), value);
        }
    }

    fn operand(&mut self, operand: Operand, scratch: u8) -> u8 {
        match operand {
            Operand::Arg(index) => FIRST_ARG + index,
            Operand::Local(index) => FIRST_LOCAL + index as u8,
            Operand::Immediate(value) => {
                self.immediate(scratch, value as i64);
                scratch
            }
        }
    }

    #[allow(
        clippy::unnecessary_cast,
        reason = "dynasm! validates a shift immediate using a u32 cast"
    )]
    fn arithmetic(
        &mut self,
        op: FixnumArithmetic,
        lhs: Operand,
        rhs: Option<Operand>,
        miss: Option<DynamicLabel>,
    ) {
        use FixnumArithmetic::{Add, Dec, Inc, Mul, Sub};
        const HIGH: u8 = 28; // t3
        let lhs = self.operand(lhs, LEFT);
        let tag = TAG_FIXNUM as i32;
        match op {
            Inc | Dec => {
                let unit = 1i32 << FIXNUM_SHIFT;
                let delta = if op == Inc { unit } else { -unit };
                dynasm!(self.code ; .arch riscv64 ; addi t0, X(lhs), delta);
                if let Some(miss) = miss {
                    let (smaller, larger) = if op == Inc {
                        (RESULT, lhs)
                    } else {
                        (lhs, RESULT)
                    };
                    self.branch(Condition::Lt, smaller, larger, miss);
                }
            }
            Add | Sub => {
                let rhs = self.operand(rhs.expect("binary arithmetic"), RIGHT);
                dynasm!(self.code ; .arch riscv64 ; addi t1, X(rhs), -tag);
                if op == Add {
                    dynasm!(self.code ; .arch riscv64 ; add t0, X(lhs), t1);
                } else {
                    dynasm!(self.code ; .arch riscv64 ; sub t0, X(lhs), t1);
                }
                if let Some(miss) = miss {
                    // For a fitting sum, the result is below lhs iff rhs is
                    // negative. For subtraction compare in the other direction.
                    let (a, b) = if op == Add {
                        (RESULT, lhs)
                    } else {
                        (lhs, RESULT)
                    };
                    dynasm!(self.code ; .arch riscv64 ; slt t2, X(a), X(b) ; slt t3, t1, zero);
                    self.branch(Condition::Ne, LEFT, HIGH, miss);
                }
            }
            Mul => {
                let rhs = self.operand(rhs.expect("binary arithmetic"), RIGHT);
                dynasm!(self.code
                    ; .arch riscv64
                    ; srai t0, X(lhs), FIXNUM_SHIFT
                    ; addi t1, X(rhs), -tag
                );
                if let Some(miss) = miss {
                    dynasm!(self.code
                        ; .arch riscv64
                        ; .feature m
                        ; mulh t3, t0, t1
                        ; mul t0, t0, t1
                        ; srai t2, t0, 63
                    );
                    self.branch(Condition::Ne, LEFT, HIGH, miss);
                } else {
                    dynasm!(self.code ; .arch riscv64 ; .feature m ; mul t0, t0, t1);
                }
                dynasm!(self.code ; .arch riscv64 ; addi t0, t0, tag);
            }
        }
    }

    fn compare(&mut self, relation: FixnumComparison, lhs: Operand, rhs: Operand) {
        use FixnumComparison::{Eq, Ge, Gt, Le, Lt, Ne};
        let lhs = self.operand(lhs, LEFT);
        let rhs = self.operand(rhs, RIGHT);
        match relation {
            Eq => dynasm!(self.code ; .arch riscv64 ; xor t0, X(lhs), X(rhs) ; seqz t0, t0),
            Ne => dynasm!(self.code ; .arch riscv64 ; xor t0, X(lhs), X(rhs) ; snez t0, t0),
            Lt | Ge => dynasm!(self.code ; .arch riscv64 ; slt t0, X(lhs), X(rhs)),
            Gt | Le => dynasm!(self.code ; .arch riscv64 ; slt t0, X(rhs), X(lhs)),
        }
        if matches!(relation, Le | Ge) {
            dynasm!(self.code ; .arch riscv64 ; xori t0, t0, 1);
        }
        dynasm!(self.code ; .arch riscv64 ; addi t0, t0, WORD_NIL as i32);
    }

    fn check_tag(&mut self, value: u8) {
        dynasm!(self.code
            ; .arch riscv64
            ; andi t0, X(value), TAG_MASK as i32
            ; addi t0, t0, -(TAG_FIXNUM as i32)
        );
    }
}

impl Assembler for LeafAssembler {
    type Label = DynamicLabel;

    fn label(&mut self) -> Self::Label {
        self.code.new_dynamic_label()
    }

    fn bind(&mut self, label: Self::Label) {
        dynasm!(self.code ; .arch riscv64 ; =>label);
    }

    fn jump(&mut self, label: Self::Label) {
        dynasm!(self.code ; .arch riscv64 ; j =>label);
    }

    fn initialize_loop_budget(&mut self, budget: u32) {
        dynasm!(self.code ; .arch riscv64 ; addi t4, zero, budget as i32);
    }

    fn take_backedge(&mut self, miss: Self::Label) {
        const T4: u8 = 29;
        dynasm!(self.code ; .arch riscv64 ; addi t4, t4, -1);
        self.branch(Condition::Lt, T4, ZERO, miss);
    }

    fn test(
        &mut self,
        value: Operand,
        test: Test,
        yes: Self::Label,
        no: Self::Label,
        next: Option<Self::Label>,
    ) {
        let value = self.operand(value, LEFT);
        let (condition, lhs, rhs) = match test {
            Test::Nil => {
                self.immediate(RIGHT, WORD_NIL as i64);
                (Condition::Eq, value, RIGHT)
            }
            Test::Fixnum => {
                self.check_tag(value);
                (Condition::Eq, RESULT, ZERO)
            }
            Test::FixnumInRange(low, high) => {
                self.check_tag(value);
                self.branch(Condition::Ne, RESULT, ZERO, no);
                let tagged = |n| (i64::from(n) << FIXNUM_SHIFT) | TAG_FIXNUM as i64;
                self.immediate(RIGHT, tagged(low));
                self.branch(Condition::Lt, value, RIGHT, no);
                self.immediate(RIGHT, tagged(high));
                (Condition::Ge, RIGHT, value)
            }
        };
        if next == Some(yes) {
            self.branch(condition.invert(), lhs, rhs, no);
        } else {
            self.branch(condition, lhs, rhs, yes);
            if next != Some(no) {
                self.jump(no);
            }
        }
    }

    fn copy(&mut self, destination: usize, value: Operand) {
        let destination = FIRST_LOCAL + destination as u8;
        let source = self.operand(value, destination);
        if source != destination {
            dynasm!(self.code ; .arch riscv64 ; mv X(destination), X(source));
        }
    }

    fn compute(&mut self, destination: usize, operation: Operation, miss: Option<Self::Label>) {
        match operation {
            Operation::Arithmetic {
                op,
                lhs,
                rhs,
                checked,
            } => self.arithmetic(op, lhs, rhs, miss.filter(|_| checked)),
            Operation::Compare { relation, lhs, rhs } => self.compare(relation, lhs, rhs),
            Operation::Bitwise { op, lhs, rhs } => {
                let lhs = self.operand(lhs, LEFT);
                let rhs = self.operand(rhs, RIGHT);
                match op {
                    BitwiseOp::And => dynasm!(self.code ; .arch riscv64 ; and t0, X(lhs), X(rhs)),
                    BitwiseOp::Or => dynasm!(self.code ; .arch riscv64 ; or t0, X(lhs), X(rhs)),
                    BitwiseOp::Xor => dynasm!(self.code
                        ; .arch riscv64
                        ; xor t0, X(lhs), X(rhs)
                        ; ori t0, t0, TAG_FIXNUM as i32
                    ),
                }
            }
            Operation::Complement(value) => {
                let src = self.operand(value, LEFT);
                dynasm!(self.code ; .arch riscv64 ; xori t0, X(src), !(TAG_MASK as i32));
            }
        }
        let destination = FIRST_LOCAL + destination as u8;
        dynasm!(self.code ; .arch riscv64 ; mv X(destination), t0);
    }

    fn return_value(&mut self, value: Operand) {
        let register = self.operand(value, FIRST_ARG);
        if register != FIRST_ARG {
            dynasm!(self.code ; .arch riscv64 ; mv a0, X(register));
        }
        if self.runtime_entry {
            dynasm!(self.code ; .arch riscv64 ; sd a0, [t6, 8]);
        }
        dynasm!(self.code ; .arch riscv64 ; ret);
    }
}
