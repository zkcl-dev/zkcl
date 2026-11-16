//! Middle-end SSA IR, bytecode lifting, optimization passes, and safepoint root layout.
//!
//! `zkcl-opt` depends on `zkcl-bytecode` without depending on the Lisp runtime. Runtime
//! call-site profiles and constant-pool metadata are supplied by the caller
//! through [`SpecializationInputs`].

#[cfg(feature = "integer-equality-saturation")]
mod integer_eqsat;
#[cfg(feature = "integer-equality-saturation")]
pub use passes::optimize_with_integer_equality_saturation;
pub mod ir;
pub mod lift;
pub mod native;
/// Representative SSA operations shared by backend lowering tests; see the `operation-samples` feature.
#[cfg(feature = "operation-samples")]
pub mod operation_samples;
pub mod passes;

pub use ir::{
    AliasDomain, ArithOp, BarrierEffect, BasicBlock, BlockId, BlockParam, BranchTarget,
    CallSiteProfile, CellTarget, CmpOp, ControlEffect, ControlScopeKind, ControlScopeNode,
    DeoptRegisterValue, DeoptState, FIXNUM_MAX, FIXNUM_MIN, FloatArithOp, FloatFormat,
    GuardedPrimitive, NoSpecializationInputs, NumericOperand, OpEffects, PopcountLowering, ScopeId,
    ScopeTree, SpecializationInputs, SsaFunction, SsaInst, SsaOp, Terminator, TypeTag, UnwindOp,
    ValueId, ValueRepr, is_valid_fixnum,
};
pub use lift::{BytecodeLiftError, FaslSpecializationInputs, VopTarget, lift_bytecode, lift_code};
pub use passes::{
    OptimizeConfig, RootLayout, RootSlot, SafepointRoots, StructuredCfg, StructuredJump,
    StructuredNode, StructuredRegion, StructuredTerminator, UnwindTable, beyond_relooper,
    beyond_relooper_graph, build_unwind_table, compute_root_layout, compute_rpo, optimize,
    optimize_with_options, structure_cfg, structure_graph,
};
