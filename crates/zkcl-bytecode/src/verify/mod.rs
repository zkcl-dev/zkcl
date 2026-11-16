mod control_flow;
mod literals;
mod runtime_objects;

use std::fmt;
use std::ops::Range;

use self::control_flow::{ControlFlowScratch, requires_scope_validation};
use self::literals::LiteralVerifier;
use crate::{
    Arity, BytecodeFunction, Callee, CaptureKind, CellLocation, CompiledFile, HandlerTarget,
    Immediate, Instruction, LiteralNode, RestartTarget, Test,
};

// Frame widths are supplied by the file independently of the byte length. Bound
// the eventual root-frame allocation before any code from the file runs.
const MAX_REGISTERS: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValidationLocation {
    Entry(usize),
    LiteralNode(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {reason}", display_position(*.location, *.instruction))]
pub struct ValidationError {
    location: ValidationLocation,
    instruction: Option<usize>,
    reason: &'static str,
}

impl ValidationError {
    const fn new(
        location: ValidationLocation,
        instruction: Option<usize>,
        reason: &'static str,
    ) -> Self {
        Self {
            location,
            instruction,
            reason,
        }
    }
}

fn display_position(location: ValidationLocation, instruction: Option<usize>) -> impl fmt::Display {
    fmt::from_fn(move |f| {
        match location {
            ValidationLocation::Entry(index) => write!(f, "entry {index}")?,
            ValidationLocation::LiteralNode(index) => write!(f, "literal node {index}")?,
        }
        if let Some(pc) = instruction {
            write!(f, ", instruction {pc}")?;
        }
        Ok(())
    })
}

type Result<T = ()> = std::result::Result<T, ValidationError>;

impl CompiledFile {
    fn valid_fixnum(&self, value: i64) -> bool {
        let (low, high) = self.target.fixnum_bounds();
        (low..=high).contains(&value)
    }

    /// Verify literal references and the register VM's structural invariants.
    pub fn validate(&self) -> Result {
        let mut literals = LiteralVerifier::new(self);
        let mut flow = ControlFlowScratch::default();
        literals.validate_nodes(&mut flow)?;
        for (index, code) in self.entries.iter().enumerate() {
            let verifier = CodeVerifier {
                file: self,
                code,
                location: ValidationLocation::Entry(index),
            };
            if code.arity != Arity::ZERO {
                return Err(verifier.invalid(None, "a file entry must take no arguments"));
            }
            verifier.run(&mut flow)?;
        }
        literals.validate_load_form_order()
    }
}

fn is_symbol(node: &LiteralNode) -> bool {
    match node {
        LiteralNode::Nil
        | LiteralNode::True
        | LiteralNode::InternedSymbol { .. }
        | LiteralNode::UninternedSymbol(_) => true,
        LiteralNode::RuntimeObject { kind, .. } => kind == "symbol",
        _ => false,
    }
}

fn is_cl_constant_name(name: &str) -> bool {
    matches!(
        name,
        "NIL"
            | "T"
            | "PI"
            | "LAMBDA-LIST-KEYWORDS"
            | "CHAR-CODE-LIMIT"
            | "MOST-POSITIVE-FIXNUM"
            | "MOST-NEGATIVE-FIXNUM"
            | "INTERNAL-TIME-UNITS-PER-SECOND"
            | "ARRAY-RANK-LIMIT"
            | "ARRAY-DIMENSION-LIMIT"
            | "ARRAY-TOTAL-SIZE-LIMIT"
            | "CALL-ARGUMENTS-LIMIT"
            | "LAMBDA-PARAMETERS-LIMIT"
            | "MULTIPLE-VALUES-LIMIT"
            | "BOOLE-CLR"
            | "BOOLE-SET"
            | "BOOLE-1"
            | "BOOLE-2"
            | "BOOLE-C1"
            | "BOOLE-C2"
            | "BOOLE-AND"
            | "BOOLE-IOR"
            | "BOOLE-XOR"
            | "BOOLE-EQV"
            | "BOOLE-NAND"
            | "BOOLE-NOR"
            | "BOOLE-ANDC1"
            | "BOOLE-ANDC2"
            | "BOOLE-ORC1"
            | "BOOLE-ORC2"
            | "MOST-POSITIVE-SHORT-FLOAT"
            | "MOST-NEGATIVE-SHORT-FLOAT"
            | "LEAST-POSITIVE-SHORT-FLOAT"
            | "LEAST-NEGATIVE-SHORT-FLOAT"
            | "LEAST-POSITIVE-NORMALIZED-SHORT-FLOAT"
            | "LEAST-NEGATIVE-NORMALIZED-SHORT-FLOAT"
            | "SHORT-FLOAT-EPSILON"
            | "SHORT-FLOAT-NEGATIVE-EPSILON"
            | "MOST-POSITIVE-SINGLE-FLOAT"
            | "MOST-NEGATIVE-SINGLE-FLOAT"
            | "LEAST-POSITIVE-SINGLE-FLOAT"
            | "LEAST-NEGATIVE-SINGLE-FLOAT"
            | "LEAST-POSITIVE-NORMALIZED-SINGLE-FLOAT"
            | "LEAST-NEGATIVE-NORMALIZED-SINGLE-FLOAT"
            | "SINGLE-FLOAT-EPSILON"
            | "SINGLE-FLOAT-NEGATIVE-EPSILON"
            | "MOST-POSITIVE-DOUBLE-FLOAT"
            | "MOST-NEGATIVE-DOUBLE-FLOAT"
            | "LEAST-POSITIVE-DOUBLE-FLOAT"
            | "LEAST-NEGATIVE-DOUBLE-FLOAT"
            | "LEAST-POSITIVE-NORMALIZED-DOUBLE-FLOAT"
            | "LEAST-NEGATIVE-NORMALIZED-DOUBLE-FLOAT"
            | "DOUBLE-FLOAT-EPSILON"
            | "DOUBLE-FLOAT-NEGATIVE-EPSILON"
            | "MOST-POSITIVE-LONG-FLOAT"
            | "MOST-NEGATIVE-LONG-FLOAT"
            | "LEAST-POSITIVE-LONG-FLOAT"
            | "LEAST-NEGATIVE-LONG-FLOAT"
            | "LEAST-POSITIVE-NORMALIZED-LONG-FLOAT"
            | "LEAST-NEGATIVE-NORMALIZED-LONG-FLOAT"
            | "LONG-FLOAT-EPSILON"
            | "LONG-FLOAT-NEGATIVE-EPSILON"
    )
}

struct CodeVerifier<'a> {
    file: &'a CompiledFile,
    code: &'a BytecodeFunction,
    location: ValidationLocation,
}

impl CodeVerifier<'_> {
    fn invalid(&self, instruction: Option<usize>, reason: &'static str) -> ValidationError {
        ValidationError::new(self.location, instruction, reason)
    }

    fn run(&self, flow: &mut ControlFlowScratch) -> Result {
        if self
            .code
            .source_file
            .is_some_and(|index| self.file.source_files.get(index as usize).is_none())
        {
            return Err(self.invalid(None, "unknown source file index"));
        }
        let arity = self.code.arity;
        if arity.required > self.code.registers
            || arity.maximum.is_some_and(|max| max < arity.required)
        {
            return Err(self.invalid(None, "function arity does not fit the register frame"));
        }
        if self.code.facts.params.len() > arity.required {
            return Err(self.invalid(None, "parameter facts exceed required arity"));
        }
        if self.code.registers > MAX_REGISTERS {
            return Err(self.invalid(None, "register frame exceeds the compiled-file limit"));
        }
        if self
            .code
            .constants
            .iter()
            .any(|&index| self.file.nodes.get(index as usize).is_none())
        {
            return Err(self.invalid(None, "constant refers to an unknown literal node"));
        }
        let mut previous = None;
        for vop in &self.code.vop_calls {
            if previous.is_some_and(|pc| pc >= vop.pc)
                || !matches!(self.code.instructions.get(vop.pc),
                    Some(Instruction::Call { callee: Callee::Named(_), args,
                        associate: false, notinline: false, .. }) if args.len() == 1)
                || !matches!(
                    self.constant(None, vop.binding)?,
                    LiteralNode::UninternedSymbol(_)
                )
                || !vop.implementations.portable && vop.implementations.aarch64.is_none()
            {
                return Err(self.invalid(None, "invalid VOP call metadata"));
            }
            previous = Some(vop.pc);
        }
        if let Some(index) = self.code.lambda_list
            && !matches!(
                self.constant(None, index as usize)?,
                LiteralNode::Pair(..) | LiteralNode::Nil
            )
        {
            return Err(self.invalid(None, "lambda list requires a list constant"));
        }
        if let Some(index) = self.code.debug.reader_package {
            let package = self.constant(None, index as usize)?;
            let is_package = match package {
                LiteralNode::Package(_) => true,
                LiteralNode::RuntimeObject { kind, .. } => kind == "package",
                _ => false,
            };
            if !is_package {
                return Err(self.invalid(None, "reader package requires a package constant"));
            }
        }
        if let Some(index) = self.code.debug.source_expression {
            if !matches!(self.constant(None, index as usize)?, LiteralNode::Pair(..)) {
                return Err(self.invalid(None, "source expression requires a list constant"));
            }
        }
        if let Some(index) = self.code.debug.function_name {
            let name = self.constant(None, index as usize)?;
            if !is_symbol(name) && !matches!(name, LiteralNode::Pair(..)) {
                return Err(self.invalid(
                    None,
                    "function name requires a symbol or SETF name constant",
                ));
            }
        }
        let source_text = self
            .code
            .source_file
            .and_then(|index| self.file.source_files.get(index as usize))
            .and_then(|source| source.text.as_deref());
        let valid_source_range = |range: &std::ops::Range<usize>| {
            range.start <= range.end
                && source_text.is_none_or(|text| {
                    range.end <= text.len()
                        && text.is_char_boundary(range.start)
                        && text.is_char_boundary(range.end)
                })
        };
        if self.code.debug.global_function && self.code.debug.function_name.is_none() {
            return Err(self.invalid(None, "global function metadata requires a function name"));
        }
        for definition in &self.code.debug.definitions {
            let range = definition
                .range
                .clone()
                .unwrap_or(definition.at..definition.at);
            if !valid_source_range(&range) {
                return Err(self.invalid(None, "invalid definition source range"));
            }
        }
        let mut previous_source = None;
        for source in &self.code.debug.sources {
            if previous_source.is_some_and(|at| at >= source.at)
                || source
                    .range
                    .as_ref()
                    .is_some_and(|range| !valid_source_range(range))
                || source
                    .expansion
                    .iter()
                    .any(|range| !valid_source_range(range))
            {
                return Err(self.invalid(None, "invalid source occurrence metadata"));
            }
            previous_source = Some(source.at);
        }
        for variable in &self.code.debug.variables {
            use crate::DebugVariableLocation::*;
            let location_valid = match variable.location {
                Register { index, .. } => index < self.code.registers,
                Capture(index) => self
                    .code
                    .captures
                    .get(index)
                    .is_some_and(|kind| *kind != crate::CaptureKind::ExitToken),
            };
            if !location_valid
                || variable.live.start > variable.live.end
                || variable.live.end > self.code.instructions.len()
                || !is_symbol(self.constant(None, variable.name as usize)?)
            {
                return Err(self.invalid(None, "invalid debugger variable metadata"));
            }
        }
        for &index in &self.code.notinline {
            let Some(node) = self
                .code
                .constants
                .get(index as usize)
                .map(|&node| &self.file.nodes[node as usize])
            else {
                return Err(self.invalid(None, "notinline declaration constant is out of bounds"));
            };
            if !is_symbol(node) {
                return Err(self.invalid(
                    None,
                    "notinline declaration must reference a symbol constant",
                ));
            }
        }
        self.validate_tables()?;
        let max_checked_keywords = if self.code.keywords.is_empty() {
            0
        } else {
            self.code
                .instructions
                .iter()
                .filter_map(|inst| match inst {
                    Instruction::CheckKeywords { table, .. } => {
                        self.code.keywords.get(*table).map(|kw| kw.names.len())
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(0)
        };
        let mut needs_scope_check = false;
        for (pc, instruction) in self.code.instructions.iter().enumerate() {
            self.operands(pc, instruction, max_checked_keywords)?;
            needs_scope_check = needs_scope_check || requires_scope_validation(instruction);
        }
        if needs_scope_check {
            flow.validate(self)?;
        }
        Ok(())
    }

    fn reg(&self, instruction: Option<usize>, reg: usize) -> Result {
        (reg < self.code.registers)
            .then_some(())
            .ok_or_else(|| self.invalid(instruction, "register is out of bounds"))
    }

    fn bound(&self, instruction: Option<usize>, register: usize) -> Result {
        (register <= self.code.registers)
            .then_some(())
            .ok_or_else(|| self.invalid(instruction, "register range endpoint is out of bounds"))
    }

    fn regs(&self, instruction: Option<usize>, range: &Range<usize>) -> Result {
        if range.start > range.end {
            return Err(self.invalid(instruction, "register range is reversed"));
        }
        self.bound(instruction, range.end)
    }

    fn jump(&self, instruction: Option<usize>, target: usize) -> Result {
        (target <= self.code.instructions.len())
            .then_some(())
            .ok_or_else(|| self.invalid(instruction, "jump target is out of bounds"))
    }

    fn constant(&self, instruction: Option<usize>, index: usize) -> Result<&LiteralNode> {
        self.code
            .constants
            .get(index)
            .map(|&index| &self.file.nodes[index as usize])
            .ok_or_else(|| self.invalid(instruction, "constant index is out of bounds"))
    }

    fn symbol(&self, instruction: Option<usize>, index: usize) -> Result {
        is_symbol(self.constant(instruction, index)?)
            .then_some(())
            .ok_or_else(|| self.invalid(instruction, "instruction requires a symbol constant"))
    }

    fn heap_symbol(&self, instruction: Option<usize>, index: usize) -> Result<&LiteralNode> {
        let node = self.constant(instruction, index)?;
        let valid = match node {
            LiteralNode::InternedSymbol { package, name } => {
                !matches!(package.as_str(), "COMMON-LISP" | "CL")
                    || !matches!(name.as_str(), "NIL" | "T")
            }
            LiteralNode::UninternedSymbol(_) => true,
            LiteralNode::RuntimeObject { kind, .. } if kind == "symbol" => true,
            _ => false,
        };
        valid
            .then_some(node)
            .ok_or_else(|| self.invalid(instruction, "instruction requires a heap symbol constant"))
    }

    fn mutable_symbol(&self, instruction: Option<usize>, index: usize) -> Result {
        let is_constant = match self.heap_symbol(instruction, index)? {
            LiteralNode::InternedSymbol { package, name } => {
                package == "KEYWORD"
                    || (matches!(package.as_str(), "COMMON-LISP" | "CL")
                        && is_cl_constant_name(name))
            }
            LiteralNode::RuntimeObject { kind, data, .. } if kind == "symbol" => {
                data.get(3) == Some(&1) || data.get(4) == Some(&1) || data.get(1) == Some(&3)
            }
            _ => false,
        };
        (!is_constant)
            .then_some(())
            .ok_or_else(|| self.invalid(instruction, "instruction cannot target a constant symbol"))
    }

    fn validate_tables(&self) -> Result {
        for table in &self.code.handlers {
            for handler in &table.handlers {
                if let Test::Named(index) = handler.test {
                    let specifier = self.constant(None, index)?;
                    // A load form can supply a class metaobject to TYPEP.
                    if !is_symbol(specifier)
                        && !matches!(
                            specifier,
                            LiteralNode::Pair(..)
                                | LiteralNode::LoadForm { .. }
                                | LiteralNode::RuntimeObject { .. }
                        )
                    {
                        return Err(self.invalid(None, "handler requires a type specifier"));
                    }
                }
                match handler.target {
                    HandlerTarget::Function(r) => self.reg(None, r)?,
                    HandlerTarget::Case {
                        pc,
                        condition,
                        clear,
                    } => {
                        self.jump(None, pc)?;
                        self.reg(None, condition)?;
                        self.bound(None, clear)?;
                    }
                }
            }
        }
        for table in &self.code.restarts {
            for spec in &table.restarts {
                self.symbol(None, spec.name)?;
                for reg in spec.frame_registers() {
                    self.reg(None, reg)?;
                }
                if let RestartTarget::Case { pc, clear } = spec.target {
                    self.jump(None, pc)?;
                    self.bound(None, clear)?;
                }
            }
        }
        for table in &self.code.keywords {
            self.symbol(None, table.override_key)?;
            for &idx in &table.names {
                self.symbol(None, idx)?;
            }
        }
        for table in &self.code.jump_tables {
            if table.targets.is_empty() {
                return Err(self.invalid(None, "jump table cannot be empty"));
            }
            for &target in &table.targets {
                self.jump(None, target)?;
            }
        }
        Ok(())
    }

    fn operands(
        &self,
        pc: usize,
        instruction: &Instruction,
        max_checked_keywords: usize,
    ) -> Result {
        use Instruction::*;

        let at = Some(pc);
        let invalid = |reason| self.invalid(at, reason);
        let reg = |reg| self.reg(at, reg);
        let bound = |register| self.bound(at, register);
        let regs = |range: &Range<usize>| self.regs(at, range);
        let jump = |target| self.jump(at, target);
        let constant = |index| self.constant(at, index);
        let symbol = |index| self.symbol(at, index);
        let heap_symbol = |index| self.heap_symbol(at, index);
        let mutable_symbol = |index| self.mutable_symbol(at, index);
        let keyword_table = |index| {
            self.code
                .keywords
                .get(index)
                .ok_or_else(|| invalid("keyword table index is out of bounds"))
        };
        let capture_kind = |index: usize| {
            self.code
                .captures
                .get(index)
                .copied()
                .ok_or_else(|| invalid("closure capture index is out of bounds"))
        };

        match instruction {
            Load(out, immediate) => {
                reg(*out)?;
                if matches!(immediate, Immediate::Fixnum(value) if !self.file.valid_fixnum(*value))
                {
                    return Err(invalid("fixnum is outside the VM's tagged range"));
                }
                Ok(())
            }
            BoxLocal { reg: out, .. }
            | ExitToken(out, _)
            | ReadValue(out, _)
            | RestArguments { out, .. }
            | ReturnFrom { token: out, .. }
            | Throw { tag: out, .. }
            | Go { token: out, .. }
            | CallValues { callee: out, .. }
            | DestructureEnd { cursor: out, .. } => reg(*out),
            OptionalArgument(index, out) => {
                reg(*out)?;
                if let Some(max) = self.code.arity.maximum
                    && *index >= max - self.code.arity.required
                {
                    return Err(invalid(
                        "optional argument index is out of bounds for function arity",
                    ));
                }
                Ok(())
            }
            KeywordArgument(index, out) => {
                reg(*out)?;
                if self.code.arity.maximum.is_some() {
                    return Err(invalid(
                        "keyword argument requires unbounded function arity",
                    ));
                }
                (*index < max_checked_keywords)
                    .then_some(())
                    .ok_or_else(|| invalid("keyword argument index is out of bounds"))
            }
            Capture(out, index) => {
                reg(*out)?;
                capture_kind(*index).map(|_| ())
            }
            Constant(out, index) => {
                reg(*out)?;
                (*index < self.code.constants.len())
                    .then_some(())
                    .ok_or_else(|| invalid("constant index is out of bounds"))
            }
            Copy(a, b)
            | BindProgv {
                symbols: a,
                values: b,
                ..
            }
            | DestructureNext {
                cursor: a, out: b, ..
            }
            | AssociateRestarts {
                condition: a,
                list: b,
                ..
            } => {
                reg(*a)?;
                reg(*b)
            }
            Clear(range) | SetValues(range) | BindValues(range) => regs(range),
            Jump(target) => jump(*target),
            Unless(test, target) | IfPresent(test, target) | UnlessFixnum(test, target) => {
                reg(*test)?;
                jump(*target)
            }
            UnlessFixnumInRange(test, low, high, target) => {
                if low > high
                    || !self.file.valid_fixnum(i64::from(*low))
                    || !self.file.valid_fixnum(i64::from(*high))
                {
                    return Err(invalid("invalid fixnum range bounds"));
                }
                reg(*test)?;
                jump(*target)
            }
            TryFixnumArithmetic {
                op,
                symbol: index,
                out,
                args,
            } => {
                reg(*out)?;
                regs(args)?;
                if args.len() != op.arity() {
                    return Err(invalid("wrong argument count for fixnum arithmetic"));
                }
                symbol(*index)
            }
            ReadCell {
                out, symbol: index, ..
            } => {
                reg(*out)?;
                symbol(*index)
            }
            WriteCell { symbol: index, src }
            | BindSpecial {
                symbol: index,
                value: src,
            } => {
                reg(*src)?;
                mutable_symbol(*index)
            }
            DefineFunction {
                symbol: index,
                definition: src,
                ..
            } => {
                reg(*src)?;
                heap_symbol(*index).map(|_| ())
            }
            ProclaimSpecial { symbol: index, .. } => mutable_symbol(*index),
            ProclaimInline { symbol: index, .. } => heap_symbol(*index).map(|_| ()),
            SetVariableDocumentation { symbol: index, doc } => {
                heap_symbol(*index)?;
                matches!(constant(*doc)?, LiteralNode::String(_))
                    .then_some(())
                    .ok_or_else(|| invalid("documentation constant must be a string"))
            }
            WhenBound {
                symbol: index,
                jump: target,
            } => {
                symbol(*index)?;
                jump(*target)
            }
            ReadBox {
                out,
                cell: location,
            }
            | WriteBox {
                src: out,
                cell: location,
            } => {
                reg(*out)?;
                match *location {
                    CellLocation::Local(local) => reg(local),
                    CellLocation::Capture(index) => {
                        let kind = capture_kind(index)?;
                        if matches!(instruction, WriteBox { .. })
                            && kind != CaptureKind::MutableCell
                        {
                            return Err(invalid("cannot write to an immutable closure capture"));
                        }
                        Ok(())
                    }
                }
            }
            Closure {
                out, code, cells, ..
            } => {
                reg(*out)?;
                regs(cells)?;
                let LiteralNode::Function(target) = constant(*code)? else {
                    return Err(invalid("closure code constant must be a function"));
                };
                (cells.len() == target.captures.len())
                    .then_some(())
                    .ok_or_else(|| invalid("closure capture count does not match target code"))
            }
            EnterBlock {
                token,
                resume,
                clear,
            }
            | EnterCatch {
                tag: token,
                resume,
                clear,
            } => {
                reg(*token)?;
                bound(*clear)?;
                jump(*resume)
            }
            EnterTagbody {
                token,
                jumps,
                clear,
            } => {
                reg(*token)?;
                bound(*clear)?;
                (*jumps < self.code.jump_tables.len())
                    .then_some(())
                    .ok_or_else(|| invalid("jump table index is out of bounds"))
            }
            EnterProtect { cleanup, clear } => {
                bound(*clear)?;
                jump(*cleanup)
            }
            EnterHandlers { table } => (*table < self.code.handlers.len())
                .then_some(())
                .ok_or_else(|| invalid("handler table index is out of bounds")),
            EnterRestarts { table, .. } => (*table < self.code.restarts.len())
                .then_some(())
                .ok_or_else(|| invalid("restart table index is out of bounds")),
            CheckKeywords { start, table } => {
                bound(*start)?;
                keyword_table(*table).map(|_| ())
            }
            DestructureKeywords {
                cursor, table, out, ..
            } => {
                reg(*cursor)?;
                let table = keyword_table(*table)?;
                regs(out)?;
                (out.len() == table.names.len())
                    .then_some(())
                    .ok_or_else(|| {
                        invalid("destructure-keywords register range does not match table width")
                    })
            }
            Call {
                callee, out, args, ..
            } => {
                match callee {
                    Callee::Named(index) => symbol(*index)?,
                    Callee::Value(r) => reg(*r)?,
                }
                if let Some(r) = out {
                    reg(*r)?;
                }
                regs(args)
            }
            SaveValues
            | AppendValues
            | RestoreValues
            | Unbind(_)
            | LeaveTarget
            | BeginCleanup
            | EndCleanup
            | LeaveHandlers
            | LeaveRestarts
            | LeaveRestartAssociation
            | ReleaseArguments => Ok(()),
        }
    }
}
