use zkcl_bytecode::BytecodeBody;

use super::validate::validate_code_bounds;
use super::{BytecodeCfg, BytecodeLiftError, BytecodeLifter, lift_validated_bytecode};
use crate::{
    BasicBlock, BlockId, BranchTarget, ScopeId, SpecializationInputs, SsaFunction, SsaOp,
    Terminator,
};

/// Lift a native continuation from a canonical loop header. Its SSA entry
/// parameters identify the incoming bytecode registers through `origin_reg`.
/// Returns `None` for headers that require activation state besides the
/// canonical registers, closure and current multiple-value buffer.
pub fn lift_bytecode_for_osr(
    body: BytecodeBody<'_>,
    specialization: &dyn SpecializationInputs,
    pc: usize,
) -> Result<Option<SsaFunction>, BytecodeLiftError> {
    if pc >= body.instructions.len()
        || specialization.native_debug()
        || body.arity.maximum != Some(body.arity.required)
    {
        return Ok(None);
    }
    let fixnum_bounds = specialization.fixnum_bounds();
    validate_code_bounds(&body, fixnum_bounds)?;
    let cfg = BytecodeCfg::analyze(&body)?;
    let Some(&header) = cfg.pc_to_block.get(&pc) else {
        return Ok(None);
    };
    if cfg.blocks[header.0 as usize].entry_scope != ScopeId::ROOT
        || !cfg.blocks.iter().any(|block| {
            block.end_pc > pc
                && block.succs.contains(&header)
                && body.instructions[block.end_pc - 1].flow().target() == Some(pc)
        })
    {
        return Ok(None);
    }
    Ok(Some(lift_validated_bytecode(
        &body,
        specialization,
        fixnum_bounds,
        cfg,
        Some(header),
    )))
}

impl BytecodeLifter<'_> {
    pub(super) fn lift_osr_entry(&self, header: BlockId, next_value_id: &mut u32) -> BasicBlock {
        let pc = self.cfg.blocks[header.0 as usize].start_pc;
        let mut insts = Vec::new();
        let mut emit = crate::ir::InstBuilder {
            next_value_id,
            insts: &mut insts,
            pc: None,
        };
        let mut params = self.block_params[header.0 as usize].clone();
        for param in &mut params {
            let reg = param
                .origin_reg
                .expect("OSR entry receives canonical registers");
            param.value = emit.fresh_value();
            if self.unwind_registers.contains(&reg) {
                emit.effect(SsaOp::StoreUnwindRegister {
                    reg,
                    value: param.value,
                });
            }
        }
        let args = params.iter().map(|param| param.value).collect();
        BasicBlock {
            id: self.entry,
            params,
            insts,
            terminator: Terminator::Jump(BranchTarget {
                block: header,
                args,
            }),
            terminator_pc: pc,
            entry_scope: ScopeId::ROOT,
            unwind_succs: Vec::new(),
        }
    }
}
