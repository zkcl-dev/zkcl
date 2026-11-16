use crate::backend::{
    CompiledRevision, CraneliftJitBackend, jit_function_from_mapping, worker_count,
};
use crate::compiler::{JitCompiler, compile_ssa_function, host_isa};
use crate::{CompiledMachineCode, JitError};
use rustc_hash::FxHashMap;
use zkcl_opt::{
    CallSiteProfile, FloatFormat, NumericOperand, SpecializationInputs, SsaFunction, lift_bytecode,
    optimize, optimize_with_integer_equality_saturation,
};
use zkcl_runtime::native::{
    JitCompilationRequest, JitFloatFormat, JitFunction, JitNumericOperand, JitSiteKind,
};

impl CraneliftJitBackend {
    /// Start a Cranelift JIT backend with up to two background compiler threads.
    pub fn new() -> Result<Self, JitError> {
        Self::with_integer_equality_saturation(true)
    }

    /// Start a JIT backend with control over the bounded search used by explicit compilation.
    pub fn with_integer_equality_saturation(enabled: bool) -> Result<Self, JitError> {
        Self::start(worker_count(), enabled)
    }

    pub(crate) fn start(
        workers: usize,
        integer_equality_saturation: bool,
    ) -> Result<Self, JitError> {
        Self::start_workers(host_isa()?, workers, integer_equality_saturation, |isa| {
            let mut compiler = JitCompiler::new();
            move |request, saturation, cache, is_pending| {
                let (id, revision) = (request.code_id, request.revision);
                let function = prepare_request(&request, saturation).ok()?;
                if !is_pending() {
                    return None;
                }
                let machine = compiler.compile(&function, &*isa).ok()?;
                if !is_pending() {
                    return None;
                }
                let mapping = cache.allocate(id, revision, &machine.bytes, None).ok()?;
                let function =
                    jit_function_from_mapping(&mapping, machine.root_slots, request.native_debug);
                Some(CompiledRevision { mapping, function })
            }
        })
    }

    /// Synchronously compile a [`JitCompilationRequest`] through
    /// bytecode lifting, SSA optimization, and Cranelift code generation.
    pub fn compile_now(&self, request: &JitCompilationRequest) -> Result<JitFunction, JitError> {
        let function = prepare_request(request, self.integer_equality_saturation)?;
        let machine = JitCompiler::new().compile(&function, &*self.target)?;
        self.install_standalone(
            request.code_id,
            request.revision,
            machine,
            request.native_debug,
        )
    }

    /// Synchronously compile a pre-built or already-optimized [`SsaFunction`] into
    /// executable machine code and register it in this backend's [`crate::CodeCache`].
    pub fn compile_ssa_function(
        &self,
        code_id: usize,
        revision: u64,
        func: &SsaFunction,
    ) -> Result<JitFunction, JitError> {
        let machine = compile_ssa_function(func, &*self.target)?;
        self.install_standalone(code_id, revision, machine, false)
    }

    fn install_standalone(
        &self,
        code_id: usize,
        revision: u64,
        machine: CompiledMachineCode,
        native_debug: bool,
    ) -> Result<JitFunction, JitError> {
        let mapping = self
            .code_cache()
            .insert(code_id, revision, &machine.bytes)?;
        Ok(jit_function_from_mapping(
            &mapping,
            machine.root_slots,
            native_debug,
        ))
    }
}

struct JitSpecializationInputs<'a> {
    native_debug: bool,
    call_site_profiles: FxHashMap<usize, CallSiteProfile>,
    common_lisp_constant_names: &'a [Option<&'static str>],
}

impl<'a> JitSpecializationInputs<'a> {
    fn from_request(request: &'a JitCompilationRequest) -> Self {
        let mut call_site_profiles = FxHashMap::default();
        let numeric_operand = |operand| match operand {
            JitNumericOperand::Fixnum => NumericOperand::Fixnum,
            JitNumericOperand::F32 => NumericOperand::F32,
            JitNumericOperand::F64 => NumericOperand::F64,
        };
        for site_feedback in &request.site_feedback {
            let profile = match site_feedback.kind {
                JitSiteKind::Cold => CallSiteProfile::Unprofiled,
                JitSiteKind::CallPrimitive => CallSiteProfile::Primitive,
                JitSiteKind::KnownFixnumArith => CallSiteProfile::FixnumArithmetic,
                JitSiteKind::KnownFloatArith { lhs, rhs } => CallSiteProfile::FloatArithmetic {
                    lhs: numeric_operand(lhs),
                    rhs: numeric_operand(rhs),
                },
                JitSiteKind::FloatArrayReader { format } => CallSiteProfile::FloatArrayReader {
                    format: float_format(format),
                },
                JitSiteKind::FloatArrayWriter { format } => CallSiteProfile::FloatArrayWriter {
                    format: float_format(format),
                },
                JitSiteKind::SimdI32x4 { operation }
                    if cfg!(all(
                        target_endian = "little",
                        any(target_arch = "aarch64", target_arch = "x86_64")
                    )) =>
                {
                    CallSiteProfile::SimdI32x4 { operation }
                }
                JitSiteKind::SimdI32x4 { .. } => CallSiteProfile::Primitive,
                JitSiteKind::PolymorphicArithmetic => CallSiteProfile::PolymorphicArithmetic,
                JitSiteKind::Megamorphic => CallSiteProfile::Megamorphic,
            };
            call_site_profiles.insert(site_feedback.canonical_pc, profile);
        }
        Self {
            native_debug: request.native_debug,
            call_site_profiles,
            common_lisp_constant_names: &request.cl_const_names,
        }
    }
}

impl SpecializationInputs for JitSpecializationInputs<'_> {
    fn native_debug(&self) -> bool {
        self.native_debug
    }

    fn call_site_profile(&self, pc: usize) -> CallSiteProfile {
        self.call_site_profiles
            .get(&pc)
            .copied()
            .unwrap_or(CallSiteProfile::Unprofiled)
    }

    fn named_symbol(&self, const_idx: usize) -> Option<(&str, &str)> {
        self.common_lisp_constant_names
            .get(const_idx)
            .copied()
            .flatten()
            .map(|name| ("COMMON-LISP", name))
    }
}

fn float_format(format: JitFloatFormat) -> FloatFormat {
    match format {
        JitFloatFormat::F32 => FloatFormat::F32,
        JitFloatFormat::F64 => FloatFormat::F64,
    }
}

pub(crate) fn prepare_request(
    request: &JitCompilationRequest,
    integer_equality_saturation: bool,
) -> Result<SsaFunction, JitError> {
    let specialization = JitSpecializationInputs::from_request(request);
    let mut ssa_function = lift_bytecode(request.bytecode(), &specialization)?;
    if integer_equality_saturation {
        optimize_with_integer_equality_saturation(&mut ssa_function, &specialization);
    } else {
        optimize(&mut ssa_function, &specialization);
    }
    Ok(ssa_function)
}
