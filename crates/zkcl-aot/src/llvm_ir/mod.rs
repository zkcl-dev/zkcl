//! LLVM AOT module construction, optimization and object emission.

mod function;

use std::sync::Once;

use inkwell::attributes::{Attribute, AttributeLoc};
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::types::{
    BasicType, BasicTypeEnum, FloatType, IntType, PointerType, StructType, VectorType,
};
use inkwell::values::FunctionValue;
use inkwell::{AddressSpace, OptimizationLevel};
use target_lexicon::{Architecture, Environment, Vendor};
use zkcl_bytecode::{BytecodeFunction, CompiledFile};
use zkcl_opt::FloatFormat;
use zkcl_opt::native::{
    ClassifiedFunction, LLVM_FEATURES, RuntimeAbiFunction, RuntimeAbiType, native_frame_layout,
};

use crate::ssa::{optimize_native_entry, register_native_entry};
use crate::{AotError, AotOptLevel, AotRegistrationEntry, AotTargetConfig, fasl_symbol_name};

type Result<T> = std::result::Result<T, AotError>;

pub(super) struct EmittedObject {
    pub bytes: Vec<u8>,
    pub registrations: Vec<AotRegistrationEntry>,
    pub ir: String,
}

pub(super) fn emit_object(
    file: &CompiledFile,
    functions: &[&BytecodeFunction],
    symbol_prefix: &str,
    fasl: &[u8],
    config: &AotTargetConfig,
    compilation_target: &zkcl_target::CompilationTarget,
) -> Result<EmittedObject> {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        Target::initialize_all(&InitializationConfig {
            disassembler: false,
            ..Default::default()
        })
    });
    let (level, pipeline) = match config.opt_level {
        AotOptLevel::None => (OptimizationLevel::None, "default<O0>"),
        AotOptLevel::Speed => (OptimizationLevel::Default, "default<O2>"),
        AotOptLevel::Aggressive => (OptimizationLevel::Aggressive, "default<O3>"),
        AotOptLevel::Size => (OptimizationLevel::Default, "default<Os>"),
    };
    // Apple arm64 ABIs include Advanced SIMD in the baseline ISA.
    let aarch64_asimd = matches!(config.target.architecture, Architecture::Aarch64(_))
        && config.target.vendor == Vendor::Apple
        && config.target.operating_system.is_like_darwin();
    let target = &config.target;
    let machine_target = compilation_target
        .machine()
        .expect("object compilation has a machine ABI");
    let environment = machine_target.environment();
    let target_name = match crate::object_file::macho_deployment(target, environment)? {
        Some((os, _, version)) => {
            // target-lexicon spells the simulator environment "sim"; LLVM uses "simulator".
            let environment = match environment {
                Environment::Sim => "-simulator",
                Environment::Macabi => "-macabi",
                _ => "",
            };
            format!(
                "{}-{}-{os}{}.{}.{}{environment}",
                target.architecture, target.vendor, version.major, version.minor, version.patch
            )
        }
        None => target.to_string(),
    };
    let triple = TargetTriple::create(&target_name);
    let triple = TargetMachine::normalize_triple(&triple);
    let unsupported = |reason| AotError::UnsupportedTarget {
        target: config.target.to_string(),
        reason,
    };
    let target = Target::from_triple(&triple).map_err(|error| unsupported(error.to_string()))?;
    let machine = target
        .create_target_machine(
            &triple,
            "generic",
            "",
            level,
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| unsupported("could not create LLVM target machine".into()))?;
    let context = Context::create();
    let fasl_symbol = fasl_symbol_name(&config.module_id);
    let module = context.create_module(&fasl_symbol);
    let data = machine.get_target_data();
    module.set_triple(&triple);
    module.set_data_layout(&data.get_data_layout());
    let word = context.ptr_sized_int_type(&data, None);
    if word.get_bit_width() != u32::from(machine_target.pointer_width().bits()) {
        return Err(unsupported(
            "LLVM pointer width differs from the FASL target".into(),
        ));
    }
    let ir = ModuleCodegen::new(&context, module, word, aarch64_asimd);
    let bytes = context.const_string(fasl, false);
    let global = ir.module.add_global(bytes.get_type(), None, &fasl_symbol);
    global.set_initializer(&bytes);
    global.set_constant(true);
    global.set_alignment(8);
    let mut registrations = Vec::new();
    for (index, code) in functions.iter().enumerate() {
        let (entry_index, ssa) = optimize_native_entry(file, code, index)?;
        let classified = ClassifiedFunction::new(&ssa);
        if !classified.is_supported(LLVM_FEATURES) {
            continue;
        }
        let frame = native_frame_layout(&ssa);
        let entry = register_native_entry(symbol_prefix, entry_index, code, frame.root_slots());
        function::emit(
            &ir,
            &ssa,
            &classified,
            &frame,
            &entry.symbol_name,
            &code.vop_calls,
        )?;
        registrations.push(entry);
    }
    ir.module.verify().map_err(AotError::Llvm)?;
    let text = ir.module.print_to_string().to_string();
    ir.module
        .run_passes(pipeline, &machine, PassBuilderOptions::create())
        .map_err(AotError::Llvm)?;
    let object = machine
        .write_to_memory_buffer(&ir.module, FileType::Object)
        .map_err(AotError::Llvm)?;
    Ok(EmittedObject {
        bytes: object.as_slice().to_vec(),
        registrations,
        ir: text,
    })
}

struct ModuleCodegen<'ctx> {
    context: &'ctx Context,
    module: Module<'ctx>,
    word: IntType<'ctx>,
    ptr: PointerType<'ctx>,
    result: StructType<'ctx>,
    zeroext: Attribute,
    aarch64_asimd: bool,
    runtime_functions: Vec<FunctionValue<'ctx>>,
}

impl<'ctx> ModuleCodegen<'ctx> {
    /// Declare the word layout, the return record and every runtime symbol.
    fn new(
        context: &'ctx Context,
        module: Module<'ctx>,
        word: IntType<'ctx>,
        aarch64_asimd: bool,
    ) -> Self {
        let ptr = context.ptr_type(AddressSpace::default());
        let result = context.opaque_struct_type("JitReturn");
        result.set_body(
            &[
                context.i8_type().into(),
                word.into(),
                word.into(),
                word.into(),
            ],
            false,
        );
        let mut codegen = Self {
            context,
            module,
            word,
            ptr,
            result,
            zeroext: context.create_enum_attribute(Attribute::get_named_enum_kind_id("zeroext"), 0),
            aarch64_asimd,
            runtime_functions: Vec::new(),
        };
        codegen.runtime_functions = codegen.declare_runtime_functions();
        codegen
    }

    fn abi_type(&self, ty: RuntimeAbiType) -> BasicTypeEnum<'ctx> {
        match ty {
            RuntimeAbiType::Pointer => self.ptr.into(),
            RuntimeAbiType::Word => self.word.into(),
            RuntimeAbiType::Bool => self.context.bool_type().into(),
            RuntimeAbiType::I64 => self.context.i64_type().into(),
        }
    }

    fn float_type(&self, format: FloatFormat) -> FloatType<'ctx> {
        match format {
            FloatFormat::F32 => self.context.f32_type(),
            FloatFormat::F64 => self.context.f64_type(),
        }
    }

    fn pack_type(&self) -> VectorType<'ctx> {
        self.context.i32_type().vec_type(4)
    }

    fn add_bool_attributes(
        &self,
        function: RuntimeAbiFunction,
        mut add: impl FnMut(AttributeLoc, Attribute),
    ) {
        // Rust's C ABI requires bool parameters and results to be zero-extended.
        for (index, ty) in function.parameter_types().iter().enumerate() {
            if matches!(ty, RuntimeAbiType::Bool) {
                add(AttributeLoc::Param(index as u32), self.zeroext);
            }
        }
        if matches!(function.return_type(), Some(RuntimeAbiType::Bool)) {
            add(AttributeLoc::Return, self.zeroext);
        }
    }

    fn declare_runtime_functions(&self) -> Vec<FunctionValue<'ctx>> {
        RuntimeAbiFunction::ALL
            .map(|runtime_function| {
                let params: Vec<_> = runtime_function
                    .parameter_types()
                    .iter()
                    .map(|&ty| self.abi_type(ty).into())
                    .collect();
                let signature = match runtime_function.return_type() {
                    Some(ty) => self.abi_type(ty).fn_type(&params, false),
                    None => self.context.void_type().fn_type(&params, false),
                };
                let declaration =
                    self.module
                        .add_function(runtime_function.symbol_name(), signature, None);
                self.add_bool_attributes(runtime_function, |location, attribute| {
                    declaration.add_attribute(location, attribute);
                });
                declaration
            })
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use target_lexicon::Triple;
    use zkcl_opt::native::{NativeOp, WordOp};
    use zkcl_opt::operation_samples::lowering_cases;
    use zkcl_opt::{
        DeoptRegisterValue, DeoptState, SsaFunction, SsaInst, SsaOp, TypeTag, ValueRepr,
    };

    fn host_target_machine() -> (TargetTriple, TargetMachine) {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            Target::initialize_all(&InitializationConfig {
                asm_parser: false,
                disassembler: false,
                ..Default::default()
            })
        });
        let triple =
            TargetMachine::normalize_triple(&TargetTriple::create(&Triple::host().to_string()));
        let target = Target::from_triple(&triple).expect("host triple is a known LLVM target");
        let machine = target
            .create_target_machine(
                &triple,
                "generic",
                "",
                OptimizationLevel::None,
                RelocMode::PIC,
                CodeModel::Default,
            )
            .expect("host target machine");
        (triple, machine)
    }

    /// Give the classification samples valid operands and recovery state for emission.
    fn well_formed(mut function: SsaFunction) -> SsaFunction {
        function.fixnum_bounds = zkcl_target::CompilationTarget::native().fixnum_bounds();
        let tagged = function.entry_params[0];
        let integer = function.fresh_value();
        let unsigned = function.fresh_value();
        let boolean = function.fresh_value();
        let f32_value = function.fresh_value();
        let f64_value = function.fresh_value();
        let pack = function.fresh_value();
        let float_value = |format| match format {
            FloatFormat::F32 => f32_value,
            FloatFormat::F64 => f64_value,
        };
        let block = &mut function.blocks[0];
        let inst = &mut block.insts[0];
        let input = match NativeOp::for_op(&inst.op) {
            NativeOp::Word(WordOp::Select(..) | WordOp::Not(_)) => boolean,
            NativeOp::Word(
                WordOp::BoxFixnum(_)
                | WordOp::I64(..)
                | WordOp::CompareI64(..)
                | WordOp::BitwiseI64(..),
            ) => integer,
            _ => match inst.op {
                SsaOp::WordCount { .. } => unsigned,
                SsaOp::AddI64Checked(..) | SsaOp::SubI64Checked(..) | SsaOp::MulI64Checked(..) => {
                    integer
                }
                SsaOp::BoxFloat { format, .. } | SsaOp::FloatArithChecked { format, .. } => {
                    float_value(format)
                }
                SsaOp::BoxI32x4(_) | SsaOp::I32x4Binary { .. } => pack,
                _ => tagged,
            },
        };
        inst.op.rewrite_inputs(|_| input);
        if let SsaOp::WriteFloatVector { format, value, .. } = &mut inst.op {
            *value = float_value(*format);
        }
        let recovery = inst.deopt.get_or_insert(DeoptState {
            pc: 0,
            registers_to_restore: Vec::new(),
        });
        for (_, value) in &mut recovery.registers_to_restore {
            match value {
                DeoptRegisterValue::UnboxedI64(operand) => *operand = integer,
                DeoptRegisterValue::UnboxedFloat { format, value } => *value = float_value(*format),
                DeoptRegisterValue::UnboxedI32x4 { value, .. } => *value = pack,
                _ => {}
            }
        }
        if let [(_, restored @ DeoptRegisterValue::UnboxedI32x4 { .. })] =
            recovery.registers_to_restore.as_slice()
        {
            recovery.registers_to_restore.push((1, *restored));
        }
        let typed_operand = |value, ty, op| SsaInst {
            id: Some(value),
            op,
            ty,
            pc: Some(0),
            deopt: Some(DeoptState {
                pc: 0,
                registers_to_restore: Vec::new(),
            }),
        };
        block.insts.splice(
            ..0,
            [
                SsaInst {
                    id: Some(integer),
                    op: SsaOp::ConstI64(1),
                    ty: TypeTag::Fixnum,
                    pc: None,
                    deopt: None,
                },
                SsaInst {
                    id: Some(unsigned),
                    op: SsaOp::ConstU64(u64::MAX),
                    ty: TypeTag::Any,
                    pc: None,
                    deopt: None,
                },
                SsaInst {
                    id: Some(boolean),
                    op: SsaOp::IsNil(tagged),
                    ty: TypeTag::Any,
                    pc: None,
                    deopt: None,
                },
                typed_operand(
                    f32_value,
                    TypeTag::Float,
                    SsaOp::UnboxFloatGuard {
                        format: FloatFormat::F32,
                        value: tagged,
                    },
                ),
                typed_operand(
                    f64_value,
                    TypeTag::Float,
                    SsaOp::UnboxFloatGuard {
                        format: FloatFormat::F64,
                        value: tagged,
                    },
                ),
                typed_operand(pack, TypeTag::I32x4, SsaOp::UnboxI32x4Guard(tagged)),
            ],
        );
        function
    }

    #[test]
    fn every_supported_operation_has_an_llvm_emitter() {
        let (triple, machine) = host_target_machine();
        let context = Context::create();
        let module = context.create_module("lowering_coverage");
        let data = machine.get_target_data();
        module.set_triple(&triple);
        module.set_data_layout(&data.get_data_layout());
        let word = context.ptr_sized_int_type(&data, None);
        let ir = ModuleCodegen::new(&context, module, word, false);
        let mut lowered_operations = 0;
        // Function entry parameters arrive through the tagged Lisp ABI.
        let cases =
            lowering_cases().filter(|case| case.blocks[0].params[0].repr == ValueRepr::TaggedWord);
        for (index, case) in cases.enumerate() {
            let operation = case.blocks[0].insts[0].op.clone();
            let case = well_formed(case);
            let classified = ClassifiedFunction::new(&case);
            if !classified.is_supported(LLVM_FEATURES) {
                continue;
            }
            let layout = native_frame_layout(&case);
            let symbol = format!("lowering_{index}");
            match function::emit(&ir, &case, &classified, &layout, &symbol, &[]) {
                Ok(()) => lowered_operations += 1,
                Err(AotError::LlvmLowering { reason, .. })
                    if reason.contains("unsupported SSA operation") =>
                {
                    panic!("supported without an LLVM emitter: {operation:?} ({reason})")
                }
                Err(error) => {
                    panic!("supported operation failed to lower: {operation:?}: {error}")
                }
            }
        }
        ir.module.verify().expect("the lowering module verifies");
        let text = ir.module.print_to_string().to_string();
        for lowering in [
            "call zeroext i1 @zkcl_rt_is_primitive_binding(",
            "call i64 @zkcl_rt_read_float_vector(",
            "call zeroext i1 @zkcl_rt_write_float_vector(",
            "fadd double",
            "add <4 x i32>",
            "call void @zkcl_rt_deopt_float_to_vm(",
            "call void @zkcl_rt_deopt_simd_to_vm(",
        ] {
            assert!(text.contains(lowering), "missing LLVM lowering {lowering}");
        }
        assert!(
            lowered_operations > 0,
            "no sample operations are supported for LLVM"
        );
    }
}
