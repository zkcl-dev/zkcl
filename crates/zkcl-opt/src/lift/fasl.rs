use zkcl_bytecode::{BytecodeFunction, CompiledFile, LiteralNode};

use crate::{
    BytecodeLiftError, FloatFormat, OptimizeConfig, SpecializationInputs, SsaFunction, TypeTag,
    lift_code, optimize_with_options,
};

/// Supplies literal types and symbol names from the function's FASL constant pool.
pub struct FaslSpecializationInputs<'a> {
    file: &'a CompiledFile,
    code: &'a BytecodeFunction,
    transforms: bool,
}

impl<'a> FaslSpecializationInputs<'a> {
    pub fn new(file: &'a CompiledFile, code: &'a BytecodeFunction) -> Self {
        Self {
            file,
            code,
            transforms: false,
        }
    }

    pub fn with_transforms(mut self) -> Self {
        self.transforms = true;
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

    /// Optimize a native entry using static numeric evidence from the file.
    pub fn optimize_native(&self) -> Result<SsaFunction, BytecodeLiftError> {
        let mut function = lift_code(self.code, self)?;
        crate::passes::optimize_static_native(&mut function, self);
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

    fn enable_transforms(&self) -> bool {
        self.transforms
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

    fn float_literal_format(&self, const_idx: usize) -> Option<FloatFormat> {
        match self.literal(const_idx)? {
            LiteralNode::SingleFloat(_) => Some(FloatFormat::F32),
            LiteralNode::DoubleFloat(_) => Some(FloatFormat::F64),
            _ => None,
        }
    }

    fn float_vector_type_format(&self, const_idx: usize) -> Option<FloatFormat> {
        let node = |index: u32| self.file.nodes.get(index as usize);
        let cl_symbol = |index| match node(index)? {
            LiteralNode::InternedSymbol { package, name }
                if matches!(package.as_str(), "COMMON-LISP" | "CL") =>
            {
                Some(name.as_str())
            }
            _ => None,
        };
        let LiteralNode::Pair(operator, rest) = *self.literal(const_idx)? else {
            return None;
        };
        let LiteralNode::Pair(element, rest) = *node(rest)? else {
            return None;
        };
        let LiteralNode::Pair(dimensions, end) = *node(rest)? else {
            return None;
        };
        let LiteralNode::Pair(length, dim_end) = *node(dimensions)? else {
            return None;
        };
        if cl_symbol(operator)? != "SIMPLE-ARRAY"
            || !matches!(node(end)?, LiteralNode::Nil)
            || !matches!(node(dim_end)?, LiteralNode::Nil)
            || !(cl_symbol(length) == Some("*")
                || matches!(node(length)?, LiteralNode::Fixnum(length) if *length >= 0))
        {
            return None;
        }
        match cl_symbol(element)? {
            "SHORT-FLOAT" | "SINGLE-FLOAT" => Some(FloatFormat::F32),
            "DOUBLE-FLOAT" | "LONG-FLOAT" => Some(FloatFormat::F64),
            _ => None,
        }
    }

    fn is_setf_aref_literal(&self, const_idx: usize) -> bool {
        matches!(self.literal(const_idx), Some(LiteralNode::Primitive(name)) if name == "(SETF AREF)")
    }
}
