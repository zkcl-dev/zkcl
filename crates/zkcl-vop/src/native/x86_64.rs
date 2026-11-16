use dynasmrt::x64::{Rq, X64Relocation};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, Register, VecAssembler, dynasm};
use zkcl_bytecode::native_abi::{
    FixnumComparison, JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_ONE, JitReturnLayout, jit_return_layout,
};
use zkcl_bytecode::{FIXNUM_SHIFT, FixnumArithmetic, TAG_FIXNUM, TAG_MASK, WORD_NIL, WORD_T};

use super::{Assembler, BitwiseOp, LeafFunction, Operand, Operation, Test, emit_body};

type MachineCode = VecAssembler<X64Relocation>;

const SYSV_ARGS: [Rq; 4] = [Rq::RDI, Rq::RSI, Rq::RDX, Rq::RCX];
const WINDOWS_ARGS: [Rq; 4] = [Rq::RCX, Rq::RDX, Rq::R8, Rq::R9];

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
    assert!(TAG_MASK <= i8::MAX as u64 && TAG_FIXNUM <= TAG_MASK);
    assert!(1u64 << FIXNUM_SHIFT <= i8::MAX as u64);
    assert!(WORD_T == WORD_NIL + 1);
};

struct LeafAssembler {
    ops: MachineCode,
    args: [Rq; 4],
    windows: bool,
    runtime_entry: bool,
}

pub(super) fn emit(function: &LeafFunction, windows: bool, runtime_entry: bool) -> Option<Vec<u8>> {
    let args = if windows { WINDOWS_ARGS } else { SYSV_ARGS };
    let mut asm = LeafAssembler {
        ops: MachineCode::new(0),
        args,
        windows,
        runtime_entry,
    };
    if runtime_entry {
        emit_entry_prologue(&mut asm.ops, &args[..function.arity()], windows);
    }
    emit_body(&mut asm, function);
    asm.ops.finalize().ok()
}

// The object writer replaces the jump displacement with an external relocation.
pub(super) fn dispatch_jump() -> Vec<u8> {
    let mut ops = MachineCode::new(0);
    dynasm!(ops
        ; .arch x64
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
        dynasm!(self.ops ; .arch x64 ; =>label);
    }

    fn jump(&mut self, label: Self::Label) {
        dynasm!(self.ops ; .arch x64 ; jmp =>label);
    }

    fn initialize_loop_budget(&mut self, budget: u32) {
        // A loop can miss, so its C entry is the runtime dispatcher. The
        // register entry can use R11, which otherwise holds the C out pointer.
        dynasm!(self.ops ; .arch x64 ; mov r11d, DWORD budget as i32);
    }

    fn take_backedge(&mut self, miss: Self::Label) {
        dynasm!(self.ops ; .arch x64 ; sub r11d, BYTE 1 ; jb =>miss);
    }

    fn test(
        &mut self,
        value: Operand,
        test: Test,
        yes: Self::Label,
        no: Self::Label,
        next: Option<Self::Label>,
    ) {
        let reg = self.operand_register(value);
        let (target, invert) = if Some(yes) == next {
            (no, true)
        } else {
            (yes, false)
        };
        emit_test_branch(&mut self.ops, reg, test, no, target, invert);
        if next != Some(yes) && next != Some(no) {
            self.jump(no);
        }
    }

    fn copy(&mut self, destination: usize, value: Operand) {
        self.load_operand(Rq::RAX, value);
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
                self.load_operand(Rq::RAX, value);
                dynasm!(self.ops ; .arch x64 ; xor rax, BYTE !(TAG_MASK as i8));
            }
        }
        self.store_local(destination);
    }

    fn return_value(&mut self, value: Operand) {
        self.load_operand(Rq::RAX, value);
        if self.runtime_entry {
            dynasm!(self.ops ; .arch x64 ; mov [r11 + 8], rax);
        }
        dynasm!(self.ops ; .arch x64 ; ret);
    }
}

fn emit_entry_prologue(ops: &mut MachineCode, args: &[Rq], windows: bool) {
    let argc = if windows {
        // The return address and four home slots precede the fifth argument.
        dynasm!(ops
            ; .arch x64
            ; mov r11, [rsp + 40] // out
            ; mov r10, r8         // args
        );
        Rq::R9
    } else {
        dynasm!(ops
            ; .arch x64
            ; mov r11, r8        // out
            ; mov r10, rdx       // args
        );
        Rq::RCX
    };
    let arity = args.len() as i32;
    dynasm!(ops
        ; .arch x64
        ; xor eax, eax
        ; mov [r11], rax       // one value, with initialized padding
        ; mov [r11 + 16], rax  // target_frame
        ; mov [r11 + 24], rax  // state
        ; cmp Rq(argc.code()), DWORD arity
        ; je BYTE >correct_arity
        ; mov BYTE [r11], BYTE JIT_RETURN_TAG_FAULT as i8
        ; mov [r11 + 8], rax
        ; ret
        ; correct_arity:
    );
    // R10 holds the source base while original argument registers are overwritten.
    for (index, reg) in args.iter().enumerate() {
        let offset = (index * 8) as i8;
        dynasm!(ops ; .arch x64 ; mov Rq(reg.code()), [BYTE r10 + offset]);
    }
}

fn emit_test_branch(
    ops: &mut MachineCode,
    arg: Rq,
    test: Test,
    fail: DynamicLabel,
    target: DynamicLabel,
    invert: bool,
) {
    let equality = match test {
        Test::Nil => {
            compare_immediate(ops, arg, WORD_NIL as i64);
            true
        }
        Test::Fixnum => {
            compare_fixnum_tag(ops, arg);
            true
        }
        Test::FixnumInRange(low, high) if low == high => {
            compare_immediate(ops, arg, tagged_fixnum(low));
            true
        }
        Test::FixnumInRange(low, high) => {
            compare_fixnum_tag(ops, arg);
            dynasm!(ops ; .arch x64 ; jne =>fail);
            // The signed tagged words preserve the ordering of their payloads.
            compare_immediate(ops, arg, tagged_fixnum(low));
            dynasm!(ops ; .arch x64 ; jl =>fail);
            compare_immediate(ops, arg, tagged_fixnum(high));
            false
        }
    };
    match (equality, invert) {
        (true, false) => dynasm!(ops ; .arch x64 ; je =>target),
        (true, true) => dynasm!(ops ; .arch x64 ; jne =>target),
        (false, false) => dynasm!(ops ; .arch x64 ; jle =>target),
        (false, true) => dynasm!(ops ; .arch x64 ; jg =>target),
    }
}

fn tagged_fixnum(bound: i32) -> i64 {
    (i64::from(bound) << FIXNUM_SHIFT) | TAG_FIXNUM as i64
}

fn compare_fixnum_tag(ops: &mut MachineCode, arg: Rq) {
    dynasm!(ops
        ; .arch x64
        ; mov eax, Rd(arg.code())
        ; and eax, BYTE TAG_MASK as i8
        ; cmp al, BYTE TAG_FIXNUM as i8
    );
}

fn compare_immediate(ops: &mut MachineCode, arg: Rq, word: i64) {
    if let Ok(word) = i32::try_from(word) {
        dynasm!(ops ; .arch x64 ; cmp Rq(arg.code()), DWORD word);
    } else {
        load_immediate(ops, Rq::RAX, word as u64);
        dynasm!(ops ; .arch x64 ; cmp Rq(arg.code()), rax);
    }
}

fn load_immediate(ops: &mut MachineCode, dst: Rq, word: u64) {
    if let Ok(word) = u32::try_from(word) {
        // The 32-bit MOV zero-extends the tagged word.
        dynasm!(ops ; .arch x64 ; mov Rd(dst.code()), DWORD word as i32);
    } else {
        dynasm!(ops ; .arch x64 ; mov Rq(dst.code()), QWORD word as i64);
    }
}

impl LeafAssembler {
    fn store_local(&mut self, destination: usize) {
        let offset = self.local_offset(destination);
        dynasm!(self.ops ; .arch x64 ; mov [BYTE rsp + offset], rax);
    }

    fn local_offset(&self, index: usize) -> i8 {
        let bytes = 8 * (index as i8 + 1);
        // Win64 always provides four caller-allocated home slots. SysV offers
        // a red zone. Both keep RSP unchanged and require no unwind record.
        if self.windows { bytes } else { -bytes }
    }

    fn load_operand(&mut self, dst: Rq, operand: Operand) {
        match operand {
            Operand::Arg(arg) => {
                let src = self.args[usize::from(arg)];
                dynasm!(self.ops ; .arch x64 ; mov Rq(dst.code()), Rq(src.code()));
            }
            Operand::Immediate(word) => load_immediate(&mut self.ops, dst, word),
            Operand::Local(index) => {
                let offset = self.local_offset(index);
                dynasm!(self.ops ; .arch x64 ; mov Rq(dst.code()), [BYTE rsp + offset]);
            }
        }
    }

    fn operand_register(&mut self, value: Operand) -> Rq {
        if let Operand::Arg(arg) = value {
            return self.args[usize::from(arg)];
        }
        self.load_operand(Rq::R10, value);
        Rq::R10
    }

    // RAX and R10 are caller-saved on both ABIs. R11 retains the adapter's out pointer.
    fn arithmetic(
        &mut self,
        op: FixnumArithmetic,
        lhs: Operand,
        rhs: Option<Operand>,
        miss: Option<DynamicLabel>,
    ) {
        use FixnumArithmetic::{Add, Dec, Inc, Mul, Sub};
        self.load_operand(Rq::RAX, lhs);
        let step = 1i8 << FIXNUM_SHIFT;
        let tag = TAG_FIXNUM as i8;
        match op {
            Inc => dynasm!(self.ops ; .arch x64 ; add rax, BYTE step),
            Dec => dynasm!(self.ops ; .arch x64 ; sub rax, BYTE step),
            Add => {
                let source = self.operand_register(rhs.expect("binary arithmetic"));
                if miss.is_some() {
                    dynasm!(self.ops ; .arch x64 ; sub rax, BYTE tag ; add rax, Rq(source.code()));
                } else {
                    dynasm!(self.ops ; .arch x64 ; lea rax, [BYTE rax + Rq(source.code()) * 1 - tag]);
                }
            }
            Sub => {
                self.load_operand(Rq::R10, rhs.expect("binary arithmetic"));
                dynasm!(self.ops ; .arch x64 ; sub r10, BYTE tag ; sub rax, r10);
            }
            Mul => {
                self.load_operand(Rq::R10, rhs.expect("binary arithmetic"));
                dynasm!(self.ops
                    ; .arch x64
                    ; and rax, BYTE !(TAG_MASK as i8)
                    ; sar r10, BYTE FIXNUM_SHIFT as i8
                    ; imul rax, r10
                );
            }
        }
        if let Some(miss) = miss {
            dynasm!(self.ops ; .arch x64 ; jo =>miss);
        }
        if op == Mul {
            dynasm!(self.ops ; .arch x64 ; or al, BYTE tag);
        }
    }

    fn bitwise(&mut self, op: BitwiseOp, lhs: Operand, rhs: Operand) {
        use BitwiseOp::{And, Or, Xor};

        self.load_operand(Rq::RAX, lhs);
        let tag = TAG_FIXNUM as i8;
        if let Operand::Immediate(word) = rhs
            && let Ok(word) = i32::try_from(word as i64)
        {
            match op {
                And => dynasm!(self.ops ; .arch x64 ; and rax, DWORD word),
                Or => dynasm!(self.ops ; .arch x64 ; or rax, DWORD word),
                Xor => dynasm!(self.ops ; .arch x64 ; xor rax, DWORD (word & !(TAG_MASK as i32))),
            }
            return;
        }
        let rhs = self.operand_register(rhs);
        match op {
            And => dynasm!(self.ops ; .arch x64 ; and rax, Rq(rhs.code())),
            Or => dynasm!(self.ops ; .arch x64 ; or rax, Rq(rhs.code())),
            Xor => dynasm!(self.ops ; .arch x64 ; xor rax, Rq(rhs.code()) ; or al, BYTE tag),
        }
    }

    fn compare(&mut self, relation: FixnumComparison, lhs: Operand, rhs: Operand) {
        self.load_operand(Rq::RAX, lhs);
        let rhs = self.operand_register(rhs);
        dynasm!(self.ops ; .arch x64 ; cmp rax, Rq(rhs.code()));
        match relation {
            FixnumComparison::Eq => dynasm!(self.ops ; .arch x64 ; sete al),
            FixnumComparison::Ne => dynasm!(self.ops ; .arch x64 ; setne al),
            FixnumComparison::Lt => dynasm!(self.ops ; .arch x64 ; setl al),
            FixnumComparison::Le => dynasm!(self.ops ; .arch x64 ; setle al),
            FixnumComparison::Gt => dynasm!(self.ops ; .arch x64 ; setg al),
            FixnumComparison::Ge => dynasm!(self.ops ; .arch x64 ; setge al),
        }
        dynasm!(self.ops ; .arch x64 ; movzx eax, al ; add eax, BYTE WORD_NIL as i8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leaf::{Block, Exit, Step};

    #[test]
    fn returns_arguments_from_the_abi_registers() {
        let modrm_by_abi = [[0xf8, 0xf0, 0xd0, 0xc8], [0xc8, 0xd0, 0xc0, 0xc8]];
        for (abi, modrm) in modrm_by_abi.into_iter().enumerate() {
            let windows = abi == 1;
            for (arg, modrm) in modrm.into_iter().enumerate() {
                let leaf_function = LeafFunction {
                    loop_budget: None,
                    arity: arg + 1,
                    word_bits: 64,
                    guards: Vec::new(),
                    blocks: vec![Block {
                        steps: vec![],
                        exit: Exit::Return(Operand::Arg(arg as u8)),
                    }],
                    locals: 0,
                };
                let rex = if windows && arg >= 2 { 0x4c } else { 0x48 };
                assert_eq!(
                    emit(&leaf_function, windows, false).unwrap(),
                    [rex, 0x89, modrm, 0xc3]
                );
            }
        }
    }

    #[test]
    fn object_dispatch_jump_has_a_rel32_field() {
        assert_eq!(dispatch_jump(), [0xe9, 0, 0, 0, 0]);
    }

    #[test]
    fn adding_extended_argument_registers_encodes_the_sib_index() {
        let leaf_function = LeafFunction {
            loop_budget: None,
            arity: 4,
            word_bits: 64,
            guards: Vec::new(),
            blocks: vec![Block {
                steps: vec![Step::Compute {
                    destination: 0,
                    op: Operation::Arithmetic {
                        op: FixnumArithmetic::Add,
                        lhs: Operand::Arg(2),
                        rhs: Some(Operand::Arg(3)),
                        checked: false,
                    },
                }],
                exit: Exit::Return(Operand::Local(0)),
            }],
            locals: 1,
        };
        assert_eq!(
            emit(&leaf_function, true, false).unwrap(),
            [
                0x4c, 0x89, 0xc0, // mov rax, r8
                0x4a, 0x8d, 0x44, 0x08, 0xff, // lea rax, [rax + r9 - 1]
                0x48, 0x89, 0x44, 0x24, 0x08, // mov [rsp + 8], rax
                0x48, 0x8b, 0x44, 0x24, 0x08, // mov rax, [rsp + 8]
                0xc3,
            ]
        );
    }
}
