//! Call-free i686 C entries. Arguments are 32-bit words passed on the stack.

use dynasmrt::x86::{Rd, X86Relocation};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, Register, VecAssembler, dynasm};
use zkcl_bytecode::native_abi::{
    FixnumComparison, JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_ONE, JitReturnLayout, jit_return_layout,
};
use zkcl_bytecode::{FIXNUM_SHIFT, FixnumArithmetic, TAG_FIXNUM, TAG_MASK, WORD_NIL, WORD_T};

use super::{Assembler, BitwiseOp, LeafFunction, Operand, Operation, Test, emit_body};

type MachineCode = VecAssembler<X86Relocation>;

const WORD_BYTES: i8 = 4;
const _: () = {
    let JitReturnLayout {
        tag,
        single_value: payload,
        target_frame: frame,
        state,
        ..
    } = jit_return_layout(WORD_BYTES as i32);
    assert!(tag == 0 && payload == 4 && frame == 8 && state == 12 && JIT_RETURN_TAG_ONE == 0);
    assert!(TAG_MASK <= i8::MAX as u64 && TAG_FIXNUM <= TAG_MASK);
    assert!(1u64 << FIXNUM_SHIFT <= i8::MAX as u64);
    assert!(WORD_T == WORD_NIL + 1);
};

struct LeafAssembler {
    ops: MachineCode,
    runtime_entry: bool,
    frame_bytes: i8,
}

pub(super) fn emit(function: &LeafFunction, runtime_entry: bool) -> Option<Vec<u8>> {
    let frame_bytes = WORD_BYTES * function.locals() as i8;
    let mut asm = LeafAssembler {
        ops: MachineCode::new(0),
        runtime_entry,
        frame_bytes,
    };
    if runtime_entry {
        emit_entry_prologue(&mut asm.ops, function.arity() as i32);
    }
    if frame_bytes != 0 {
        dynasm!(asm.ops ; .arch x86 ; sub esp, BYTE frame_bytes);
    }
    emit_body(&mut asm, function);
    asm.ops.finalize().ok()
}

// The object writer binds the relative jump to the runtime dispatcher.
pub(super) fn dispatch_jump() -> Vec<u8> {
    let mut ops = MachineCode::new(0);
    dynasm!(ops
        ; .arch x86
        ; jmp DWORD >dispatch
        ; dispatch:
    );
    ops.finalize()
        .expect("the dispatch label immediately follows its jump")
}

impl Assembler for LeafAssembler {
    type Label = DynamicLabel;

    fn label(&mut self) -> Self::Label {
        self.ops.new_dynamic_label()
    }

    fn bind(&mut self, label: Self::Label) {
        dynasm!(self.ops ; .arch x86 ; =>label);
    }

    fn jump(&mut self, label: Self::Label) {
        dynasm!(self.ops ; .arch x86 ; jmp =>label);
    }

    fn initialize_loop_budget(&mut self, budget: u32) {
        // Only the register entry emits a loop. EDX is free of the C out pointer.
        dynasm!(self.ops ; .arch x86 ; mov edx, DWORD budget as i32);
    }

    fn take_backedge(&mut self, miss: Self::Label) {
        dynasm!(self.ops ; .arch x86 ; sub edx, BYTE 1 ; jb =>miss);
    }

    fn test(
        &mut self,
        value: Operand,
        test: Test,
        yes: Self::Label,
        no: Self::Label,
        next: Option<Self::Label>,
    ) {
        self.load_operand(Rd::EAX, value);
        let (target, invert) = if Some(yes) == next {
            (no, true)
        } else {
            (yes, false)
        };
        emit_test_branch(&mut self.ops, test, no, target, invert);
        if next != Some(yes) && next != Some(no) {
            self.jump(no);
        }
    }

    fn copy(&mut self, destination: usize, value: Operand) {
        self.load_operand(Rd::EAX, value);
        self.store_local(destination);
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
            Operation::Bitwise { op, lhs, rhs } => self.bitwise(op, lhs, rhs),
            Operation::Complement(value) => {
                self.load_operand(Rd::EAX, value);
                dynasm!(self.ops ; .arch x86 ; xor eax, BYTE !(TAG_MASK as i8));
            }
        }
        self.store_local(destination);
    }

    fn return_value(&mut self, value: Operand) {
        self.load_operand(Rd::EAX, value);
        if self.runtime_entry {
            dynasm!(self.ops ; .arch x86 ; mov [edx + 4], eax);
        }
        if self.frame_bytes != 0 {
            dynasm!(self.ops ; .arch x86 ; add esp, BYTE self.frame_bytes);
        }
        dynasm!(self.ops ; .arch x86 ; ret);
    }
}

fn emit_entry_prologue(ops: &mut MachineCode, arity: i32) {
    dynasm!(ops
        ; .arch x86
        ; mov edx, [esp + 20]   // out
        ; xor eax, eax
        ; mov [edx], eax        // one value, with initialized padding
        ; mov [edx + 8], eax    // target_frame
        ; mov [edx + 12], eax   // state
        ; cmp DWORD [esp + 16], DWORD arity
        ; je BYTE >correct_arity
        ; mov BYTE [edx], BYTE JIT_RETURN_TAG_FAULT as i8
        ; mov [edx + 4], eax
        ; ret
        ; correct_arity:
    );
}

fn emit_test_branch(
    ops: &mut MachineCode,
    test: Test,
    fail: DynamicLabel,
    target: DynamicLabel,
    invert: bool,
) {
    let equality = match test {
        Test::Nil => {
            dynasm!(ops ; .arch x86 ; cmp eax, DWORD WORD_NIL as i32);
            true
        }
        Test::Fixnum => {
            compare_fixnum_tag(ops);
            true
        }
        Test::FixnumInRange(low, high) if low == high => {
            dynasm!(ops ; .arch x86 ; cmp eax, DWORD tagged_fixnum(low));
            true
        }
        Test::FixnumInRange(low, high) => {
            compare_fixnum_tag(ops);
            // The signed tagged words preserve the ordering of their 29-bit payloads.
            dynasm!(ops
                ; .arch x86
                ; jne =>fail
                ; cmp eax, DWORD tagged_fixnum(low)
                ; jl =>fail
                ; cmp eax, DWORD tagged_fixnum(high)
            );
            false
        }
    };
    match (equality, invert) {
        (true, false) => dynasm!(ops ; .arch x86 ; je =>target),
        (true, true) => dynasm!(ops ; .arch x86 ; jne =>target),
        (false, false) => dynasm!(ops ; .arch x86 ; jle =>target),
        (false, true) => dynasm!(ops ; .arch x86 ; jg =>target),
    }
}

fn tagged_fixnum(bound: i32) -> i32 {
    (bound << FIXNUM_SHIFT) | TAG_FIXNUM as i32
}

fn compare_fixnum_tag(ops: &mut MachineCode) {
    dynasm!(ops
        ; .arch x86
        ; mov ecx, eax
        ; and cl, BYTE TAG_MASK as i8
        ; cmp cl, BYTE TAG_FIXNUM as i8
    );
}

impl LeafAssembler {
    fn store_local(&mut self, destination: usize) {
        let offset = WORD_BYTES * destination as i8;
        dynasm!(self.ops ; .arch x86 ; mov [BYTE esp + offset], eax);
    }

    fn load_operand(&mut self, dst: Rd, operand: Operand) {
        match operand {
            Operand::Immediate(word) => {
                dynasm!(self.ops ; .arch x86 ; mov Rd(dst.code()), DWORD word as i32);
            }
            Operand::Arg(arg) if self.runtime_entry => {
                let args = self.frame_bytes + 12;
                let offset = WORD_BYTES * arg as i8;
                dynasm!(self.ops
                    ; .arch x86
                    ; mov Rd(dst.code()), [BYTE esp + args]
                    ; mov Rd(dst.code()), [BYTE Rd(dst.code()) + offset]
                );
            }
            Operand::Arg(arg) => {
                let offset = self.frame_bytes + WORD_BYTES * (1 + arg as i8);
                dynasm!(self.ops ; .arch x86 ; mov Rd(dst.code()), [BYTE esp + offset]);
            }
            Operand::Local(index) => {
                let offset = WORD_BYTES * index as i8;
                dynasm!(self.ops ; .arch x86 ; mov Rd(dst.code()), [BYTE esp + offset]);
            }
        }
    }

    // EAX and ECX are caller-saved. EDX retains the adapter's output pointer.
    fn arithmetic(
        &mut self,
        op: FixnumArithmetic,
        lhs: Operand,
        rhs: Option<Operand>,
        miss: Option<DynamicLabel>,
    ) {
        use FixnumArithmetic::{Add, Dec, Inc, Mul, Sub};
        self.load_operand(Rd::EAX, lhs);
        if let Some(rhs) = rhs {
            self.load_operand(Rd::ECX, rhs);
        }
        let step = 1i8 << FIXNUM_SHIFT;
        let tag = TAG_FIXNUM as i8;
        match op {
            Inc => dynasm!(self.ops ; .arch x86 ; add eax, BYTE step),
            Dec => dynasm!(self.ops ; .arch x86 ; sub eax, BYTE step),
            Add if miss.is_none() => {
                dynasm!(self.ops ; .arch x86 ; lea eax, [BYTE eax + ecx - tag])
            }
            Add => dynasm!(self.ops ; .arch x86 ; sub ecx, BYTE tag ; add eax, ecx),
            Sub => dynasm!(self.ops ; .arch x86 ; sub ecx, BYTE tag ; sub eax, ecx),
            Mul => dynasm!(self.ops
                ; .arch x86
                ; and eax, BYTE !(TAG_MASK as i8)
                ; sar ecx, BYTE FIXNUM_SHIFT as i8
                ; imul eax, ecx
            ),
        }
        if let Some(miss) = miss {
            dynasm!(self.ops ; .arch x86 ; jo =>miss);
        }
        if op == Mul {
            dynasm!(self.ops ; .arch x86 ; or al, BYTE tag);
        }
    }

    fn bitwise(&mut self, op: BitwiseOp, lhs: Operand, rhs: Operand) {
        use BitwiseOp::{And, Or, Xor};

        self.load_operand(Rd::EAX, lhs);
        if let Operand::Immediate(word) = rhs {
            let word = word as i32;
            match op {
                And => dynasm!(self.ops ; .arch x86 ; and eax, DWORD word),
                Or => dynasm!(self.ops ; .arch x86 ; or eax, DWORD word),
                Xor => dynasm!(self.ops ; .arch x86 ; xor eax, DWORD (word & !(TAG_MASK as i32))),
            }
            return;
        }
        let tag = TAG_FIXNUM as i8;
        self.load_operand(Rd::ECX, rhs);
        match op {
            And => dynasm!(self.ops ; .arch x86 ; and eax, ecx),
            Or => dynasm!(self.ops ; .arch x86 ; or eax, ecx),
            Xor => dynasm!(self.ops ; .arch x86 ; xor eax, ecx ; or al, BYTE tag),
        }
    }

    fn compare(&mut self, relation: FixnumComparison, lhs: Operand, rhs: Operand) {
        self.load_operand(Rd::EAX, lhs);
        self.load_operand(Rd::ECX, rhs);
        dynasm!(self.ops ; .arch x86 ; cmp eax, ecx);
        match relation {
            FixnumComparison::Eq => dynasm!(self.ops ; .arch x86 ; sete al),
            FixnumComparison::Ne => dynasm!(self.ops ; .arch x86 ; setne al),
            FixnumComparison::Lt => dynasm!(self.ops ; .arch x86 ; setl al),
            FixnumComparison::Le => dynasm!(self.ops ; .arch x86 ; setle al),
            FixnumComparison::Gt => dynasm!(self.ops ; .arch x86 ; setg al),
            FixnumComparison::Ge => dynasm!(self.ops ; .arch x86 ; setge al),
        }
        dynasm!(self.ops ; .arch x86 ; movzx eax, al ; add eax, BYTE WORD_NIL as i8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_dispatch_jump_has_a_rel32_field() {
        assert_eq!(dispatch_jump(), [0xe9, 0, 0, 0, 0]);
    }
}
