//! SSA optimization pipeline and analysis entry points.

mod calls;
mod cfg;
mod dom;
mod fixnum;
mod float;
mod flow;
mod gvn_licm;
mod liveness;
mod memory;
mod propagate;
mod rewrite;
mod roots;
mod simd;
mod structured_cfg;

#[cfg(feature = "integer-equality-saturation")]
pub(crate) use cfg::visit_control_targets;
pub use cfg::{UnwindTable, build_unwind_table, compute_rpo};
pub(crate) use cfg::{eliminate_trivial_block_params, rpo_with_indices, unwind_table_with_indices};
pub use liveness::{Liveness, instruction_uses, restored_value};
pub(crate) use roots::root_layout_with_indices;
pub use roots::{RootLayout, RootSlot, SafepointRoots, compute_root_layout};
pub use structured_cfg::{
    StructuredCfg, StructuredJump, StructuredNode, StructuredRegion, StructuredTerminator,
    beyond_relooper, beyond_relooper_graph, structure_cfg, structure_graph,
};

use crate::ir::{SpecializationInputs, SsaFunction};
use calls::{share_binding_guards, specialize_named_calls};
use cfg::simplify_cfg;
use dom::{DomTree, LoopInfo};
use fixnum::{eliminate_redundant_fixnum_checks, hoist_loop_guards, unbox_loop_params};
use flow::run_sparse_flow;
use gvn_licm::{gvn, licm};
use propagate::propagate_constants_and_copies;
use rewrite::eliminate_dead_code;

/// Options controlling middle-end optimization aggressiveness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptimizeConfig {
    /// Whether speculative guards may fall back to bytecode when a runtime
    /// binding or value fails a check.
    ///
    /// - JIT mode enables speculative deopt based on feedback / guarded assumptions.
    /// - AOT mode disables speculative deopt so native binaries never require bytecode deopt frames
    ///   for untyped polymorphic operations.
    pub allow_speculative_deopt: bool,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            allow_speculative_deopt: true,
        }
    }
}

/// Run the full middle-end optimization pipeline on `func` using default JIT options.
pub fn optimize(func: &mut SsaFunction, specialization: &dyn SpecializationInputs) {
    optimize_with_options(func, specialization, OptimizeConfig::default());
}

/// Optimize JIT SSA, including bounded algebraic search over proven fixnum regions.
#[cfg(feature = "integer-equality-saturation")]
pub fn optimize_with_integer_equality_saturation(
    func: &mut SsaFunction,
    specialization: &dyn SpecializationInputs,
) {
    optimize(func, specialization);
    if crate::integer_eqsat::optimize(func) {
        if propagate_constants_and_copies(func) {
            simplify_cfg(func);
        }
        eliminate_dead_code(func);
    }
}

/// Run the middle-end optimization pipeline on `func` with explicit [`OptimizeConfig`].
pub fn optimize_with_options(
    func: &mut SsaFunction,
    specialization: &dyn SpecializationInputs,
    options: OptimizeConfig,
) {
    const MAX_ROUNDS: usize = 6;
    simplify_cfg(func);
    let mut reached_fixed_point = false;
    for _ in 0..MAX_ROUNDS {
        let mut changed = false;
        changed |= propagate_constants_and_copies(func);
        let block_map = func.block_index_map();
        let dom = DomTree::compute(func, &block_map);
        changed |= run_sparse_flow(func, specialization, &dom);
        if options.allow_speculative_deopt {
            changed |= specialize_named_calls(func, specialization);
        }
        if changed {
            changed |= propagate_constants_and_copies(func);
        }
        if options.allow_speculative_deopt {
            changed |= unbox_loop_params(func);
            changed |= hoist_loop_guards(func);
        }
        let block_map = func.block_index_map();
        let dom = DomTree::compute(func, &block_map);
        let loops = LoopInfo::compute(func, &block_map, &dom);
        changed |= gvn(func, &dom);
        changed |= licm(func, &dom, &loops);
        changed |= memory::late_load_elimination(func, &dom);
        changed |= memory::scalar_replace_local_boxes(func, &dom);
        changed |= eliminate_redundant_fixnum_checks(func);
        changed |= eliminate_dead_code(func);
        changed |= simplify_cfg(func);
        if !changed {
            reached_fixed_point = true;
            break;
        }
    }
    let mut unboxing_changed = false;
    if options.allow_speculative_deopt {
        unboxing_changed |= float::keep_unboxed(func, specialization);
        unboxing_changed |= simd::keep_i32x4_unboxed(func);
    }
    if unboxing_changed {
        eliminate_dead_code(func);
    }
    if unboxing_changed || !reached_fixed_point {
        let block_map = func.block_index_map();
        let dom = DomTree::compute(func, &block_map);
        run_sparse_flow(func, specialization, &dom);
    }
    if options.allow_speculative_deopt {
        share_binding_guards(func);
    }
}
