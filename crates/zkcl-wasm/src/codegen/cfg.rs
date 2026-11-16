use zkcl_opt::native::NativeFrameLayout;
use zkcl_opt::{SsaFunction, StructuredCfg, StructuredRegion, structure_cfg, structure_graph};
use zkcl_vop::leaf::Block;

pub(super) type Region = StructuredRegion;
pub(super) type ControlFlow = StructuredCfg;

pub(super) fn for_ssa(function: &SsaFunction, frame: &NativeFrameLayout) -> ControlFlow {
    structure_cfg(function, frame)
}

pub(super) fn for_leaf(blocks: &[Block]) -> ControlFlow {
    let order: Vec<_> = (0..blocks.len()).collect();
    structure_graph(
        blocks.len(),
        &[0],
        |index, add_edge| blocks[index].exit.visit_targets(add_edge),
        &order,
    )
}
