//! Structured JavaScript lowering for pure bytecode functions.
//!
//! Functions that only move values between registers, branch, and call other
//! functions are lowered via [`zkcl_opt::structure_graph`] directly into labeled
//! JS blocks/loops with lexical register variables, eliminating the
//! `switch (frame.pc)` dispatch loop, `frame.registers` heap reads/writes, and
//! per-instruction `task.budget` checks.
//!
//! A call still enters the runtime's frame stack, which can only resume through
//! the dispatched step function. The lowering therefore flushes the registers
//! that may differ from the frame before each call and returns when control
//! transfers. The runtime runs `fast` from PC zero, so a suspended frame
//! normally continues in `step` from the instruction after the call.
//!
//! A loop back edge is the second exit. When the body has no dispatch region it
//! can be entered with a finite budget, so every back edge spends one budget unit
//! and, when it runs out, flushes the registers, records the loop header's PC and
//! returns. The runtime then resumes the frame in `step` at that PC. Bodies with a
//! dispatch region keep the sync-only contract: the local dispatch index
//! cannot be reconstructed from a single `frame.pc`.

use std::collections::BTreeSet;
use std::fmt::Write;

use crate::emit::{
    BranchCondition, Steps, constant_label, identifier_fragment, instruction_label,
    truncate_comment, write_symbol_component,
};
use crate::js_source::indent_str;
use crate::source_map::SourceMap;
use zkcl_bytecode::{
    BlockLeaders, BytecodeFunction, CompiledSource, Immediate, Instruction, InstructionFlow,
    LiteralNode, RegisterSet,
};
use zkcl_opt::{StructuredCfg, StructuredRegion, structure_graph};

const MAX_FAST_REGISTERS: usize = 64;

/// What the emitted body records in the function's data object.
pub(super) struct FastEmission {
    /// The body spends one budget unit per loop back edge, so the runtime may
    /// enter it with a finite budget.
    pub(super) checks_budget: bool,
}

/// Emit a structured `function {name}(task, frame)` fast path when `code` is
/// within the register/range unrolling budget. Its call operands are already
/// written into the function's data object.
pub(super) fn try_emit_fast_function<'source>(
    steps: &mut Steps<'source>,
    code: &'source BytecodeFunction,
    name: &str,
    nodes: &[LiteralNode],
    operand_indices: &[Option<usize>],
    register_names: &[Option<&str>],
    source: Option<&'source CompiledSource>,
) -> Option<FastEmission> {
    let summary = analyze_fast_eligibility(code)?;

    let (blocks, rpo, exit_block) = build_basic_blocks(&code.instructions);
    let mut predecessors = vec![0; blocks.len()];
    let flow = structure_graph(
        blocks.len(),
        &[0],
        |block, add_edge| {
            for &succ in &blocks[block].succs {
                add_edge(succ);
            }
        },
        &rpo,
    );
    for &block in &rpo {
        for &target in &blocks[block].succs {
            predecessors[target] += 1;
        }
    }

    // A dispatch region resumes through its local index, which a single `frame.pc` cannot
    // reconstruct, so those bodies stay sync-only and record no budget claim.
    let checks_budget = !flow.dispatch.iter().any(Option::is_some);

    let dirty_in = dirty_registers(&code.instructions, &blocks, &rpo);
    let locals: Vec<_> = (0..code.registers)
        .map(|register| {
            let mut local = format!("r{register}");
            if let Some(Some(name)) = register_names.get(register) {
                let suffix = identifier_fragment(name, 24).to_ascii_lowercase();
                let suffix = suffix.trim_matches('_');
                if !suffix.is_empty() {
                    write!(local, "_{suffix}").unwrap();
                }
            }
            local
        })
        .collect();
    if let Some(at) = code.debug.entry {
        steps.map.mark(
            steps.text.len() + "function ".len(),
            source,
            &code.debug,
            at,
            code.name.as_deref(),
        );
    }
    emit_header(
        &mut steps.text,
        code,
        name,
        &summary,
        checks_budget,
        &locals,
        register_names,
    );
    let mut emitter = StructuredEmitter {
        out: &mut steps.text,
        map: &mut steps.map,
        source,
        code,
        nodes,
        instructions: &code.instructions,
        blocks: &blocks,
        predecessors: &predecessors,
        exit_block,
        flow: &flow,
        operand_indices,
        locals: &locals,
        localized: summary.localized,
        dirty_in: &dirty_in,
        dirty: 0,
        active_loops: Vec::new(),
        checks_budget,
    };
    if let Some((first, rest)) = flow.regions.split_first() {
        emitter.emit_region_list(first, rest, 1);
    }
    emitter.out.push_str("}\n\n");
    Some(FastEmission { checks_budget })
}

#[derive(Default)]
struct FastSummary {
    /// The runtime objects the lowered body reads, named once in its header.
    objects: BTreeSet<&'static str>,
    needs_constants: bool,
    needs_captures: bool,
    needs_operands: bool,
    localized: u64,
}

fn analyze_fast_eligibility(code: &BytecodeFunction) -> Option<FastSummary> {
    if code.instructions.is_empty() || code.registers > MAX_FAST_REGISTERS {
        return None;
    }
    let mut summary = FastSummary::default();

    // The compiled-file verifier has already checked registers and branches.
    for instruction in &code.instructions {
        use Instruction::*;
        let object = match instruction {
            Load(_, Immediate::Nil) | Unless(..) | ReadValue(..) => Some("NIL"),
            Load(_, Immediate::True) => Some("T"),
            Load(_, Immediate::Character(_)) => Some("LispCharacter"),
            IfPresent(..) | OptionalArgument(..) => Some("UNBOUND"),
            Clear(range) | BindValues(range) if !range.is_empty() => Some("NIL"),
            Constant(..) => {
                summary.needs_constants = true;
                None
            }
            Capture(..) => {
                summary.needs_captures = true;
                None
            }
            Call { .. } => {
                summary.needs_operands = true;
                None
            }
            Load(_, Immediate::Fixnum(_))
            | Copy(..)
            | Clear(..)
            | Jump(..)
            | UnlessFixnum(..)
            | UnlessFixnumInRange(..)
            | SetValues(..)
            | BindValues(..)
            | ReleaseArguments => None,
            _ => return None,
        };
        if let Some(object) = object {
            summary.objects.insert(object);
        }
        let operands = instruction.register_operands();
        summary.localized |= register_mask(operands.reads) | register_mask(operands.writes);
    }
    Some(summary)
}

struct BasicBlockInfo {
    start_pc: usize,
    end_pc: usize,
    succs: Vec<usize>,
}

fn build_basic_blocks(instructions: &[Instruction]) -> (Vec<BasicBlockInfo>, Vec<usize>, usize) {
    let mut leaders = BlockLeaders::new(instructions.len());
    for (pc, instruction) in instructions.iter().enumerate() {
        leaders.observe(pc, instruction.flow());
    }

    let ranges: Vec<_> = leaders.ranges().collect();
    let block_for_pc = |pc| {
        ranges
            .binary_search_by_key(&pc, |range| range.start)
            .unwrap()
    };
    let exit_block = ranges.len() - 1;
    let mut blocks = Vec::with_capacity(ranges.len());
    for range in &ranges {
        let mut succs = Vec::new();
        if !range.is_empty() {
            let flow = instructions[range.end - 1].flow();
            // The structured emitter consumes an explicit branch target first.
            succs.extend(flow.successors(range.end).rev().map(block_for_pc));
            succs.dedup();
        }
        blocks.push(BasicBlockInfo {
            start_pc: range.start,
            end_pc: range.end,
            succs,
        });
    }

    // Compute reverse postorder over blocks reachable from entry block 0.
    let mut visited = vec![false; blocks.len()];
    let mut postorder = Vec::with_capacity(blocks.len());
    let mut stack = vec![(0usize, false)];
    while let Some((block, exit)) = stack.pop() {
        if exit {
            postorder.push(block);
        } else if !visited[block] {
            visited[block] = true;
            stack.push((block, true));
            for &succ in blocks[block].succs.iter().rev() {
                if !visited[succ] {
                    stack.push((succ, false));
                }
            }
        }
    }
    postorder.reverse();
    (blocks, postorder, exit_block)
}

fn register_writes(instruction: &Instruction) -> u64 {
    // A self-copy is elided by the structured emitter.
    if matches!(instruction, Instruction::Copy(out, src) if out == src) {
        return 0;
    }
    register_mask(instruction.register_operands().writes)
}

fn register_mask(registers: RegisterSet<'_>) -> u64 {
    let mut mask = 0;
    registers.for_each(|reg| mask |= 1 << reg);
    mask
}

/// A register is dirty if any incoming path assigned it since the last call.
/// The frame is observable to host callbacks, so the set includes dead slots.
fn dirty_registers(
    instructions: &[Instruction],
    blocks: &[BasicBlockInfo],
    rpo: &[usize],
) -> Vec<u64> {
    let transfers: Vec<_> = blocks
        .iter()
        .map(|block| {
            let mut preserves_input = true;
            let mut writes = 0;
            for instruction in &instructions[block.start_pc..block.end_pc] {
                if matches!(instruction, Instruction::Call { .. }) {
                    // The call flushes prior writes and produces its result in the frame.
                    preserves_input = false;
                    writes = 0;
                } else {
                    writes |= register_writes(instruction);
                }
            }
            (preserves_input, writes)
        })
        .collect();

    let mut incoming = vec![0; blocks.len()];
    let mut pending: Vec<_> = rpo.iter().rev().copied().collect();
    let mut queued = vec![true; blocks.len()];
    while let Some(block) = pending.pop() {
        queued[block] = false;
        let (preserves_input, writes) = transfers[block];
        let outgoing = if preserves_input {
            incoming[block] | writes
        } else {
            writes
        };
        for &target in &blocks[block].succs {
            if outgoing & !incoming[target] != 0 {
                incoming[target] |= outgoing;
                if !std::mem::replace(&mut queued[target], true) {
                    pending.push(target);
                }
            }
        }
    }
    incoming
}

fn emit_header(
    out: &mut String,
    code: &BytecodeFunction,
    name: &str,
    summary: &FastSummary,
    checks_budget: bool,
    locals: &[String],
    register_names: &[Option<&str>],
) {
    writeln!(out, "function {name}(task, frame) {{").unwrap();
    if !summary.objects.is_empty() {
        let names: Vec<_> = summary.objects.iter().copied().collect();
        writeln!(
            out,
            "  const {{ {} }} = task.runtime.objects;",
            names.join(", ")
        )
        .unwrap();
    }
    if summary.needs_constants {
        out.push_str("  const constants = frame.constants;\n");
    }
    if summary.needs_captures {
        out.push_str("  const captures = frame.captures;\n");
    }
    if summary.localized != 0 {
        out.push_str("  const regs = frame.registers;\n");
        for (register, local) in locals.iter().enumerate() {
            if summary.localized & (1 << register) == 0 {
                continue;
            }
            write!(out, "  let {local} = regs[{register}];").unwrap();
            if let Some(Some(name)) = register_names.get(register) {
                let mut label = String::new();
                write_symbol_component(&mut label, name);
                let kind = if register < code.arity.required {
                    "parameter"
                } else {
                    "variable"
                };
                write!(out, " // {kind} {}", truncate_comment(label)).unwrap();
            }
            out.push('\n');
        }
    }
    if summary.needs_operands {
        out.push_str("  const operands = frame.code.operands;\n  let resumeEpoch;\n");
    }
    if !checks_budget {
        out.push_str("  let dispatchIndex = 0;\n");
    }
}

struct StructuredEmitter<'a, 'source> {
    out: &'a mut String,
    map: &'a mut SourceMap<'source>,
    source: Option<&'source CompiledSource>,
    code: &'source BytecodeFunction,
    nodes: &'a [LiteralNode],
    instructions: &'a [Instruction],
    blocks: &'a [BasicBlockInfo],
    predecessors: &'a [usize],
    exit_block: usize,
    flow: &'a StructuredCfg,
    operand_indices: &'a [Option<usize>],
    locals: &'a [String],
    localized: u64,
    dirty_in: &'a [u64],
    dirty: u64,
    active_loops: Vec<usize>,
    /// Whether loop back edges guard the budget (false when a dispatch region
    /// makes the body unreachable from a single PC).
    checks_budget: bool,
}

impl StructuredEmitter<'_, '_> {
    fn emit_region_list<'region>(
        &mut self,
        mut first: &'region StructuredRegion,
        mut rest: &'region [StructuredRegion],
        indent: usize,
    ) {
        while let (
            StructuredRegion::Block(head),
            [
                StructuredRegion::Block(left),
                StructuredRegion::Block(right),
                join @ StructuredRegion::Block(next),
                tail @ ..,
            ],
        ) = (first, rest)
        {
            if !self.emit_diamond(*head, *left, *right, *next, indent) {
                break;
            }
            first = join;
            rest = tail;
        }
        let needs_label = |region: &StructuredRegion| self.flow.forward_labels[region.entry()];
        // Breaking out of a label enters the corresponding later region.
        let mut body_indent = indent;
        for region in rest.iter().rev().filter(|region| needs_label(region)) {
            writeln!(
                self.out,
                "{}block_pc{}: {{",
                indent_str(body_indent),
                self.blocks[region.entry()].start_pc
            )
            .unwrap();
            body_indent += 1;
        }
        self.emit_region(
            first,
            body_indent,
            rest.first().map(StructuredRegion::entry),
        );
        for (index, region) in rest.iter().enumerate() {
            if needs_label(region) {
                body_indent -= 1;
                writeln!(self.out, "{}}}", indent_str(body_indent)).unwrap();
            }
            self.emit_region(
                region,
                body_indent,
                rest.get(index + 1).map(StructuredRegion::entry),
            );
        }
    }

    /// Lower two single-entry arms that reunite immediately to a plain `if`.
    fn emit_diamond(
        &mut self,
        head: usize,
        left: usize,
        right: usize,
        join: usize,
        indent: usize,
    ) -> bool {
        let &[taken, fall] = self.blocks[head].succs.as_slice() else {
            return false;
        };
        if !((taken == left && fall == right) || (taken == right && fall == left))
            || self.blocks[left].succs != [join]
            || self.blocks[right].succs != [join]
            || self.predecessors[left] != 1
            || self.predecessors[right] != 1
        {
            return false;
        }
        let pad = indent_str(indent);
        let (register, _, condition) =
            BranchCondition::for_instruction(&self.instructions[self.blocks[head].end_pc - 1]);
        let test = condition.expression(&self.locals[register], true);
        self.emit_block_body(head, &pad);
        writeln!(self.out, "{pad}if ({test}) {{").unwrap();
        self.emit_block(taken, indent + 1, Some(join));
        writeln!(self.out, "{pad}}} else {{").unwrap();
        self.emit_block(fall, indent + 1, Some(join));
        writeln!(self.out, "{pad}}}").unwrap();
        true
    }

    fn emit_region(&mut self, region: &StructuredRegion, indent: usize, next: Option<usize>) {
        match region {
            StructuredRegion::Block(block) => self.emit_block(*block, indent, next),
            StructuredRegion::Loop { header, body } => {
                let pad = indent_str(indent);
                let pc = self.blocks[*header].start_pc;
                writeln!(self.out, "{pad}loop_pc{pc}: while (true) {{").unwrap();
                self.active_loops.push(*header);
                self.emit_region_list(&StructuredRegion::Block(*header), body, indent + 1);
                self.active_loops.pop();
                writeln!(self.out, "{pad}}}").unwrap();
            }
            StructuredRegion::Dispatch(members) => {
                let entry = members[0];
                let pc = self.blocks[entry].start_pc;
                let pad = indent_str(indent);
                writeln!(self.out, "{pad}loop_pc{pc}: while (true) {{").unwrap();
                writeln!(self.out, "{pad}  switch (dispatchIndex) {{").unwrap();
                self.active_loops.push(entry);
                for (case, &member) in members.iter().enumerate() {
                    let pc = self.blocks[member].start_pc;
                    writeln!(self.out, "{pad}    case {case}: {{ // PC {pc}").unwrap();
                    self.emit_block(member, indent + 3, None);
                    writeln!(self.out, "{pad}    }}").unwrap();
                }
                self.active_loops.pop();
                writeln!(self.out, "{pad}  }}").unwrap();
                writeln!(self.out, "{pad}}}").unwrap();
            }
        }
    }

    fn emit_block(&mut self, block: usize, indent: usize, next: Option<usize>) {
        let pad = indent_str(indent);
        if block == self.exit_block {
            writeln!(self.out, "{pad}return task.returnFrame(frame);").unwrap();
            return;
        }
        let info = &self.blocks[block];
        let last_pc = info.end_pc - 1;
        self.emit_block_body(block, &pad);
        let [taken, fall] = info.succs.as_slice() else {
            self.emit_edge(info.succs[0], last_pc, &pad, next);
            return;
        };
        // An edge can expand to several lines (a budget guard), so the shape of
        // the branch is chosen from whether each edge needs statements at all.
        let (first, otherwise) = match (
            self.edge_has_statements(*taken, next),
            self.edge_has_statements(*fall, next),
        ) {
            (true, true) => (*taken, Some(*fall)),
            (true, false) => (*taken, None),
            (false, true) => (*fall, None),
            (false, false) => return,
        };
        let (register, _, condition) =
            BranchCondition::for_instruction(&self.instructions[last_pc]);
        let test = condition.expression(&self.locals[register], first == *taken);
        let nested = format!("{pad}  ");
        writeln!(self.out, "{pad}if ({test}) {{").unwrap();
        self.emit_edge(first, last_pc, &nested, next);
        if let Some(otherwise) = otherwise {
            writeln!(self.out, "{pad}}} else {{").unwrap();
            self.emit_edge(otherwise, last_pc, &nested, next);
        }
        writeln!(self.out, "{pad}}}").unwrap();
    }

    fn emit_block_body(&mut self, block: usize, pad: &str) {
        let info = &self.blocks[block];
        let instructions = self.instructions;
        let last = &instructions[info.end_pc - 1];
        let end = info.end_pc - usize::from(last.flow() != InstructionFlow::Next);
        self.dirty = self.dirty_in[block];
        for (pc, instruction) in instructions
            .iter()
            .enumerate()
            .take(end)
            .skip(info.start_pc)
        {
            self.emit_statement(pc, instruction, pad);
            if matches!(instruction, Instruction::Call { .. }) {
                self.dirty = 0;
            } else {
                self.dirty |= register_writes(instruction);
            }
        }
    }

    /// Whether the edge to `target` needs statements: a dispatch edge always
    /// records its case, and a plain edge is skipped when the next region is
    /// already at that label.
    fn edge_has_statements(&self, target: usize, next: Option<usize>) -> bool {
        self.flow.dispatch[target].is_some() || Some(target) != next
    }

    fn emit_edge(&mut self, target: usize, from_pc: usize, pad: &str, next: Option<usize>) {
        let anchor = match self.flow.dispatch[target] {
            Some((entry, case)) => {
                writeln!(self.out, "{pad}dispatchIndex = {case};").unwrap();
                entry
            }
            None => target,
        };
        if Some(anchor) == next {
            return;
        }
        if self.active_loops.contains(&anchor) {
            self.emit_loop_back_edge(anchor, from_pc, pad);
        } else {
            writeln!(
                self.out,
                "{pad}break block_pc{};",
                self.blocks[anchor].start_pc
            )
            .unwrap();
        }
    }

    /// Jump to a loop header, guarding the budget when this body claims it.
    ///
    /// On exhaustion the frame's register file is made authoritative, the
    /// header's PC is recorded, and the frame returns to the runtime, which
    /// resumes it in the dispatched step function. The recorded PC is the
    /// bytecode jump target, also used in the loop label.
    fn emit_loop_back_edge(&mut self, header: usize, from_pc: usize, pad: &str) {
        let pc = self.blocks[header].start_pc;
        if self.checks_budget {
            let body = format!("{pad}  ");
            writeln!(self.out, "{pad}if (task.budget-- <= 0) {{").unwrap();
            self.emit_register_writeback(&body);
            writeln!(self.out, "{body}frame.sourcePc = {from_pc};").unwrap();
            writeln!(self.out, "{body}frame.pc = {pc};").unwrap();
            writeln!(self.out, "{body}return;").unwrap();
            writeln!(self.out, "{pad}}}").unwrap();
        }
        writeln!(self.out, "{pad}continue loop_pc{pc};").unwrap();
    }

    fn emit_register_writeback(&mut self, pad: &str) {
        let mut dirty = self.dirty;
        while dirty != 0 {
            let register = dirty.trailing_zeros() as usize;
            dirty &= dirty - 1;
            writeln!(
                self.out,
                "{pad}regs[{register}] = {};",
                self.locals[register]
            )
            .unwrap();
        }
    }

    fn emit_statement(&mut self, pc: usize, instruction: &Instruction, pad: &str) {
        use Instruction::*;
        let locals = self.locals;
        let r = |reg: usize| &locals[reg];
        match instruction {
            Load(out, Immediate::Nil) => writeln!(self.out, "{pad}{} = NIL;", r(*out)).unwrap(),
            Load(out, Immediate::True) => writeln!(self.out, "{pad}{} = T;", r(*out)).unwrap(),
            Load(out, Immediate::Fixnum(value)) => {
                writeln!(self.out, "{pad}{} = {value};", r(*out)).unwrap();
            }
            Load(out, Immediate::Character(value)) => {
                writeln!(
                    self.out,
                    "{pad}{} = new LispCharacter({});",
                    r(*out),
                    *value as u32
                )
                .unwrap();
            }
            Constant(out, index) => {
                write!(self.out, "{pad}{} = constants[{index}];", r(*out)).unwrap();
                if let Some(label) = constant_label(self.code, self.nodes, *index) {
                    write!(self.out, " // {label}").unwrap();
                }
                self.out.push('\n');
            }
            Copy(out, src) => {
                if out != src {
                    writeln!(self.out, "{pad}{} = {};", r(*out), r(*src)).unwrap();
                }
            }
            Clear(range) => {
                for register in range.clone() {
                    writeln!(self.out, "{pad}{} = NIL;", r(register)).unwrap();
                }
            }
            SetValues(range) => {
                let items = if range.is_empty() {
                    &[]
                } else {
                    &locals[range.clone()]
                };
                if items.len() <= 4 {
                    writeln!(self.out, "{pad}task.values = [{}];", items.join(", ")).unwrap();
                } else {
                    writeln!(self.out, "{pad}task.values = [").unwrap();
                    for item in items {
                        writeln!(self.out, "{pad}  {item},").unwrap();
                    }
                    writeln!(self.out, "{pad}];").unwrap();
                }
            }
            BindValues(range) => {
                for (offset, register) in range.clone().enumerate() {
                    writeln!(
                        self.out,
                        "{pad}{} = task.values[{offset}] ?? NIL;",
                        r(register)
                    )
                    .unwrap();
                }
                writeln!(self.out, "{pad}task.values = [];").unwrap();
            }
            ReadValue(out, index) => {
                writeln!(self.out, "{pad}{} = task.values[{index}] ?? NIL;", r(*out)).unwrap();
            }
            Capture(out, index) => {
                writeln!(self.out, "{pad}{} = captures[{index}];", r(*out)).unwrap();
            }
            OptionalArgument(index, out) => {
                writeln!(
                    self.out,
                    "{pad}{} = frame.arguments[{index}] ?? UNBOUND;",
                    r(*out)
                )
                .unwrap();
            }
            ReleaseArguments => {
                writeln!(
                    self.out,
                    "{pad}frame.arguments = [];\n{pad}frame.keywords = [];"
                )
                .unwrap();
            }
            Call { out, .. } => self.emit_call(pc, *out, pad),
            _ => unreachable!("non-fast instruction reached structured emitter"),
        }
    }

    /// Emit a call that hands control back to the runtime.
    ///
    /// The callee reads its arguments and writes its result through the frame's
    /// register file, so pending local assignments are stored before the call.
    /// A callee that is pushed or transfers control returns there, and
    /// the runtime resumes the frame in the step function at the instruction
    /// after the call. An in-place result only needs to be reloaded when the
    /// structured body also accesses its register directly.
    fn emit_call(&mut self, pc: usize, out: Option<usize>, pad: &str) {
        let operand = self
            .operand_indices
            .get(pc)
            .copied()
            .flatten()
            .expect("call operand index");
        self.emit_register_writeback(pad);
        writeln!(self.out, "{pad}frame.sourcePc = {pc};").unwrap();
        writeln!(self.out, "{pad}frame.pc = {};", pc + 1).unwrap();
        writeln!(self.out, "{pad}resumeEpoch = frame.resumeEpoch;").unwrap();
        let instruction = &self.instructions[pc];
        if let Some(at) = instruction.source_offset() {
            self.map.mark(
                self.out.len() + pad.len(),
                self.source,
                &self.code.debug,
                at,
                None,
            );
        }
        write!(
            self.out,
            "{pad}task.ops.Call(task, frame, operands[{operand}]);"
        )
        .unwrap();
        let label = instruction_label(instruction, self.code, self.nodes);
        if let Some(label) = label {
            write!(self.out, " // {label}").unwrap();
        }
        self.out.push('\n');
        writeln!(
            self.out,
            "{pad}if (task.currentFrame !== frame || task.suspended || task.done) return;"
        )
        .unwrap();
        writeln!(
            self.out,
            "{pad}if (frame.resumeEpoch !== resumeEpoch || frame.pc !== {}) return;",
            pc + 1
        )
        .unwrap();
        if let Some(out) = out.filter(|&register| self.localized & (1 << register) != 0) {
            writeln!(self.out, "{pad}{} = regs[{out}];", self.locals[out]).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use zkcl_bytecode::{Arity, Callee};

    use super::*;

    fn fn_with(instructions: Vec<Instruction>, registers: usize) -> BytecodeFunction {
        BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity {
                required: 1,
                maximum: Some(1),
            },
            registers,
            instructions,
            constants: Vec::new(),
            lambda_list: None,
            captures: Vec::new(),
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        }
    }

    #[test]
    fn pure_loop_and_branch_emit_structured_fast_js_without_pc_switch() {
        let code = fn_with(
            vec![
                Instruction::Load(1, Immediate::Fixnum(0)),
                // pc=1 loop header: unless r0, exit (pc=4)
                Instruction::Unless(0, 4),
                Instruction::Load(0, Immediate::Nil),
                Instruction::Jump(1),
                Instruction::SetValues(1..2),
            ],
            2,
        );
        let mut steps = Steps::default();
        let emission = try_emit_fast_function(&mut steps, &code, "fast_demo", &[], &[], &[], None)
            .expect("loop body is fast-eligible");
        let out = steps.text;
        assert!(emission.checks_budget);
        assert!(out.contains("function fast_demo(task, frame)"));
        assert!(out.contains("while (true)"));
        assert!(!out.contains("switch (frame.pc)"));
    }

    #[test]
    fn a_loop_back_edge_guards_the_budget_and_resumes_at_the_header_pc() {
        // pc=1 is the `Unless` the `Jump(1)` targets, so it is the resume point.
        let code = fn_with(
            vec![
                Instruction::Load(1, Immediate::Fixnum(0)),
                Instruction::Unless(0, 4),
                Instruction::Load(0, Immediate::Nil),
                Instruction::Jump(1),
                Instruction::SetValues(1..2),
            ],
            2,
        );
        let mut steps = Steps::default();
        assert!(
            try_emit_fast_function(&mut steps, &code, "fast_loop", &[], &[], &[], None).is_some(),
            "loop body is fast-eligible"
        );
        let out = steps.text;
        // The guard runs before the jump: it makes the frame authoritative, then
        // records the header's PC so `step` resumes at the loop header.
        assert!(out.contains("if (task.budget-- <= 0) {"));
        assert!(out.contains("regs[0] = r0;\n"));
        assert!(out.contains("frame.pc = 1;"));
        assert!(out.contains("return;\n"));
        assert!(out.contains("continue loop_pc1;"));
    }

    #[test]
    fn special_binding_functions_fall_back_to_step_mode() {
        let code = fn_with(vec![Instruction::WriteCell { symbol: 0, src: 0 }], 1);
        let mut steps = Steps::default();
        assert!(
            try_emit_fast_function(&mut steps, &code, "fast_call", &[], &[], &[], None).is_none(),
            "special binding falls back to step mode"
        );
        assert!(steps.text.is_empty());
    }

    #[test]
    fn calls_flush_registers_and_resume_through_the_step_function() {
        let code = fn_with(
            vec![
                Instruction::Load(1, Immediate::Fixnum(1)),
                Instruction::Call {
                    callee: Callee::Named(0),
                    out: Some(0),
                    args: 1..2,
                    at: 0,
                    associate: false,
                    notinline: false,
                },
                Instruction::SetValues(0..1),
            ],
            2,
        );
        let mut steps = Steps::default();
        let emission = try_emit_fast_function(
            &mut steps,
            &code,
            "fast_caller",
            &[],
            &[None, Some(0), None],
            &[],
            None,
        )
        .expect("call-only body is fast-eligible");
        let out = steps.text;
        // No loop means no back edge to guard, but the body is still safe under a
        // finite budget: every path returns to the runtime.
        assert!(emission.checks_budget);
        assert!(!out.contains("task.budget--"));
        assert!(out.contains("const operands = frame.code.operands;"));
        assert!(!out.contains("regs[0] = r0;"));
        assert!(out.contains("regs[1] = r1;"));
        assert!(out.contains("frame.sourcePc = 1;"));
        assert!(out.contains("frame.pc = 2;"));
        assert!(out.contains("task.ops.Call(task, frame, operands[0]);"));
        assert!(out.contains("if (frame.resumeEpoch !== resumeEpoch || frame.pc !== 2) return;"));
        assert!(out.contains("\n  r0 = regs[0];\n"));
    }
}
