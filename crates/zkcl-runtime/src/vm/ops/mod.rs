mod dispatch;
mod lower;

use zkcl_bytecode::Instruction;
use zkcl_bytecode::native_abi::IntegerBitPrimitive;

use super::VmCode;

pub(in crate::vm) use self::dispatch::dispatch_vm_op;
pub(crate) use self::dispatch::exec_cmp_branch_fast;
pub(crate) use self::lower::lower_code;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CmpKind {
    EqNum,
    NeNum,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Eql,
}

impl CmpKind {
    #[inline(always)]
    pub(super) fn matches_primitive(self, primitive: super::Primitive) -> bool {
        use super::{Primitive, numbers::Op};
        matches!(
            (self, primitive),
            (Self::EqNum, Primitive::Number(Op::Eq))
                | (Self::NeNum, Primitive::Number(Op::Ne))
                | (Self::Lt, Primitive::Number(Op::Lt))
                | (Self::Le, Primitive::Number(Op::Le))
                | (Self::Gt, Primitive::Number(Op::Gt))
                | (Self::Ge, Primitive::Number(Op::Ge))
                | (Self::Eq, Primitive::Eq)
                | (Self::Eql, Primitive::Eql)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArithKind {
    Add,
    Sub,
    Mul,
}

impl ArithKind {
    #[inline(always)]
    pub(super) fn matches_primitive(self, primitive: super::Primitive) -> bool {
        use super::{Primitive, numbers::Op};
        matches!(
            (self, primitive),
            (Self::Add, Primitive::Number(Op::Add))
                | (Self::Sub, Primitive::Number(Op::Sub))
                | (Self::Mul, Primitive::Number(Op::Mul))
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CanonicalSymbol {
    Add,
    Aref,
    Sub,
    Mul,
    Div,
    Inc,
    Dec,
    Funcall,
    EqNum,
    NeNum,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Eql,
    Car,
    Cdr,
    Cons,
    Null,
    Typep,
    SingleFloat,
    DoubleFloat,
    IntegerBits(IntegerBitPrimitive),
}

impl CanonicalSymbol {
    pub(crate) fn from_cl_name(name: &str) -> Option<Self> {
        Some(match name {
            "+" => Self::Add,
            "AREF" => Self::Aref,
            "-" => Self::Sub,
            "*" => Self::Mul,
            "/" => Self::Div,
            "1+" => Self::Inc,
            "1-" => Self::Dec,
            "FUNCALL" => Self::Funcall,
            "=" => Self::EqNum,
            "/=" => Self::NeNum,
            "<" => Self::Lt,
            "<=" => Self::Le,
            ">" => Self::Gt,
            ">=" => Self::Ge,
            "EQ" => Self::Eq,
            "EQL" => Self::Eql,
            "CAR" => Self::Car,
            "CDR" | "REST" => Self::Cdr,
            "CONS" => Self::Cons,
            "NULL" | "NOT" => Self::Null,
            "TYPEP" => Self::Typep,
            "SINGLE-FLOAT" => Self::SingleFloat,
            "DOUBLE-FLOAT" => Self::DoubleFloat,
            _ => Self::IntegerBits(IntegerBitPrimitive::named(name)?),
        })
    }

    fn cmp_kind(self) -> Option<CmpKind> {
        Some(match self {
            Self::EqNum => CmpKind::EqNum,
            Self::NeNum => CmpKind::NeNum,
            Self::Lt => CmpKind::Lt,
            Self::Le => CmpKind::Le,
            Self::Gt => CmpKind::Gt,
            Self::Ge => CmpKind::Ge,
            Self::Eq => CmpKind::Eq,
            Self::Eql => CmpKind::Eql,
            _ => return None,
        })
    }

    fn arith_kind(self) -> Option<ArithKind> {
        Some(match self {
            Self::Add => ArithKind::Add,
            Self::Sub => ArithKind::Sub,
            Self::Mul => ArithKind::Mul,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Lowered {
    op: VmOp,
    consumed: u8,
}

pub(crate) struct LoweredCode {
    pub(crate) vm_ops: Box<[VmOp]>,
    // One-to-one lowering preserves every canonical PC, including the end position.
    pub(crate) pc_mapping: Option<PcMapping>,
}

pub(crate) struct PcMapping {
    pub(crate) canonical_by_vm: Box<[u32]>,
    pub(crate) vm_by_canonical: Box<[u32]>,
}

pub(super) fn resume_native_call(code: &VmCode, pc: usize) -> super::Resume {
    use super::{FusedBranch, Resume};

    match code.vm_ops[code.vm_target(pc)] {
        VmOp::CmpBranch {
            out,
            target,
            clear_start,
            clear_len,
            ..
        }
        | VmOp::NullBranch {
            out,
            target,
            clear_start,
            clear_len,
            ..
        } => Resume::BranchUnless(FusedBranch {
            target,
            out,
            clear_start,
            clear_len,
        }),
        _ => match code.canonical[pc] {
            Instruction::Call { out, .. } => Resume::Call(out),
            Instruction::CallValues { .. } => Resume::Call(None),
            _ => unreachable!("only a call has a callee continuation"),
        },
    }
}

macro_rules! vm_operand_ty {
    (reg) => {
        u16
    };
    (reg_start($len:ident)) => {
        u16
    };
    (reg_len) => {
        u16
    };
    (fixed_reg_start($count:expr)) => {
        u16
    };
    (capture) => {
        u16
    };
    (constant) => {
        u32
    };
    (constant16) => {
        u16
    };
    (arg_index) => {
        u32
    };
    (binding_count) => {
        u16
    };
    (branch) => {
        u32
    };
    (canonical_pc) => {
        u32
    };
    (imm_i64) => {
        i64
    };
    (imm_i32) => {
        i32
    };
    (char) => {
        char
    };
    (bool) => {
        bool
    };
    (cmp_kind) => {
        CmpKind
    };
    (arith_kind) => {
        ArithKind
    };
    (fixnum_arith) => {
        zkcl_bytecode::FixnumArithmetic
    };
}

macro_rules! remap_operand {
    (branch, $field:ident, $pc_map:ident) => {
        *$field = $pc_map[*$field as usize];
    };
    ($kind:ident $(($($arg:tt)*))?, $field:ident, $pc_map:ident) => {
        let _ = $field;
    };
}

#[cfg(any(debug_assertions, test))]
#[inline]
fn check_index(
    value: usize,
    limit: usize,
    vm_pc: usize,
    variant: &str,
    label: &str,
    field: &str,
) -> Result<(), String> {
    if value >= limit {
        return Err(format!(
            "vm_op {vm_pc} ({variant}): {label} {field}={value} >= {limit}"
        ));
    }
    Ok(())
}

#[cfg(any(debug_assertions, test))]
#[inline]
fn check_range(
    start: u16,
    count: usize,
    registers: usize,
    vm_pc: usize,
    variant: &str,
) -> Result<(), String> {
    let end = usize::from(start) + count;
    if end > registers {
        return Err(format!(
            "vm_op {vm_pc} ({variant}): register range {start}..{end} > {registers}"
        ));
    }
    Ok(())
}

#[cfg(any(debug_assertions, test))]
macro_rules! verify_operand {
    (
        reg, $field:ident, $vm_pc:ident, $variant:expr,
        $vm_ops_len:ident, $logical_pc:ident, $registers:ident, $constants_len:ident, $captures_len:ident
    ) => {
        check_index(
            usize::from(*$field),
            $registers,
            $vm_pc,
            $variant,
            "register",
            stringify!($field),
        )?;
    };
    (
        capture, $field:ident, $vm_pc:ident, $variant:expr,
        $vm_ops_len:ident, $logical_pc:ident, $registers:ident, $constants_len:ident, $captures_len:ident
    ) => {
        check_index(
            usize::from(*$field),
            $captures_len,
            $vm_pc,
            $variant,
            "capture",
            stringify!($field),
        )?;
    };
    (
        constant, $field:ident, $vm_pc:ident, $variant:expr,
        $vm_ops_len:ident, $logical_pc:ident, $registers:ident, $constants_len:ident, $captures_len:ident
    ) => {
        check_index(
            *$field as usize,
            $constants_len,
            $vm_pc,
            $variant,
            "constant",
            stringify!($field),
        )?;
    };
    (
        constant16, $field:ident, $vm_pc:ident, $variant:expr,
        $vm_ops_len:ident, $logical_pc:ident, $registers:ident, $constants_len:ident, $captures_len:ident
    ) => {
        check_index(
            usize::from(*$field),
            $constants_len,
            $vm_pc,
            $variant,
            "constant",
            stringify!($field),
        )?;
    };
    (
        reg_start($len:ident), $field:ident, $vm_pc:ident, $variant:expr,
        $vm_ops_len:ident, $logical_pc:ident, $registers:ident, $constants_len:ident, $captures_len:ident
    ) => {
        check_range(*$field, usize::from(*$len), $registers, $vm_pc, $variant)?;
    };
    (
        fixed_reg_start($count:expr), $field:ident, $vm_pc:ident, $variant:expr,
        $vm_ops_len:ident, $logical_pc:ident, $registers:ident, $constants_len:ident, $captures_len:ident
    ) => {
        check_range(*$field, $count, $registers, $vm_pc, $variant)?;
    };
    (
        branch, $field:ident, $vm_pc:ident, $variant:expr,
        $vm_ops_len:ident, $logical_pc:ident, $registers:ident, $constants_len:ident, $captures_len:ident
    ) => {
        if (*$field as usize) > $vm_ops_len {
            return Err(format!(
                "vm_op {} ({}): branch target {} > {}",
                $vm_pc, $variant, *$field, $vm_ops_len
            ));
        }
    };
    (
        canonical_pc, $field:ident, $vm_pc:ident, $variant:expr,
        $vm_ops_len:ident, $logical_pc:ident, $registers:ident, $constants_len:ident, $captures_len:ident
    ) => {
        if *$field != $logical_pc {
            return Err(format!(
                "vm_op {} ({}): canonical PC {} differs from {}",
                $vm_pc, $variant, *$field, $logical_pc
            ));
        }
    };
    (
        $other:ident, $field:ident, $vm_pc:ident, $variant:expr,
        $vm_ops_len:ident, $logical_pc:ident, $registers:ident, $constants_len:ident, $captures_len:ident
    ) => {};
}

macro_rules! define_vm_ops {
    ($(
        $variant:ident { $($field:ident : $kind:ident $(($($arg:tt)*))?),* $(,)? }
    ),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum VmOp {
            $(
                $variant { $($field: vm_operand_ty!($kind $(($($arg)*))?)),* },
            )*
        }

        const _: () = assert!(std::mem::size_of::<VmOp>() <= 16);

        pub(crate) fn remap_branch_targets(op: &mut VmOp, pc_map: &[u32]) {
            match op {
                $(
                    VmOp::$variant { $($field),* } => {
                        $(remap_operand!($kind $(($($arg)*))?, $field, pc_map);)*
                    }
                )*
            }
        }

        #[cfg(any(debug_assertions, test))]
        pub(crate) fn verify_vm_ops(
            vm_ops: &[VmOp],
            logical_pcs: Option<&[u32]>,
            registers: usize,
            constants_len: usize,
            captures_len: usize,
            instructions_len: usize,
        ) -> Result<(), String> {
            let logical_len = logical_pcs.map_or(instructions_len, |pcs| pcs.len());
            if vm_ops.len() != logical_len {
                return Err(format!(
                    "vm_ops length ({}) does not match logical_pcs length ({logical_len})",
                    vm_ops.len()
                ));
            }
            let vm_ops_len = vm_ops.len();
            for (vm_pc, op) in vm_ops.iter().enumerate() {
                let logical_pc = logical_pcs.map_or(vm_pc as u32, |pcs| pcs[vm_pc]);
                if (logical_pc as usize) >= instructions_len {
                    return Err(format!(
                        "vm_op {vm_pc}: logical_pc {logical_pc} >= instructions_len {instructions_len}"
                    ));
                }
                match op {
                    $(
                        VmOp::$variant { $($field),* } => {
                            $(
                                let _ = $field;
                                verify_operand!(
                                    $kind $(($($arg)*))?,
                                    $field,
                                    vm_pc,
                                    stringify!($variant),
                                    vm_ops_len,
                                    logical_pc,
                                    registers,
                                    constants_len,
                                    captures_len
                                );
                            )*
                        }
                    )*
                }
            }
            Ok(())
        }
    };
}

define_vm_ops! {
    LoadNil { out: reg },
    LoadTrue { out: reg },
    LoadFixnum { out: reg, value: imm_i64 },
    LoadChar { out: reg, ch: char },
    Constant { out: reg, idx: constant },
    Copy { out: reg, src: reg },
    Clear { start: reg_start(len), len: reg_len },
    Jump { pc: branch },
    Unless { test: reg, pc: branch },
    IfPresent { reg: reg, pc: branch },
    UnlessFixnum { reg: reg, pc: branch },
    UnlessFixnumInRange { reg: reg, low: imm_i32, high: imm_i32, pc: branch },
    TryFixnumArithmetic {
        op: fixnum_arith,
        symbol: constant,
        out: reg,
        start: reg_start(len),
        len: reg_len,
    },
    SetValues1 { src: fixed_reg_start(1) },
    SetValues { start: reg_start(len), len: reg_len },
    BindValues1 { out: fixed_reg_start(1) },
    BindValues { start: reg_start(len), len: reg_len },
    OptionalArgument { index: arg_index, out: reg },
    KeywordArgument { index: arg_index, out: reg },
    ReleaseArguments {},
    ReadCell { out: reg, sym: constant, function: bool },
    WriteCell { sym: constant, src: reg },
    BindSpecial { symbol: constant, value: reg },
    Unbind { count: binding_count },
    BoxLocal { reg: reg },
    Closure { out: reg, code: constant, start: reg_start(len), len: reg_len },
    ReadBoxLocal { out: reg, cell: reg },
    Capture { out: reg, capture: capture },
    ReadBoxCapture { out: reg, capture: capture },
    WriteBoxLocal { cell: reg, src: reg },
    WriteBoxCapture { capture: capture, src: reg },
    CallNamed0 { out: reg, sym: constant },
    CallNamed1 { out: reg, sym: constant, arg0: reg },
    CallNamed2 { out: reg, sym: constant, arg_start: fixed_reg_start(2) },
    CallNamed3 { out: reg, sym: constant, arg_start: fixed_reg_start(3) },
    TailCallNamed { sym: constant, start: reg_start(argc), argc: reg_len },
    TailCallValue { callee: reg, start: reg_start(argc), argc: reg_len },
    TailCallCanonical { pc: canonical_pc },
    CmpBranch {
        kind: cmp_kind,
        a: fixed_reg_start(2),
        sym: constant16,
        out: reg,
        clear_start: reg_start(clear_len),
        clear_len: reg_len,
        target: branch,
    },
    NullBranch {
        reg: reg,
        sym: constant16,
        out: reg,
        clear_start: reg_start(clear_len),
        clear_len: reg_len,
        target: branch,
    },
    ArithFixnumGuarded { kind: arith_kind, out: reg, first: fixed_reg_start(2), sym: constant },
    AddFixnumImm { out: reg, src: reg, literal: reg, imm: imm_i32, sym: constant },
    SubFixnumImm { out: reg, src: reg, literal: reg, imm: imm_i32, sym: constant },
    MulFixnumImm { out: reg, src: reg, literal: reg, imm: imm_i32, sym: constant },
    CarFast { out: reg, src: reg, sym: constant },
    CdrFast { out: reg, src: reg, sym: constant },
    ConsFast { out: reg, first: fixed_reg_start(2), sym: constant },
    ClearBackedge { start: reg_start(len), len: reg_len, target: branch },
    Slow { pc: canonical_pc },
}

#[cfg(all(test, not(feature = "vm-no-spec")))]
mod tests {
    use super::*;
    use crate::Runtime;
    use crate::fasl::CompiledFileLoader;
    use crate::heap::Word;
    use crate::loader::{FileContents, LoadFile};
    use zkcl_bytecode::{Arity, BytecodeFunction, Callee, CompiledFile, Immediate, LiteralNode};

    fn load_test_file(runtime: &Runtime, file: CompiledFile) {
        let bytes = file.encode().unwrap();
        let file = CompiledFile::decode(&bytes).unwrap();
        crate::vm::load_host(
            runtime,
            LoadFile {
                contents: FileContents::Compiled(Box::new(CompiledFileLoader::new(
                    runtime, file, None,
                ))),
                pathname: runtime.root_slot(Word::NIL),
                truename: runtime.root_slot(Word::NIL),
                print: false,
            },
        )
        .unwrap();
    }

    #[test]
    fn lowering_uses_compact_instructions_only_when_the_operands_fit() {
        let last_compact = usize::from(u16::MAX);
        let wide = last_compact + 1;
        let instructions = [
            Instruction::SetValues(last_compact..last_compact),
            Instruction::SetValues(last_compact..wide),
            Instruction::SetValues(last_compact..wide + 1),
            Instruction::BindValues(0..1),
            Instruction::BindValues(1..4),
            Instruction::SetValues(0..wide),
            Instruction::BindValues(wide..wide + 1),
            Instruction::OptionalArgument(0, wide),
            Instruction::KeywordArgument(0, wide),
            Instruction::OptionalArgument(3, 0),
            Instruction::KeywordArgument(4, 1),
            Instruction::ReleaseArguments,
            Instruction::BindSpecial {
                symbol: 0,
                value: last_compact,
            },
            Instruction::BindSpecial {
                symbol: 0,
                value: wide,
            },
            Instruction::Unbind(last_compact),
            Instruction::Unbind(wide),
        ];
        let code = lower_code(&instructions, wide + 1, 0, &[None], &[], &[], &[], false);
        assert_eq!(
            &*code.vm_ops,
            &[
                VmOp::SetValues {
                    start: u16::MAX,
                    len: 0,
                },
                VmOp::SetValues1 { src: u16::MAX },
                VmOp::SetValues {
                    start: u16::MAX,
                    len: 2,
                },
                VmOp::BindValues1 { out: 0 },
                VmOp::BindValues { start: 1, len: 3 },
                VmOp::Slow { pc: 5 },
                VmOp::Slow { pc: 6 },
                VmOp::Slow { pc: 7 },
                VmOp::Slow { pc: 8 },
                VmOp::OptionalArgument { index: 3, out: 0 },
                VmOp::KeywordArgument { index: 4, out: 1 },
                VmOp::ReleaseArguments {},
                VmOp::BindSpecial {
                    symbol: 0,
                    value: u16::MAX
                },
                VmOp::Slow { pc: 13 },
                VmOp::Unbind { count: u16::MAX },
                VmOp::Slow { pc: 15 },
            ]
        );

        #[cfg(target_pointer_width = "64")]
        {
            let wide_index = u32::MAX as usize + 1;
            let instructions = [
                Instruction::OptionalArgument(wide_index, 0),
                Instruction::KeywordArgument(wide_index, 1),
                Instruction::BindSpecial {
                    symbol: wide_index,
                    value: 0,
                },
            ];
            let code = lower_code(&instructions, 2, 0, &[], &[], &[], &[], false);
            assert_eq!(
                &*code.vm_ops,
                &[
                    VmOp::Slow { pc: 0 },
                    VmOp::Slow { pc: 1 },
                    VmOp::Slow { pc: 2 }
                ],
            );
        }
    }

    #[test]
    fn fasl_fusion_preserves_registers_read_after_throw() {
        let file = CompiledFile {
            target: crate::CompilationTarget::native(),
            source_files: Vec::new(),
            nodes: vec![
                LiteralNode::InternedSymbol {
                    package: "KEYWORD".into(),
                    name: "FUSION-TEST".into(),
                },
                LiteralNode::InternedSymbol {
                    package: "COMMON-LISP".into(),
                    name: "+".into(),
                },
                LiteralNode::InternedSymbol {
                    package: "COMMON-LISP-USER".into(),
                    name: "*FUSION-TEST*".into(),
                },
            ],
            entries: vec![BytecodeFunction {
                name: None,
                source_file: None,
                debug: Default::default(),
                arity: Arity::ZERO,
                registers: 4,
                instructions: vec![
                    Instruction::Constant(0, 0),
                    Instruction::EnterCatch {
                        tag: 0,
                        resume: 7,
                        clear: 3,
                    },
                    Instruction::Load(1, Immediate::Fixnum(40)),
                    Instruction::Load(2, Immediate::Fixnum(2)),
                    Instruction::Call {
                        callee: Callee::Named(1),
                        out: Some(3),
                        args: 1..3,
                        at: 0,
                        associate: false,
                        notinline: false,
                    },
                    Instruction::Throw { tag: 0, at: 0 },
                    Instruction::LeaveTarget,
                    Instruction::WriteCell { symbol: 2, src: 2 },
                ],
                constants: vec![0, 1, 2],
                lambda_list: None,
                captures: vec![],
                notinline: vec![],
                vop_calls: Vec::new(),
                handlers: vec![],
                restarts: vec![],
                keywords: vec![],
                jump_tables: vec![],
                facts: Default::default(),
            }],
        };
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        load_test_file(&runtime, file);

        assert_eq!(
            runtime.eval("*fusion-test*").unwrap()[0].as_fixnum(),
            Some(2)
        );
    }

    #[test]
    fn fasl_single_result_calls_discard_previous_multiple_values() {
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        let symbol = |package: &str, name: &str| LiteralNode::InternedSymbol {
            package: package.into(),
            name: name.into(),
        };
        let code = |registers, instructions, constants| BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers,
            instructions,
            constants,
            lambda_list: None,
            captures: vec![],
            notinline: vec![],
            vop_calls: Vec::new(),
            handlers: vec![],
            restarts: vec![],
            keywords: vec![],
            jump_tables: vec![],
            facts: Default::default(),
        };
        for (name, primitive, args, branch) in [
            ("NULL", CanonicalSymbol::Null, 1..2, true),
            ("<", CanonicalSymbol::Lt, 1..3, true),
            ("+", CanonicalSymbol::Add, 1..3, false),
            ("/", CanonicalSymbol::Div, 1..3, false),
            ("CAR", CanonicalSymbol::Car, 1..2, false),
            ("CONS", CanonicalSymbol::Cons, 1..3, false),
        ] {
            for direct in [false, true] {
                let first = match primitive {
                    CanonicalSymbol::Car => Immediate::Nil,
                    CanonicalSymbol::Div => Immediate::Fixnum(4),
                    _ => Immediate::Fixnum(1),
                };
                let mut instructions = vec![
                    Instruction::Load(0, Immediate::Fixnum(42)),
                    Instruction::SetValues(0..1),
                    Instruction::Load(1, first),
                    Instruction::Load(2, Immediate::Fixnum(2)),
                ];
                let callee = if direct {
                    Callee::Named(0)
                } else {
                    instructions.push(Instruction::ReadCell {
                        out: 5,
                        symbol: 0,
                        function: true,
                        at: 0,
                    });
                    Callee::Value(5)
                };
                let call_pc = instructions.len();
                instructions.push(Instruction::Call {
                    callee,
                    out: Some(3),
                    args: args.clone(),
                    at: 0,
                    associate: false,
                    notinline: false,
                });
                if branch {
                    instructions.push(Instruction::Unless(3, instructions.len() + 1));
                }
                instructions.extend([Instruction::BindValues(4..5), Instruction::SetValues(4..5)]);

                let lowered = lower_code(
                    &instructions,
                    6,
                    0,
                    &[Some(primitive)],
                    &[],
                    &[],
                    &[],
                    false,
                );
                let vm_pc = lowered
                    .pc_mapping
                    .as_ref()
                    .map_or(call_pc, |map| map.vm_by_canonical[call_pc] as usize);
                let op = lowered.vm_ops[vm_pc];
                assert!(matches!(
                    (direct, primitive, op),
                    (false, _, VmOp::Slow { .. })
                        | (true, CanonicalSymbol::Null, VmOp::NullBranch { .. })
                        | (true, CanonicalSymbol::Lt, VmOp::CmpBranch { .. })
                        | (true, CanonicalSymbol::Add, VmOp::AddFixnumImm { .. })
                        | (true, CanonicalSymbol::Div, VmOp::CallNamed2 { .. })
                        | (true, CanonicalSymbol::Car, VmOp::CarFast { .. })
                        | (true, CanonicalSymbol::Cons, VmOp::ConsFast { .. })
                ));
                let file = CompiledFile {
                    target: crate::CompilationTarget::native(),
                    source_files: Vec::new(),
                    nodes: vec![
                        symbol("COMMON-LISP", name),
                        symbol("COMMON-LISP-USER", "*FUSION-TEST-FUNCTION*"),
                        LiteralNode::Function(code(6, instructions, vec![0])),
                    ],
                    entries: vec![code(
                        1,
                        vec![
                            Instruction::Constant(0, 0),
                            Instruction::WriteCell { symbol: 1, src: 0 },
                        ],
                        vec![2, 1],
                    )],
                };
                load_test_file(&runtime, file);
                let function = runtime.eval("*fusion-test-function*").unwrap();
                for invocation in 1..=2 {
                    let result = runtime.call(&function[0], &[]).unwrap();
                    assert_eq!(result.len(), 1);
                    assert!(
                        result[0].is_nil(),
                        "{name}, direct={direct}, call {invocation}"
                    );
                }
            }
        }
    }

    #[test]
    fn immediate_load_releases_the_previous_root_before_the_arithmetic_call() {
        use crate::symbol::Name;
        use crate::vm::{ConstantTemplate, FunctionTemplate};

        let rt = Runtime::start().unwrap();
        rt.load_str(
            "(defvar *literal-watch*)
             (defun make-literal-root ()
               (let ((object (list :old)))
                 (setq *literal-watch* (zk:make-weak-pointer object))
                 object))",
        )
        .unwrap();
        let (make_root, at) = crate::read::read_symbol_name(&rt, "MAKE-LITERAL-ROOT").unwrap();
        let function = FunctionTemplate {
            name: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 3,
            constants: vec![
                ConstantTemplate::Symbol(make_root, at),
                ConstantTemplate::Symbol(Name::common_lisp("+"), 0),
            ],
            instructions: vec![
                Instruction::Call {
                    callee: Callee::Named(0),
                    out: Some(1),
                    args: 0..0,
                    at: 0,
                    associate: false,
                    notinline: false,
                },
                Instruction::Load(0, Immediate::Fixnum(40)),
                Instruction::Load(1, Immediate::Fixnum(2)),
                Instruction::Call {
                    callee: Callee::Named(1),
                    out: Some(2),
                    args: 0..2,
                    at: 0,
                    associate: false,
                    notinline: false,
                },
                Instruction::SetValues(1..3),
            ],
            lambda_list: None,
            captures: vec![],
            notinline: vec![],
            vop_calls: Vec::new(),
            handlers: vec![],
            restarts: vec![],
            keywords: vec![],
            jump_tables: vec![],
            facts: Default::default(),
        }
        .instantiate(&rt)
        .unwrap();
        let function = rt.root(function);
        for _ in 0..2 {
            let values = rt.call(&function, &[]).unwrap();
            assert_eq!(values[0].as_fixnum(), Some(2));
            assert_eq!(values[1].as_fixnum(), Some(42));
        }
        let replacement = rt
            .eval(
                "(lambda (a b) (declare (ignore a b))
               (zk:gc) (zk:weak-pointer-value *literal-watch*))",
            )
            .unwrap();
        let symbol = rt.find_named_symbol(&Name::common_lisp("+")).unwrap();
        rt.heap
            .set_symbol_cell(symbol, true, replacement[0].slot.get());
        let values = rt.call(&function, &[]).unwrap();
        assert_eq!(values[0].as_fixnum(), Some(2));
        assert!(values[1].is_nil());
    }

    #[test]
    fn deopt_at_fused_arithmetic_call_preserves_an_aliased_destination() {
        let mut instructions = [
            Instruction::Load(1, Immediate::Fixnum(1)),
            Instruction::Call {
                callee: Callee::Named(0),
                args: 0..2,
                out: Some(1),
                at: 0,
                associate: false,
                notinline: false,
            },
            Instruction::Clear(1..2),
        ];
        let lower = |instructions: &[Instruction]| {
            lower_code(
                instructions,
                2,
                0,
                &[Some(CanonicalSymbol::Add)],
                &[],
                &[],
                &[],
                false,
            )
        };
        let fused = lower(&instructions);
        assert!(matches!(
            fused.vm_ops[0],
            VmOp::AddFixnumImm { out: 1, src: 0, .. }
        ));
        let mapping = fused.pc_mapping.unwrap();
        assert_eq!(&*mapping.canonical_by_vm, &[1, 2]);
        assert_eq!(&*mapping.vm_by_canonical, &[0, 0, 1, 2]);

        instructions[2] = Instruction::SetValues(1..2);
        let fused = lower(&instructions);
        assert!(matches!(
            fused.vm_ops[0],
            VmOp::AddFixnumImm {
                literal: 1,
                out: 1,
                ..
            }
        ));
    }

    #[test]
    fn clear_backedge_fusion_preserves_jump_entries() {
        let lower = |instructions: &[Instruction]| {
            lower_code(instructions, 2, 0, &[], &[], &[], &[], false)
        };
        let mut instructions = [
            Instruction::Unless(0, 3),
            Instruction::Clear(1..2),
            Instruction::Jump(0),
        ];
        let fused = lower(&instructions);
        assert_eq!(
            &*fused.vm_ops,
            &[
                VmOp::Unless { test: 0, pc: 2 },
                VmOp::ClearBackedge {
                    start: 1,
                    len: 1,
                    target: 0
                },
            ],
        );
        let mapping = fused.pc_mapping.unwrap();
        assert_eq!(&*mapping.vm_by_canonical, &[0, 1, 1, 2]);

        instructions[0] = Instruction::Unless(0, 2);
        let unfused = lower(&instructions);
        assert!(matches!(
            &*unfused.vm_ops,
            [_, VmOp::Clear { .. }, VmOp::Jump { pc: 0 }]
        ));
        assert!(unfused.pc_mapping.is_none());
    }

    #[test]
    fn branch_fusion_preserves_a_clear_of_the_test_register() {
        for (prim, args) in [
            (CanonicalSymbol::Null, 0..1),
            (CanonicalSymbol::EqNum, 0..2),
        ] {
            let mut instructions = [
                Instruction::Call {
                    callee: Callee::Named(0),
                    out: Some(2),
                    args,
                    at: 0,
                    associate: false,
                    notinline: false,
                },
                Instruction::Clear(2..3),
                Instruction::Unless(2, 4),
                Instruction::Load(2, Immediate::True),
            ];
            let lower =
                |code: &[Instruction]| lower_code(code, 3, 0, &[Some(prim)], &[], &[], &[], false);
            let unfused = lower(&instructions);
            assert!(matches!(
                &*unfused.vm_ops,
                [
                    _,
                    VmOp::Clear { start: 2, len: 1 },
                    VmOp::Unless { test: 2, pc: 4 },
                    _
                ]
            ));

            instructions[1] = Instruction::Clear(0..1);
            let fused = lower(&instructions);
            assert!(matches!(
                &*fused.vm_ops,
                [
                    VmOp::NullBranch { target: 2, .. } | VmOp::CmpBranch { target: 2, .. },
                    _
                ]
            ));
        }
    }

    #[test]
    fn branch_fusion_retains_the_clear_for_register_reads_after_the_branch() {
        let mut instructions = vec![
            Instruction::Call {
                callee: Callee::Named(0),
                out: Some(64),
                args: 0..1,
                at: 0,
                associate: false,
                notinline: false,
            },
            Instruction::Clear(0..64),
            Instruction::Unless(64, 4),
        ];
        instructions.resize(260, Instruction::Copy(65, 65));
        let lower = |instructions: &[Instruction]| {
            lower_code(
                instructions,
                66,
                0,
                &[Some(CanonicalSymbol::Null)],
                &[],
                &[],
                &[],
                false,
            )
        };
        for live_register in [
            Instruction::SetValues(17..18),
            Instruction::SetValues(20..64),
            Instruction::Jump(0),
        ] {
            instructions.push(live_register);
            assert!(matches!(
                lower(&instructions).vm_ops[0],
                VmOp::NullBranch {
                    out: 64,
                    clear_start: 0,
                    clear_len: 64,
                    ..
                }
            ));
            instructions.pop();
        }
        assert!(matches!(
            lower(&instructions).vm_ops[0],
            VmOp::NullBranch { .. }
        ));
    }

    #[test]
    fn fused_branches_preserve_call_registers_until_the_callee_returns() {
        use crate::symbol::Name;
        use crate::vm::{ConstantTemplate, FunctionTemplate};

        let runtime = Runtime::start().unwrap();
        runtime
            .load_str(
                "(defvar *old-out-watch* nil)
                 (defvar *operand-watch* nil)
                 (defvar *result-watch* nil)
                 (defun branch-old-out ()
                   (let ((word (list :old)))
                     (setf *old-out-watch* (zk:make-weak-pointer word)) word))
                 (defun branch-operand ()
                   (let ((word (list :argument)))
                     (setf *operand-watch* (zk:make-weak-pointer word)) word))
                 (defun branch-watch-state ()
                   (zk:gc)
                   (list (if (zk:weak-pointer-value *old-out-watch*) t nil)
                         (if (zk:weak-pointer-value *operand-watch*) t nil)
                         (if (and *result-watch* (zk:weak-pointer-value *result-watch*)) t nil)))",
            )
            .unwrap();
        let replacement = runtime
            .eval(
                "(lambda (a &optional b)
                   (declare (ignore a b))
                   (unless (equal (branch-watch-state) '(t t nil)) (error \"lost live call roots\"))
                   (let ((word (list :result)))
                     (setf *result-watch* (zk:make-weak-pointer word))
                     (values word nil)))",
            )
            .unwrap()
            .pop()
            .unwrap();
        let call = |symbol, out, args, notinline| Instruction::Call {
            callee: Callee::Named(symbol),
            out: Some(out),
            args,
            at: 0,
            associate: false,
            notinline,
        };
        let user_symbol = |name| {
            let (symbol, at) = crate::read::read_symbol_name(&runtime, name).unwrap();
            ConstantTemplate::Symbol(symbol, at)
        };
        let warm_caller = |name, arity, matching, notinline| {
            let code = FunctionTemplate {
                name: None,
                debug: Default::default(),
                instructions: vec![
                    call(0, 2, 0..0, false),
                    call(1, 0, 0..0, false),
                    if matching {
                        Instruction::Copy(1, 0)
                    } else {
                        Instruction::Load(1, Immediate::Nil)
                    },
                    call(2, 2, 0..arity, notinline),
                    Instruction::Clear(0..arity),
                    Instruction::Unless(2, 8),
                    Instruction::Load(4, Immediate::True),
                    Instruction::Jump(9),
                    Instruction::Load(4, Immediate::Nil),
                    call(3, 3, 0..0, false),
                    Instruction::SetValues(3..5),
                ],
                constants: vec![
                    user_symbol("BRANCH-OLD-OUT"),
                    user_symbol("BRANCH-OPERAND"),
                    ConstantTemplate::Symbol(Name::common_lisp(name), 0),
                    user_symbol("BRANCH-WATCH-STATE"),
                ],
                lambda_list: None,
                registers: 5,
                arity: Arity::ZERO,
                captures: vec![],
                notinline: vec![],
                vop_calls: Vec::new(),
                handlers: vec![],
                restarts: vec![],
                keywords: vec![],
                jump_tables: vec![],
                facts: Default::default(),
            };
            let function = runtime.root(code.instantiate(&runtime).unwrap());
            for _ in 0..2 {
                let result = runtime.call(&function, &[]).unwrap();
                assert_eq!(runtime.repr(&result[0]).unwrap(), "(NIL NIL NIL)");
                assert_eq!(result[1].is_t(), matching);
            }
            function
        };
        for (name, arity, matching) in [("NULL", 1, false), ("EQ", 2, false), ("EQ", 2, true)] {
            let generic = warm_caller(name, arity, matching, true);
            let fused = warm_caller(name, arity, matching, false);
            let symbol = runtime.root(runtime.find_named_symbol(&Name::common_lisp(name)).unwrap());
            runtime
                .heap
                .set_symbol_cell(symbol.slot.get(), true, replacement.slot.get());
            for caller in [&generic, &fused] {
                let result = runtime.call(caller, &[]).unwrap();
                assert_eq!(runtime.repr(&result[0]).unwrap(), "(NIL NIL T)");
                assert!(result[1].is_t());
                runtime.eval("(setq *result-watch* nil)").unwrap();
            }
            runtime.heap.set_symbol_cell(
                symbol.slot.get(),
                true,
                crate::vm::Primitive::named(name).unwrap(),
            );
        }
    }
}
