//! Structured JavaScript lowering for non-suspending pure bytecode functions.
//!
//! Functions whose instructions never call Lisp, suspend, bind specials, or
//! establish dynamic handlers are lowered via [`zkcl_opt::structure_graph`]
//! directly into labeled JS blocks/loops with lexical register variables,
//! eliminating the `switch (frame.pc)` dispatch loop, `frame.registers` heap
//! reads/writes, and per-instruction `task.budget` checks.

use std::collections::BTreeSet;
use std::fmt::Write;

use zkcl_bytecode::{BytecodeFunction, DebugVariableLocation, Immediate, Instruction, LiteralNode};
use zkcl_opt::{StructuredCfg, StructuredRegion, structure_graph};
use zkcl_target::CompilationTarget;

const MAX_FAST_REGISTERS: usize = 64;

/// Emit a structured `function {name}(task, frame)` fast path when `code` is
/// purely non-suspending and within the register/range unrolling budget.
pub(super) fn try_emit_fast_function(
    steps: &mut String,
    code: &BytecodeFunction,
    name: &str,
    nodes: &[LiteralNode],
) -> bool {
    let Some(summary) = analyze_fast_eligibility(code) else {
        return false;
    };

    let (blocks, rpo, exit_block) = build_basic_blocks(&code.instructions);
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

    emit_header(steps, code, name, &summary, &flow, nodes);
    let mut emitter = StructuredEmitter {
        out: steps,
        instructions: &code.instructions,
        blocks: &blocks,
        exit_block,
        flow: &flow,
        active_loops: Vec::new(),
    };
    emitter.emit_region_list(&flow.regions, 1);
    emitter.out.push_str("}\n\n");
    true
}

struct FastSummary {
    reg_count: usize,
    needs_nil: bool,
    needs_t: bool,
    needs_unbound: bool,
    needs_char: bool,
    needs_constants: bool,
    needs_captures: bool,
}

impl FastSummary {
    fn note_reg(&mut self, reg: usize) -> Option<()> {
        if reg >= MAX_FAST_REGISTERS {
            return None;
        }
        self.reg_count = self.reg_count.max(reg + 1);
        Some(())
    }

    fn note_range(&mut self, range: &std::ops::Range<usize>) -> Option<()> {
        if range.len() > MAX_FAST_REGISTERS {
            return None;
        }
        if let Some(last) = range.end.checked_sub(1)
            && range.start <= last
        {
            self.note_reg(last)?;
        }
        Some(())
    }
}

fn analyze_fast_eligibility(code: &BytecodeFunction) -> Option<FastSummary> {
    if code.instructions.is_empty() || code.registers > MAX_FAST_REGISTERS {
        return None;
    }

    let n = code.instructions.len();
    let mut summary = FastSummary {
        reg_count: code.registers.max(code.arity.required),
        needs_nil: false,
        needs_t: false,
        needs_unbound: false,
        needs_char: false,
        needs_constants: false,
        needs_captures: false,
    };

    for instruction in &code.instructions {
        use Instruction::*;
        match instruction {
            Load(out, imm) => {
                summary.note_reg(*out)?;
                match imm {
                    Immediate::Nil => summary.needs_nil = true,
                    Immediate::True => summary.needs_t = true,
                    Immediate::Fixnum(_) => {}
                    Immediate::Character(_) => summary.needs_char = true,
                }
            }
            Constant(out, _) => {
                summary.note_reg(*out)?;
                summary.needs_constants = true;
            }
            Copy(out, src) => {
                summary.note_reg(*out)?;
                summary.note_reg(*src)?;
            }
            Clear(range) => {
                summary.note_range(range)?;
                if !range.is_empty() {
                    summary.needs_nil = true;
                }
            }
            Jump(target) => {
                if *target > n {
                    return None;
                }
            }
            Unless(reg, target) => {
                summary.note_reg(*reg)?;
                if *target > n {
                    return None;
                }
                summary.needs_nil = true;
            }
            IfPresent(reg, target) => {
                summary.note_reg(*reg)?;
                if *target > n {
                    return None;
                }
                summary.needs_unbound = true;
            }
            UnlessFixnum(reg, target) | UnlessFixnumInRange(reg, _, _, target) => {
                summary.note_reg(*reg)?;
                if *target > n {
                    return None;
                }
            }
            SetValues(range) => {
                // The register limit also bounds the number of returned values.
                summary.note_range(range)?;
            }
            BindValues(range) => {
                summary.note_range(range)?;
                if !range.is_empty() {
                    summary.needs_nil = true;
                }
            }
            ReadValue(out, _) => {
                summary.note_reg(*out)?;
                summary.needs_nil = true;
            }
            Capture(out, _) => {
                summary.note_reg(*out)?;
                summary.needs_captures = true;
            }
            OptionalArgument(_, out) => {
                summary.note_reg(*out)?;
                summary.needs_unbound = true;
            }
            ReleaseArguments => {}
            _ => return None,
        }
    }

    if code.registers < summary.reg_count {
        summary.needs_nil = true;
    }
    Some(summary)
}

struct BasicBlockInfo {
    start_pc: usize,
    end_pc: usize,
    succs: Vec<usize>,
}

fn branch_target(instruction: &Instruction) -> Option<(usize, bool)> {
    match *instruction {
        Instruction::Jump(target) => Some((target, false)),
        Instruction::Unless(_, target)
        | Instruction::IfPresent(_, target)
        | Instruction::UnlessFixnum(_, target)
        | Instruction::UnlessFixnumInRange(_, _, _, target) => Some((target, true)),
        _ => None,
    }
}

fn build_basic_blocks(instructions: &[Instruction]) -> (Vec<BasicBlockInfo>, Vec<usize>, usize) {
    let n = instructions.len();
    let mut leaders = vec![false; n];
    leaders[0] = true;
    for (pc, instruction) in instructions.iter().enumerate() {
        if let Some((target, _)) = branch_target(instruction) {
            if target < n {
                leaders[target] = true;
            }
            if pc + 1 < n {
                leaders[pc + 1] = true;
            }
        }
    }

    let starts: Vec<usize> = leaders
        .iter()
        .enumerate()
        .filter_map(|(pc, &is_leader)| is_leader.then_some(pc))
        .collect();
    let exit_block = starts.len();
    let block_for_pc = |pc: usize| -> usize {
        if pc == n {
            exit_block
        } else {
            starts.binary_search(&pc).expect("leader PC")
        }
    };

    let mut blocks = Vec::with_capacity(starts.len() + 1);
    for (idx, &start_pc) in starts.iter().enumerate() {
        let end_pc = starts.get(idx + 1).copied().unwrap_or(n);
        let last = &instructions[end_pc - 1];
        let succs = match branch_target(last) {
            Some((target, false)) => vec![block_for_pc(target)],
            Some((target, true)) => {
                let taken = block_for_pc(target);
                let fall = block_for_pc(end_pc);
                if taken == fall {
                    vec![taken]
                } else {
                    vec![taken, fall]
                }
            }
            None => vec![block_for_pc(end_pc)],
        };
        blocks.push(BasicBlockInfo {
            start_pc,
            end_pc,
            succs,
        });
    }
    blocks.push(BasicBlockInfo {
        start_pc: n,
        end_pc: n,
        succs: Vec::new(),
    });

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

fn emit_header(
    out: &mut String,
    code: &BytecodeFunction,
    name: &str,
    summary: &FastSummary,
    flow: &StructuredCfg,
    nodes: &[LiteralNode],
) {
    writeln!(out, "function {name}(task, frame) {{").unwrap();
    let mut objects = BTreeSet::new();
    if summary.needs_nil {
        objects.insert("NIL");
    }
    if summary.needs_t {
        objects.insert("T");
    }
    if summary.needs_unbound {
        objects.insert("UNBOUND");
    }
    if summary.needs_char {
        objects.insert("LispCharacter");
    }
    if !objects.is_empty() {
        let names: Vec<_> = objects.into_iter().collect();
        writeln!(out, "  const {{ {} }} = task.runtime.objects;", names.join(", ")).unwrap();
    }
    if summary.needs_constants {
        out.push_str("  const constants = frame.constants;\n");
    }
    if summary.needs_captures {
        out.push_str("  const captures = frame.captures;\n");
    }
    if summary.reg_count > 0 {
        if code.registers > 0 {
            out.push_str("  const regs = frame.registers;\n");
        }
        let names = required_parameter_names(code, nodes);
        for (r, name) in names.into_iter().take(summary.reg_count).enumerate() {
            if r < code.registers {
                write!(out, "  let r{r} = regs[{r}];").unwrap();
                if let Some(name) = name {
                    out.push_str(" // parameter ");
                    crate::emit::write_symbol_component(out, name);
                }
                out.push('\n');
            } else {
                writeln!(out, "  let r{r} = NIL;").unwrap();
            }
        }
    }
    if flow.dispatch.iter().any(Option::is_some) {
        out.push_str("  let __d = 0;\n");
    }
}

fn required_parameter_names<'a>(
    code: &BytecodeFunction,
    nodes: &'a [LiteralNode],
) -> [Option<&'a str>; MAX_FAST_REGISTERS] {
    let mut names = [None; MAX_FAST_REGISTERS];
    let mut seen = [false; MAX_FAST_REGISTERS];
    for variable in &code.debug.variables {
        let DebugVariableLocation::Register { index, boxed } = variable.location else {
            continue;
        };
        if index >= code.arity.required {
            continue;
        }
        if std::mem::replace(&mut seen[index], true)
            || boxed
            || variable.live != (0..code.instructions.len())
        {
            names[index] = None;
            continue;
        }
        match &nodes[code.constants[variable.name as usize] as usize] {
            LiteralNode::InternedSymbol { name, .. } | LiteralNode::UninternedSymbol(name) => {
                names[index] = Some(name.as_str());
            }
            _ => {}
        }
    }
    names
}

struct StructuredEmitter<'a> {
    out: &'a mut String,
    instructions: &'a [Instruction],
    blocks: &'a [BasicBlockInfo],
    exit_block: usize,
    flow: &'a StructuredCfg,
    active_loops: Vec<usize>,
}

impl StructuredEmitter<'_> {
    fn emit_region_list(&mut self, regions: &[StructuredRegion], indent: usize) {
        let Some((first, rest)) = regions.split_first() else {
            return;
        };
        let break_targets = regions
            .iter()
            .all(|region| matches!(region, StructuredRegion::Block(_)))
            .then(|| {
                // In a flat list, only edges that skip the next block need a break.
                let mut targets = vec![false; self.blocks.len()];
                for (index, region) in regions.iter().enumerate() {
                    let next = regions.get(index + 1).map(StructuredRegion::entry);
                    for &target in &self.blocks[region.entry()].succs {
                        if Some(target) != next {
                            targets[target] = true;
                        }
                    }
                }
                targets
            });
        let needs_label = |region: &StructuredRegion| {
            break_targets
                .as_ref()
                .is_none_or(|targets| targets[region.entry()])
        };
        // Breaking out of a label enters the corresponding later region.
        let mut body_indent = indent;
        for region in rest.iter().rev().filter(|region| needs_label(region)) {
            writeln!(
                self.out,
                "{}b{}: {{",
                indent_str(body_indent),
                region.entry()
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

    fn emit_region(&mut self, region: &StructuredRegion, indent: usize, next: Option<usize>) {
        match region {
            StructuredRegion::Block(block) => self.emit_block(*block, indent, next),
            StructuredRegion::Loop { header, body } => {
                let pad = indent_str(indent);
                writeln!(self.out, "{pad}l{header}: while (true) {{").unwrap();
                self.active_loops.push(*header);
                for (offset, child) in body.iter().rev().enumerate() {
                    writeln!(
                        self.out,
                        "{}b{}: {{",
                        indent_str(indent + 1 + offset),
                        child.entry()
                    )
                    .unwrap();
                }
                let header_indent = indent + 1 + body.len();
                self.emit_block(*header, header_indent, body.first().map(StructuredRegion::entry));
                for (idx, child) in body.iter().enumerate() {
                    let close_indent = indent + 1 + (body.len() - 1 - idx);
                    writeln!(self.out, "{}}}", indent_str(close_indent)).unwrap();
                    let child_indent = indent + 1 + (body.len() - 1 - idx);
                    let child_next = body.get(idx + 1).map(StructuredRegion::entry);
                    self.emit_region(child, child_indent, child_next);
                }
                self.active_loops.pop();
                writeln!(self.out, "{pad}}}").unwrap();
            }
            StructuredRegion::Dispatch(members) => {
                let entry = members[0];
                let pad = indent_str(indent);
                writeln!(self.out, "{pad}l{entry}: while (true) {{").unwrap();
                writeln!(self.out, "{pad}  switch (__d) {{").unwrap();
                self.active_loops.push(entry);
                for (case, &member) in members.iter().enumerate() {
                    writeln!(self.out, "{pad}    case {case}: {{").unwrap();
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
        let last = &self.instructions[info.end_pc - 1];
        let body_end = info.end_pc - usize::from(branch_target(last).is_some());
        for instruction in &self.instructions[info.start_pc..body_end] {
            self.emit_statement(instruction, &pad);
        }
        let [taken, fall] = info.succs.as_slice() else {
            self.emit_jump_line(info.succs[0], &pad, next);
            return;
        };
        let test = branch_test_expr(last);
        let taken_code = self.format_jump_stmts(*taken, next);
        let fall_code = self.format_jump_stmts(*fall, next);
        match (taken_code.is_empty(), fall_code.is_empty()) {
            (false, false) => {
                writeln!(self.out, "{pad}if ({test}) {{").unwrap();
                for line in taken_code {
                    writeln!(self.out, "{pad}  {line}").unwrap();
                }
                writeln!(self.out, "{pad}}} else {{").unwrap();
                for line in fall_code {
                    writeln!(self.out, "{pad}  {line}").unwrap();
                }
                writeln!(self.out, "{pad}}}").unwrap();
            }
            (false, true) => {
                writeln!(self.out, "{pad}if ({test}) {{").unwrap();
                for line in taken_code {
                    writeln!(self.out, "{pad}  {line}").unwrap();
                }
                writeln!(self.out, "{pad}}}").unwrap();
            }
            (true, false) => {
                writeln!(self.out, "{pad}if (!({test})) {{").unwrap();
                for line in fall_code {
                    writeln!(self.out, "{pad}  {line}").unwrap();
                }
                writeln!(self.out, "{pad}}}").unwrap();
            }
            (true, true) => {}
        }
    }

    fn emit_jump_line(&mut self, target: usize, pad: &str, next: Option<usize>) {
        for line in self.format_jump_stmts(target, next) {
            writeln!(self.out, "{pad}{line}").unwrap();
        }
    }

    fn format_jump_stmts(&self, target: usize, next: Option<usize>) -> Vec<String> {
        let mut stmts = Vec::new();
        let anchor = if let Some((entry, case)) = self.flow.dispatch[target] {
            stmts.push(format!("__d = {case};"));
            entry
        } else {
            target
        };
        if Some(anchor) == next {
            return stmts;
        }
        if self.active_loops.contains(&anchor) {
            stmts.push(format!("continue l{anchor};"));
        } else {
            stmts.push(format!("break b{anchor};"));
        }
        stmts
    }

    fn emit_statement(&mut self, instruction: &Instruction, pad: &str) {
        use Instruction::*;
        match instruction {
            Load(out, Immediate::Nil) => writeln!(self.out, "{pad}r{out} = NIL;").unwrap(),
            Load(out, Immediate::True) => writeln!(self.out, "{pad}r{out} = T;").unwrap(),
            Load(out, Immediate::Fixnum(value)) => {
                writeln!(self.out, "{pad}r{out} = {value};").unwrap();
            }
            Load(out, Immediate::Character(value)) => {
                writeln!(self.out, "{pad}r{out} = new LispCharacter({});", *value as u32).unwrap();
            }
            Constant(out, index) => {
                writeln!(self.out, "{pad}r{out} = constants[{index}];").unwrap();
            }
            Copy(out, src) => {
                if out != src {
                    writeln!(self.out, "{pad}r{out} = r{src};").unwrap();
                }
            }
            Clear(range) => {
                for r in range.clone() {
                    writeln!(self.out, "{pad}r{r} = NIL;").unwrap();
                }
            }
            SetValues(range) => {
                let items: Vec<_> = range.clone().map(|r| format!("r{r}")).collect();
                writeln!(self.out, "{pad}task.values = [{}];", items.join(", ")).unwrap();
            }
            BindValues(range) => {
                for (offset, r) in range.clone().enumerate() {
                    writeln!(self.out, "{pad}r{r} = task.values[{offset}] ?? NIL;").unwrap();
                }
                writeln!(self.out, "{pad}task.values = [];").unwrap();
            }
            ReadValue(out, index) => {
                writeln!(self.out, "{pad}r{out} = task.values[{index}] ?? NIL;").unwrap();
            }
            Capture(out, index) => {
                writeln!(self.out, "{pad}r{out} = captures[{index}];").unwrap();
            }
            OptionalArgument(index, out) => {
                writeln!(
                    self.out,
                    "{pad}r{out} = frame.arguments[{index}] ?? UNBOUND;"
                )
                .unwrap();
            }
            ReleaseArguments => {
                writeln!(self.out, "{pad}frame.arguments = [];\n{pad}frame.keywords = [];")
                    .unwrap();
            }
            _ => unreachable!("non-fast instruction reached structured emitter"),
        }
    }
}

fn branch_test_expr(instruction: &Instruction) -> String {
    match instruction {
        Instruction::Unless(reg, _) => format!("r{reg} === NIL"),
        Instruction::IfPresent(reg, _) => format!("r{reg} !== UNBOUND"),
        Instruction::UnlessFixnum(reg, _) => {
            let (low, high) = CompilationTarget::javascript().fixnum_bounds();
            format!("typeof r{reg} !== 'number' || !Number.isInteger(r{reg}) || r{reg} < {low} || r{reg} > {high}")
        }
        Instruction::UnlessFixnumInRange(reg, low, high, _) => {
            format!("typeof r{reg} !== 'number' || !Number.isInteger(r{reg}) || r{reg} < {low} || r{reg} > {high}")
        }
        _ => unreachable!("non-branch instruction queried for branch test"),
    }
}

fn indent_str(level: usize) -> String {
    "  ".repeat(level)
}

#[cfg(test)]
mod tests {
    use zkcl_bytecode::Arity;

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
        let mut out = String::new();
        assert!(try_emit_fast_function(&mut out, &code, "fast_demo", &[]));
        assert!(out.contains("function fast_demo(task, frame)"));
        assert!(out.contains("while (true)"));
        assert!(out.contains("continue l"));
        assert!(!out.contains("switch (frame.pc)"));
        assert!(!out.contains("task.budget"));
    }

    #[test]
    fn suspending_or_call_functions_fall_back_to_step_mode() {
        let code = fn_with(
            vec![Instruction::WriteCell { symbol: 0, src: 0 }],
            1,
        );
        let mut out = String::new();
        assert!(!try_emit_fast_function(&mut out, &code, "fast_call", &[]));
        assert!(out.is_empty());
    }
}
