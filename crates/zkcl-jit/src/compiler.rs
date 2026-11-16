use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::{Inst, InstBuilder, SigRef, UserFuncName, Value, types};
use cranelift_codegen::isa::{OwnedTargetIsa, TargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use zkcl_cranelift::{
    LoweringConfig, LoweringError, RuntimeAbiFunction, RuntimeCallEmitter, lower_ssa_function_into,
};
use zkcl_opt::SsaFunction;
use zkcl_runtime::native::jit_runtime_functions;

pub use crate::JitError;

impl From<LoweringError> for JitError {
    fn from(err: LoweringError) -> Self {
        // Preserve the compiler's separate UnsupportedOp and InvalidIr error categories.
        match err {
            LoweringError::UnsupportedOp(msg) => Self::UnsupportedOp(msg),
            other => Self::InvalidIr(other),
        }
    }
}

/// Machine code and its root-slot count for a runtime entry.
#[derive(Clone, Debug)]
pub struct CompiledMachineCode {
    pub bytes: Vec<u8>,
    pub root_slots: usize,
}

/// Build an [`OwnedTargetIsa`] configured for the current host machine.
pub fn host_isa() -> Result<OwnedTargetIsa, JitError> {
    let mut flag_builder = settings::builder();
    flag_builder
        .set("opt_level", "speed")
        .map_err(JitError::CraneliftOptLevel)?;
    let flags = settings::Flags::new(flag_builder);
    let isa_builder =
        cranelift_native::builder().map_err(|e| JitError::UnsupportedTarget(e.to_string()))?;
    isa_builder.finish(flags).map_err(JitError::CraneliftIsa)
}

/// Compile an SSA function for the current host ISA.
pub fn compile_ssa_function_for_host(func: &SsaFunction) -> Result<CompiledMachineCode, JitError> {
    let isa = host_isa()?;
    compile_ssa_function(func, &*isa)
}

struct IndirectRuntimeCallEmitter {
    call_conv: cranelift_codegen::isa::CallConv,
    ptr_ty: cranelift_codegen::ir::Type,
    runtime_function_signatures: [Option<SigRef>; RuntimeAbiFunction::ALL.len()],
}

impl RuntimeCallEmitter for IndirectRuntimeCallEmitter {
    fn emit_runtime_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        runtime_function: RuntimeAbiFunction,
        args: &[Value],
    ) -> Result<Inst, LoweringError> {
        let sig_ref = *self.runtime_function_signatures[runtime_function as usize]
            .get_or_insert_with(|| {
                let sig = zkcl_cranelift::abi::runtime_call_signature(
                    runtime_function,
                    self.ptr_ty,
                    self.call_conv,
                );
                builder.import_signature(sig)
            });
        let addr = jit_runtime_functions().address(runtime_function);
        let fn_ptr = builder.ins().iconst(self.ptr_ty, addr as i64);
        Ok(builder.ins().call_indirect(sig_ref, fn_ptr, args))
    }
}

/// Lower `func` into Cranelift IR and compile it for `isa`.
pub fn compile_ssa_function(
    func: &SsaFunction,
    isa: &dyn TargetIsa,
) -> Result<CompiledMachineCode, JitError> {
    JitCompiler::new().compile(func, isa)
}

pub(crate) struct JitCompiler {
    codegen_context: Context,
    builder_context: FunctionBuilderContext,
}

impl JitCompiler {
    pub(crate) fn new() -> Self {
        Self {
            codegen_context: Context::new(),
            builder_context: FunctionBuilderContext::new(),
        }
    }

    pub(crate) fn compile(
        &mut self,
        func: &SsaFunction,
        isa: &dyn TargetIsa,
    ) -> Result<CompiledMachineCode, JitError> {
        self.compile_entry(func, isa, None)
    }

    pub(crate) fn compile_osr(
        &mut self,
        func: &SsaFunction,
        isa: &dyn TargetIsa,
        register_count: usize,
    ) -> Result<CompiledMachineCode, JitError> {
        self.compile_entry(func, isa, Some(register_count))
    }

    fn compile_entry(
        &mut self,
        func: &SsaFunction,
        isa: &dyn TargetIsa,
        osr_register_count: Option<usize>,
    ) -> Result<CompiledMachineCode, JitError> {
        if isa.pointer_type() != types::I64 {
            return Err(JitError::UnsupportedTarget(format!(
                "zkcl-jit requires 64-bit pointer target, got {:?}",
                isa.pointer_type()
            )));
        }

        let name = UserFuncName::user(0, 0);
        let input = match osr_register_count {
            Some(count) => LoweringConfig::for_osr(func, name, count),
            None => LoweringConfig::new(func, name),
        }
        .ok_or_else(|| {
            let message = if osr_register_count.is_some() {
                "unsupported Cranelift loop entry or operations"
            } else {
                "function contains operations unsupported by Cranelift backend"
            };
            JitError::UnsupportedOp(message.into())
        })?;
        let root_slots = input.root_slots();

        let mut emitter = IndirectRuntimeCallEmitter {
            call_conv: isa.default_call_conv(),
            ptr_ty: isa.pointer_type(),
            runtime_function_signatures: [None; RuntimeAbiFunction::ALL.len()],
        };
        self.codegen_context.clear();
        if let Err(error) = lower_ssa_function_into(
            &input,
            isa,
            &mut self.builder_context,
            &mut emitter,
            &mut self.codegen_context.func,
        ) {
            // Lowering may return before the builder finalizes and clears its state.
            self.builder_context = FunctionBuilderContext::new();
            return Err(error.into());
        }
        let compiled = self
            .codegen_context
            .compile(isa, &mut ControlPlane::default())
            .map_err(|error| JitError::CraneliftCodegen(error.inner))?;
        if !compiled.buffer.relocs().is_empty() {
            return Err(JitError::Codegen(format!(
                "unexpected {} relocations in JIT machine code",
                compiled.buffer.relocs().len()
            )));
        }

        Ok(CompiledMachineCode {
            bytes: compiled.code_buffer().to_vec(),
            root_slots,
        })
    }
}
