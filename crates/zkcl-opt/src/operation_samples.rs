//! Representative SSA operations for backend lowering and dispatch tests.
//!
//! Every [`SsaOp`](crate::SsaOp) variant needs an entry in [`ops!`]'s exhaustive
//! match, so adding a variant forces adding a sample. The values are only
//! used to test lowering coverage; they are not executable bytecode.

use zkcl_bytecode::native_abi::I32x4Operation;
use zkcl_bytecode::{Arity, FixnumArithmetic, Immediate, fixnum_bounds_for_bits};

use crate::native::{
    CRANELIFT_FEATURES, LLVM_FEATURES, NativeFeature, NativeOp, WASM_FEATURES, supports_lowering,
};
use crate::{
    BasicBlock, BitwiseOp, BlockId, BlockParam, CallSiteProfile, CellTarget, CmpOp, DeoptRegisterValue,
    DeoptState, FloatArithOp, FloatFormat, GuardedPrimitive, ScopeId, ScopeTree,
    SsaFunction, SsaInst, SsaOp, Terminator, TypeTag, ValueId, ValueRepr,
};

macro_rules! ops {
    (@pattern $name:ident ($($arg:expr),*)) => { SsaOp::$name(..) };
    (@pattern $name:ident { $($field:ident: $value:expr),* }) => { SsaOp::$name { .. } };
    (@pattern $name:ident) => { SsaOp::$name };
    ($($name:ident $(($($arg:expr),*))? $({$($field:ident: $value:expr),*})?),* $(,)?) => {{
        let cases = [$(SsaOp::$name $(($($arg),*))? $({$($field: $value),*})?),*];
        for op in &cases {
            match op { $(ops!(@pattern $name $(($($arg),*))? $({$($field: $value),*})?) => (),)* }
        }
        cases
    }};
}

/// One instance of every `SsaOp` variant.
pub fn sample_operations() -> Vec<SsaOp> {
    let v = ValueId(0);
    let b = BlockId(0);
    let fmt = FloatFormat::F64;
    let cell = CellTarget::Local(v);
    let arithmetic = FixnumArithmetic::Add;
    ops![
        ConstWord(Immediate::Nil),
        ConstI64(0),
        ConstU64(u64::MAX),
        ConstBool(false),
        LoadConst(0),
        Copy(v),
        SelectBool {
            cond: v,
            when_true: Immediate::True,
            when_false: Immediate::Nil
        },
        BoxFixnum(v),
        TryFixnumArithmetic {
            op: arithmetic,
            symbol: 0,
            lhs: v,
            rhs: v
        },
        GuardTransformBinding {
            symbol: 0,
            binding: 0
        },
        UnboxU64Guard(v),
        WordCount { op: zkcl_bytecode::WordCountOp::Popcount, input: v },
        UnboxFixnumGuard(v),
        BoxFloat {
            format: fmt,
            value: v
        },
        UnboxFloatGuard {
            format: fmt,
            value: v
        },
        BoxI32x4(v),
        UnboxI32x4Guard(v),
        I32x4Binary {
            op: I32x4Operation::Add,
            lhs: v,
            rhs: v
        },
        GuardPrimitiveBinding {
            symbol: 0,
            primitive: GuardedPrimitive::Typep
        },
        FloatArithChecked {
            format: fmt,
            op: FloatArithOp::Add,
            lhs: v,
            rhs: v
        },
        ReadFloatVector {
            symbol: 0,
            format: fmt,
            array: v,
            index: v
        },
        WriteFloatVector {
            symbol: 0,
            format: fmt,
            setter: v,
            array: v,
            index: v,
            value: v
        },
        AddI64Checked(v, v),
        SubI64Checked(v, v),
        MulI64Checked(v, v),
        AddI64Unchecked(v, v),
        SubI64Unchecked(v, v),
        MulI64Unchecked(v, v),
        BitwiseI64 { op: BitwiseOp::And, lhs: v, rhs: v },
        CmpI64 {
            op: CmpOp::Eq,
            lhs: v,
            rhs: v
        },
        IsPresent(v),
        IsNil(v),
        IsFixnum(v),
        IsFixnumInRange {
            value: v,
            low: 0,
            high: 1
        },
        NotBool(v),
        SetValues(vec![v]),
        ReadValue(0),
        ExtractValue {
            source: v,
            index: 0
        },
        SaveValues,
        AppendValues,
        RestoreValues,
        OptionalArgument(0),
        KeywordArgument(0),
        RestArguments { start: 0, at: 0 },
        CheckKeywords { start: 0, table: 0 },
        DestructureNext {
            cursor: v,
            required: false,
            macro_call: false,
            at: 0
        },
        DestructureEnd {
            cursor: v,
            macro_call: false,
            at: 0
        },
        DestructureKeywords {
            cursor: v,
            table: 0,
            width: 0,
            macro_call: false,
            at: 0
        },
        ReleaseArguments,
        ReadCell {
            symbol: 0,
            function: false,
            at: 0
        },
        WriteCell { symbol: 0, src: v },
        DefineFunction {
            symbol: 0,
            definition: v,
            is_macro: false,
            at: 0
        },
        ProclaimSpecial { symbol: 0, at: 0 },
        ProclaimInline {
            symbol: 0,
            setf: false,
            notinline: false
        },
        SetVariableDocumentation { symbol: 0, doc: 0 },
        IsSymbolBound { symbol: 0 },
        BindSpecial {
            symbol: 0,
            value: v
        },
        BindProgv {
            symbols: v,
            values: v,
            at: 0
        },
        Unbind(0),
        BoxLocal { value: v, at: 0 },
        ReadBox(cell),
        WriteBox { cell: cell, src: v },
        ReadCapture(0),
        Closure {
            code: 0,
            cells: vec![],
            at: 0
        },
        CallNamed {
            symbol: 0,
            args: vec![],
            has_result: true,
            at: 0,
            associate: false,
            notinline: false,
            profile: CallSiteProfile::Unprofiled
        },
        CallValue {
            callee: v,
            args: vec![],
            has_result: true,
            at: 0,
            associate: false,
            notinline: false,
            profile: CallSiteProfile::Unprofiled
        },
        CallValues { callee: v, at: 0 },
        DebugPoint {
            pc: 0,
            registers: vec![]
        },
        DebugRegister(0),
        Checkpoint { values: None },
        StoreUnwindRegister { reg: 0, value: v },
        ExitToken(0),
        EnterBlock {
            token: v,
            resume: b,
            clear: 0
        },
        EnterCatch {
            tag: v,
            resume: b,
            clear: 0
        },
        EnterTagbody {
            token: v,
            jumps: 0,
            targets: vec![],
            clear: 0
        },
        LeaveTarget,
        EnterProtect {
            cleanup: b,
            clear: 0
        },
        BeginCleanup,
        EndCleanup,
        EnterHandlers {
            table: 0,
            targets: vec![],
            function_targets: false
        },
        LeaveHandlers,
        EnterRestarts { table: 0, at: 0 },
        LeaveRestarts,
        AssociateRestarts {
            condition: v,
            list: v,
            at: 0
        },
        LeaveRestartAssociation,
        HandlerCondition,
    ]
    .into()
}

/// One single-block function per operation sample, crossed with every parameter
/// representation and every recovery-register kind that target feature checks distinguish.
pub fn lowering_cases() -> impl Iterator<Item = SsaFunction> {
    let v = ValueId(0);
    let mut ops = sample_operations();
    for mut op in ops.clone() {
        match &mut op {
            SsaOp::CallNamed { associate, .. } | SsaOp::CallValue { associate, .. } => {
                *associate = true;
            }
            SsaOp::EnterHandlers {
                function_targets, ..
            } => *function_targets = true,
            _ => continue,
        }
        ops.push(op);
    }
    let restored = [
        None,
        Some(DeoptRegisterValue::TaggedWord(v)),
        Some(DeoptRegisterValue::UnboxedI64(v)),
        Some(DeoptRegisterValue::ConstImmediate(Immediate::Nil)),
        Some(DeoptRegisterValue::UnboxedFloat {
            value: v,
            format: FloatFormat::F32,
        }),
        Some(DeoptRegisterValue::UnboxedFloat {
            value: v,
            format: FloatFormat::F64,
        }),
        Some(DeoptRegisterValue::UnboxedI32x4 {
            identity: v,
            value: v,
        }),
    ];
    let reprs = [
        ValueRepr::TaggedWord,
        ValueRepr::Bool,
        ValueRepr::UnboxedI64,
        ValueRepr::UnboxedF32,
        ValueRepr::UnboxedF64,
        ValueRepr::UnboxedI32x4,
    ];
    ops.into_iter().flat_map(move |op| {
        reprs.into_iter().flat_map(move |repr| {
            let op = op.clone();
            restored.into_iter().map(move |reg| {
                let mut insts = vec![SsaInst {
                    id: None,
                    op: op.clone(),
                    ty: TypeTag::Any,
                    pc: Some(0),
                    deopt: reg.map(|value| DeoptState {
                        pc: 0,
                        registers_to_restore: vec![(0, value)],
                    }),
                }];
                // A handler condition is read from the slot its registration owns;
                // without the registration the frame layout has no slot to read, so
                // this case only becomes structurally complete with it present.
                if matches!(op, SsaOp::HandlerCondition) {
                    insts.push(SsaInst {
                        id: None,
                        op: SsaOp::EnterHandlers {
                            table: 0,
                            targets: vec![BlockId(0)],
                            function_targets: false,
                        },
                        ty: TypeTag::Any,
                        pc: Some(0),
                        deopt: None,
                    });
                }
                SsaFunction {
                    fixnum_bounds: fixnum_bounds_for_bits(32).unwrap(),
                    arity: Arity {
                        required: 1,
                        maximum: Some(1),
                    },
                    constant_count: 1,
                    entry: BlockId(0),
                    entry_params: vec![v],
                    blocks: vec![BasicBlock {
                        id: BlockId(0),
                        params: vec![BlockParam {
                            value: v,
                            ty: TypeTag::Any,
                            repr,
                            origin_reg: Some(0),
                        }],
                        insts,
                        terminator: Terminator::ReturnOne(v),
                        terminator_pc: 1,
                        entry_scope: ScopeId::ROOT,
                        unwind_succs: vec![],
                    }],
                    scopes: ScopeTree::new(),
                    pc_to_block: Default::default(),
                    next_value_id: 1,
                }
            })
        })
    })
}

/// Support result for each sample operation under one backend's feature set, in
/// sample order. Written as `op<TAB>supported` rows for TSV comparison.
pub fn lowering_support_rows(features: &[NativeFeature]) -> Vec<(&'static str, bool)> {
    sample_operations()
        .into_iter()
        .map(|op| {
            let function = single(op.clone());
            (op_name(&op), supports_lowering(&function, features))
        })
        .collect()
}

/// Names of every sample operation, used by the backend matrices.
pub fn sample_operation_names() -> Vec<&'static str> {
    sample_operations().iter().map(op_name).collect()
}

/// One row of the lowering matrix: operation name and whether each backend's
/// feature set supports it.
pub fn lowering_support_matrix() -> Vec<(&'static str, bool, bool, bool)> {
    let cl = lowering_support_rows(CRANELIFT_FEATURES);
    let llvm = lowering_support_rows(LLVM_FEATURES);
    let wasm = lowering_support_rows(WASM_FEATURES);
    cl.into_iter()
        .zip(llvm)
        .zip(wasm)
        .map(|(((name, cl), (llvm_name, llvm)), (wasm_name, wasm))| {
            assert_eq!(name, llvm_name);
            assert_eq!(name, wasm_name);
            (name, cl, llvm, wasm)
        })
        .collect()
}

fn single(op: SsaOp) -> SsaFunction {
    lowering_cases()
        .find(|function| {
            let inst = &function.blocks[0].insts[0];
            inst.op == op
                && inst.deopt.as_ref().is_some_and(|state| {
                    matches!(
                        state.registers_to_restore.as_slice(),
                        [(_, DeoptRegisterValue::TaggedWord(_))]
                    )
                })
        })
        .expect("every sample operation has a case")
}

fn op_name(op: &SsaOp) -> &'static str {
    match op {
        SsaOp::ConstWord(_) => "ConstWord",
        SsaOp::ConstI64(_) => "ConstI64",
        SsaOp::ConstU64(_) => "ConstU64",
        SsaOp::ConstBool(_) => "ConstBool",
        SsaOp::LoadConst(_) => "LoadConst",
        SsaOp::Copy(_) => "Copy",
        SsaOp::SelectBool { .. } => "SelectBool",
        SsaOp::BoxFixnum(_) => "BoxFixnum",
        SsaOp::TryFixnumArithmetic { .. } => "TryFixnumArithmetic",
        SsaOp::UnboxFixnumGuard(_) => "UnboxFixnumGuard",
        SsaOp::BoxFloat { .. } => "BoxFloat",
        SsaOp::UnboxFloatGuard { .. } => "UnboxFloatGuard",
        SsaOp::BoxI32x4(_) => "BoxI32x4",
        SsaOp::UnboxI32x4Guard(_) => "UnboxI32x4Guard",
        SsaOp::I32x4Binary { .. } => "I32x4Binary",
        SsaOp::GuardPrimitiveBinding { .. } => "GuardPrimitiveBinding",
        SsaOp::GuardTransformBinding { .. } => "GuardTransformBinding",
        SsaOp::UnboxU64Guard(_) => "UnboxU64Guard",
        SsaOp::WordCount { .. } => "WordCount",
        SsaOp::FloatArithChecked { .. } => "FloatArithChecked",
        SsaOp::ReadFloatVector { .. } => "ReadFloatVector",
        SsaOp::WriteFloatVector { .. } => "WriteFloatVector",
        SsaOp::AddI64Checked(_, _) => "AddI64Checked",
        SsaOp::SubI64Checked(_, _) => "SubI64Checked",
        SsaOp::MulI64Checked(_, _) => "MulI64Checked",
        SsaOp::AddI64Unchecked(_, _) => "AddI64Unchecked",
        SsaOp::SubI64Unchecked(_, _) => "SubI64Unchecked",
        SsaOp::MulI64Unchecked(_, _) => "MulI64Unchecked",
        SsaOp::BitwiseI64 { .. } => "BitwiseI64",
        SsaOp::CmpI64 { .. } => "CmpI64",
        SsaOp::IsPresent(_) => "IsPresent",
        SsaOp::IsNil(_) => "IsNil",
        SsaOp::IsFixnum(_) => "IsFixnum",
        SsaOp::IsFixnumInRange { .. } => "IsFixnumInRange",
        SsaOp::NotBool(_) => "NotBool",
        SsaOp::SetValues(_) => "SetValues",
        SsaOp::ReadValue(_) => "ReadValue",
        SsaOp::ExtractValue { .. } => "ExtractValue",
        SsaOp::SaveValues => "SaveValues",
        SsaOp::AppendValues => "AppendValues",
        SsaOp::RestoreValues => "RestoreValues",
        SsaOp::OptionalArgument(_) => "OptionalArgument",
        SsaOp::KeywordArgument(_) => "KeywordArgument",
        SsaOp::RestArguments { .. } => "RestArguments",
        SsaOp::CheckKeywords { .. } => "CheckKeywords",
        SsaOp::DestructureNext { .. } => "DestructureNext",
        SsaOp::DestructureEnd { .. } => "DestructureEnd",
        SsaOp::DestructureKeywords { .. } => "DestructureKeywords",
        SsaOp::ReleaseArguments => "ReleaseArguments",
        SsaOp::ReadCell { .. } => "ReadCell",
        SsaOp::WriteCell { .. } => "WriteCell",
        SsaOp::DefineFunction { .. } => "DefineFunction",
        SsaOp::ProclaimSpecial { .. } => "ProclaimSpecial",
        SsaOp::ProclaimInline { .. } => "ProclaimInline",
        SsaOp::SetVariableDocumentation { .. } => "SetVariableDocumentation",
        SsaOp::IsSymbolBound { .. } => "IsSymbolBound",
        SsaOp::BindSpecial { .. } => "BindSpecial",
        SsaOp::BindProgv { .. } => "BindProgv",
        SsaOp::Unbind(_) => "Unbind",
        SsaOp::BoxLocal { .. } => "BoxLocal",
        SsaOp::ReadBox(_) => "ReadBox",
        SsaOp::WriteBox { .. } => "WriteBox",
        SsaOp::ReadCapture(_) => "ReadCapture",
        SsaOp::Closure { .. } => "Closure",
        SsaOp::CallNamed { .. } => "CallNamed",
        SsaOp::CallValue { .. } => "CallValue",
        SsaOp::CallValues { .. } => "CallValues",
        SsaOp::DebugPoint { .. } => "DebugPoint",
        SsaOp::DebugRegister(_) => "DebugRegister",
        SsaOp::Checkpoint { .. } => "Checkpoint",
        SsaOp::StoreUnwindRegister { .. } => "StoreUnwindRegister",
        SsaOp::ExitToken(_) => "ExitToken",
        SsaOp::EnterBlock { .. } => "EnterBlock",
        SsaOp::EnterCatch { .. } => "EnterCatch",
        SsaOp::EnterTagbody { .. } => "EnterTagbody",
        SsaOp::LeaveTarget => "LeaveTarget",
        SsaOp::EnterProtect { .. } => "EnterProtect",
        SsaOp::BeginCleanup => "BeginCleanup",
        SsaOp::EndCleanup => "EndCleanup",
        SsaOp::EnterHandlers { .. } => "EnterHandlers",
        SsaOp::LeaveHandlers => "LeaveHandlers",
        SsaOp::EnterRestarts { .. } => "EnterRestarts",
        SsaOp::LeaveRestarts => "LeaveRestarts",
        SsaOp::AssociateRestarts { .. } => "AssociateRestarts",
        SsaOp::LeaveRestartAssociation => "LeaveRestartAssociation",
        SsaOp::HandlerCondition => "HandlerCondition",
    }
}

/// Supported operations whose `NativeOp` variant is a target-specific encoding.
pub fn supported_target_ops(features: &[NativeFeature]) -> Vec<SsaOp> {
    sample_operations()
        .into_iter()
        .filter(|op| {
            supports_lowering(&single(op.clone()), features)
                && matches!(NativeOp::for_op(op), NativeOp::Target { .. })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{CRANELIFT_FEATURES, LLVM_FEATURES, WASM_FEATURES};

    #[test]
    fn supported_operations_never_map_to_the_unsupported_branch() {
        for features in [CRANELIFT_FEATURES, LLVM_FEATURES, WASM_FEATURES] {
            for op in sample_operations() {
                if supports_lowering(&single(op.clone()), features) {
                    assert!(
                        !matches!(NativeOp::for_op(&op), NativeOp::Unsupported),
                        "{features:?} supports an unsupported operation: {op:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn lowering_support_table() {
        let mut table = String::from("op\tCL\tLLVM\tWasm\n");
        for (name, cl, llvm, wasm) in lowering_support_matrix() {
            table.push_str(&format!("{name}\t{cl}\t{llvm}\t{wasm}\n"));
        }
        // The comparison script asks for a file; printing alone is lost in CI logs.
        if let Ok(path) = std::env::var("ZKCL_LOWERING_TSV") {
            std::fs::write(path, &table).expect("lowering matrix is writable");
        }
        print!("{table}");
    }

    #[test]
    fn every_sample_operation_has_a_buildable_lowering_case() {
        for op in sample_operations() {
            let _ = single(op);
        }
    }
}
