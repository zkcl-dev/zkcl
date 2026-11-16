use zkcl_bytecode::{BytecodeFunction, CompiledFile, LiteralNode};

use crate::{
    BytecodeLiftError, OptimizeConfig, PopcountLowering, SpecializationInputs, SsaFunction,
    TypeTag, lift_code, optimize_with_options,
};

/// Supplies literal types and symbol names from the function's FASL constant pool.
pub struct FaslSpecializationInputs<'a> {
    file: &'a CompiledFile,
    code: &'a BytecodeFunction,
    vop_target: Option<VopTarget>,
}

/// Which user POPCOUNT implementations the code generator can lower.
#[derive(Clone, Copy)]
pub enum VopTarget {
    Portable,
    Aarch64Asimd,
}

impl<'a> FaslSpecializationInputs<'a> {
    pub fn new(file: &'a CompiledFile, code: &'a BytecodeFunction) -> Self {
        Self {
            file,
            code,
            vop_target: None,
        }
    }

    pub fn with_vop_target(mut self, target: VopTarget) -> Self {
        self.vop_target = Some(target);
        self
    }

    pub fn optimize(&self) -> Result<SsaFunction, BytecodeLiftError> {
        let mut function = lift_code(self.code, self)?;
        optimize_with_options(
            &mut function,
            self,
            OptimizeConfig {
                allow_speculative_deopt: false,
            },
        );
        Ok(function)
    }

    fn literal(&self, index: usize) -> Option<&LiteralNode> {
        let &node = self.code.constants.get(index)?;
        self.file.nodes.get(node as usize)
    }
}

impl SpecializationInputs for FaslSpecializationInputs<'_> {
    fn native_debug(&self) -> bool {
        self.code.debug.retain_frame
    }

    fn fixnum_bounds(&self) -> (i64, i64) {
        self.file.target.fixnum_bounds()
    }

    fn popcount_vop(&self, pc: usize) -> Option<(usize, PopcountLowering)> {
        let target = self.vop_target?;
        let index = self
            .code
            .vop_calls
            .binary_search_by_key(&pc, |call| call.pc)
            .ok()?;
        let call = &self.code.vop_calls[index];
        if let (VopTarget::Aarch64Asimd, Some(assembly)) = (target, call.implementations.aarch64) {
            return Some((call.binding, PopcountLowering::Aarch64(assembly)));
        }
        call.implementations
            .portable
            .then_some((call.binding, PopcountLowering::Portable))
    }

    fn named_symbol(&self, const_idx: usize) -> Option<(&str, &str)> {
        match self.literal(const_idx)? {
            LiteralNode::InternedSymbol { package, name } => {
                Some((package.as_str(), name.as_str()))
            }
            _ => None,
        }
    }

    fn constant_type(&self, const_idx: usize) -> TypeTag {
        match self.literal(const_idx) {
            Some(LiteralNode::Nil) => TypeTag::Nil,
            Some(LiteralNode::True) => TypeTag::True,
            Some(LiteralNode::Fixnum(_)) => TypeTag::Fixnum,
            Some(LiteralNode::SingleFloat(_)) | Some(LiteralNode::DoubleFloat(_)) => TypeTag::Float,
            Some(LiteralNode::Character(_)) => TypeTag::Char,
            Some(LiteralNode::Pair(_, _)) => TypeTag::Cons,
            Some(LiteralNode::InternedSymbol { .. }) | Some(LiteralNode::UninternedSymbol(_)) => {
                TypeTag::Symbol
            }
            Some(LiteralNode::Function(_)) => TypeTag::Function,
            _ => TypeTag::Any,
        }
    }
}
