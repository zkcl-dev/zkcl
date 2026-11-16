use rustc_hash::{FxHashMap, FxHashSet};

use super::NativeFrameLayout;
use crate::{BasicBlock, BlockId, SsaFunction, SsaOp, Terminator, ValueId};

#[derive(Clone, Copy)]
pub enum TailCallee {
    Named(usize),
    Value(ValueId),
}

#[derive(Clone, Copy)]
pub struct TailCall<'a> {
    pub callee: TailCallee,
    /// `None` consumes the current multiple values as arguments.
    pub args: Option<&'a [ValueId]>,
    pub at: usize,
}

impl<'a> TailCall<'a> {
    pub fn fixed_arguments(self, arity: zkcl_bytecode::Arity) -> Option<&'a [ValueId]> {
        self.args
            .filter(|args| arity.required == args.len() && arity.maximum == Some(args.len()))
    }
}

/// Select a remaining call after SSA optimization has specialized primitives.
/// Active control scopes must have been exited before this call.
pub fn tail_call<'a>(
    function: &SsaFunction,
    frame: &NativeFrameLayout,
    block: &'a BasicBlock,
) -> Option<TailCall<'a>> {
    let call = &block.insts.last()?.op;
    let (callee, args, at) = match *call {
        SsaOp::CallNamed {
            symbol,
            ref args,
            at,
            has_result: false,
            associate: false,
            ..
        } => (TailCallee::Named(symbol), Some(args.as_slice()), at),
        SsaOp::CallValue {
            callee,
            ref args,
            at,
            has_result: false,
            associate: false,
            ..
        } => (TailCallee::Value(callee), Some(args.as_slice()), at),
        SsaOp::CallValues { callee, at } => (TailCallee::Value(callee), None, at),
        _ => return None,
    };
    if !block.unwind_succs.is_empty()
        || !returns_dynamic_values(block, |target| {
            frame
                .block_index(target)
                .is_some_and(|index| is_trivial_return(&function.blocks[index]))
        })
    {
        return None;
    }
    // Retained debug activations must stay present while their callees execute.
    if frame.has_debug_points() {
        return None;
    }
    Some(TailCall { callee, args, at })
}

fn is_empty_continuation(block: &BasicBlock) -> bool {
    block.params.is_empty() && block.insts.is_empty() && block.unwind_succs.is_empty()
}

fn is_trivial_return(block: &BasicBlock) -> bool {
    is_empty_continuation(block) && matches!(block.terminator, Terminator::ReturnDynamicValues)
}

/// Blocks that lead to a dynamic return through empty, argument-free continuations.
pub(crate) fn empty_return_blocks(blocks: &[BasicBlock]) -> FxHashSet<BlockId> {
    let mut jumps_to = FxHashMap::<BlockId, Vec<BlockId>>::default();
    let mut pending = Vec::new();
    for block in blocks.iter().filter(|block| is_empty_continuation(block)) {
        match &block.terminator {
            Terminator::ReturnDynamicValues => pending.push(block.id),
            Terminator::Jump(target) if target.args.is_empty() => {
                jumps_to.entry(target.block).or_default().push(block.id);
            }
            _ => {}
        }
    }

    // Each empty jump has one successor. Walking from returns classifies shared
    // suffixes once and never visits cycles that cannot reach a return.
    let mut returning = FxHashSet::default();
    while let Some(block) = pending.pop() {
        returning.insert(block);
        if let Some(predecessors) = jumps_to.remove(&block) {
            pending.extend(predecessors);
        }
    }
    returning
}

pub(crate) fn returns_dynamic_values(
    block: &BasicBlock,
    returns_without_work: impl FnOnce(BlockId) -> bool,
) -> bool {
    match &block.terminator {
        Terminator::ReturnDynamicValues => true,
        Terminator::Jump(target) if target.args.is_empty() => returns_without_work(target.block),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{FxHashSet, empty_return_blocks};
    use crate::{
        BasicBlock, BlockId, BranchTarget, DeoptState, ScopeId, SsaInst, SsaOp, Terminator, TypeTag,
    };

    #[test]
    fn empty_return_paths_stop_at_checkpoints_unwind_edges_and_cycles() {
        let block = |id, next: Option<u32>| BasicBlock {
            id: BlockId(id),
            params: Vec::new(),
            insts: Vec::new(),
            terminator: next.map_or(Terminator::ReturnDynamicValues, |next| {
                Terminator::Jump(BranchTarget {
                    block: BlockId(next),
                    args: Vec::new(),
                })
            }),
            terminator_pc: id as usize,
            entry_scope: ScopeId::ROOT,
            unwind_succs: Vec::new(),
        };
        let mut blocks = [
            block(0, Some(1)),
            block(1, Some(2)),
            block(2, None),
            block(3, Some(4)),
            block(4, Some(3)),
            block(5, Some(1)),
            block(6, Some(5)),
            block(7, Some(1)),
            block(8, Some(7)),
        ];
        blocks[5].insts.push(SsaInst {
            id: None,
            op: SsaOp::Checkpoint { values: None },
            ty: TypeTag::Top,
            pc: Some(5),
            deopt: Some(DeoptState {
                pc: 5,
                registers_to_restore: Vec::new(),
            }),
        });
        blocks[7].unwind_succs.push(BlockId(0));

        assert_eq!(
            empty_return_blocks(&blocks),
            FxHashSet::from_iter([BlockId(0), BlockId(1), BlockId(2)])
        );
    }
}
