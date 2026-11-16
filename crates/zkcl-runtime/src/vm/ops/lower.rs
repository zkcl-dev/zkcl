use std::ops::Range;

use zkcl_bytecode::{
    Callee, CellLocation, HandlerTable, HandlerTarget, Immediate, Instruction, JumpTable, Register,
    RestartTable, RestartTarget, tail_call_sites,
};

use super::{
    ArithKind, CanonicalSymbol, CmpKind, Lowered, LoweredCode, PcMapping, VmOp,
    remap_branch_targets,
};

#[expect(
    clippy::too_many_arguments,
    reason = "lowering borrows the verified instructions and control tables independently"
)]
pub(crate) fn lower_code(
    instructions: &[Instruction],
    registers: usize,
    _captures_len: usize,
    canonical_symbols: &[Option<CanonicalSymbol>],
    handlers: &[HandlerTable],
    restarts: &[RestartTable],
    jump_tables_in: &[JumpTable],
    retain_frame: bool,
) -> LoweredCode {
    let mut ctx = VmLowerer {
        instructions,
        canonical_symbols,
        handlers,
        restarts,
        jump_tables: jump_tables_in,
        jump_targets: None,
        allow_spec: !cfg!(feature = "vm-no-spec") && registers <= u16::MAX as usize,
    };

    let len = instructions.len();
    let mut vm_ops = Vec::with_capacity(len);
    let mut pc_mapping = None;
    let mut pc = 0usize;
    let mut tails = tail_call_sites(instructions).peekable();
    while pc < len {
        let vm_pc = vm_ops.len() as u32;
        let lowered = if !retain_frame && tails.next_if_eq(&pc).is_some() {
            ctx.lower_tail_call(pc)
        } else {
            ctx.lower_instruction(pc)
        };
        let next = pc + lowered.consumed as usize;
        if lowered.consumed != 1 || pc_mapping.is_some() {
            let (logical_pcs, pc_map) = pc_mapping.get_or_insert_with(|| {
                // Canonical and VM PCs agree up to the first fused instruction.
                let mut logical_pcs = Vec::with_capacity(len);
                logical_pcs.extend(0..vm_pc);
                let mut pc_map = vec![0; len + 1];
                for (target, vm_pc) in pc_map[..pc].iter_mut().zip(0..vm_pc) {
                    *target = vm_pc;
                }
                (logical_pcs, pc_map)
            });
            let logical_pc = if matches!(
                lowered.op,
                VmOp::AddFixnumImm { .. } | VmOp::SubFixnumImm { .. } | VmOp::MulFixnumImm { .. }
            ) {
                pc + 1
            } else {
                pc
            };
            logical_pcs.push(logical_pc as u32);
            pc_map[pc] = vm_pc;
            pc_map[pc + 1..next].fill((vm_ops.len() + 1) as u32);
            // Deoptimization at a fused arithmetic call must still execute the call.
            pc_map[logical_pc] = vm_pc;
            if matches!(lowered.op, VmOp::ClearBackedge { .. }) {
                // Replaying the preceding clear on a resume at Jump is idempotent.
                pc_map[pc + 1] = vm_pc;
            }
        }
        vm_ops.push(lowered.op);
        pc = next;
    }
    let pc_mapping = pc_mapping.map(|(logical_pcs, mut pc_map)| {
        pc_map[len] = vm_ops.len() as u32;
        for op in &mut vm_ops {
            remap_branch_targets(op, &pc_map);
        }
        PcMapping {
            canonical_by_vm: logical_pcs.into_boxed_slice(),
            vm_by_canonical: pc_map.into_boxed_slice(),
        }
    });
    if ctx.allow_spec {
        coalesce_clear_pairs(&mut vm_ops);
    }

    // Canonical code comes from the compiler or the FASL verifier. Check the
    // generated representation during development to catch selector bugs.
    #[cfg(any(debug_assertions, test))]
    super::verify_vm_ops(
        &vm_ops,
        pc_mapping.as_ref().map(|map| map.canonical_by_vm.as_ref()),
        registers,
        canonical_symbols.len(),
        _captures_len,
        len,
    )
    .expect("vm op invariant violated after lowering");

    LoweredCode {
        vm_ops: vm_ops.into_boxed_slice(),
        pc_mapping,
    }
}

fn coalesce_clear_pairs(vm_ops: &mut [VmOp]) {
    let mut ops = vm_ops.iter_mut().peekable();
    while let Some(first) = ops.next() {
        let VmOp::Clear { start, len } = *first else {
            continue;
        };
        let Some(VmOp::Clear {
            start: next_start,
            len: next_len,
        }) = ops.peek().map(|op| **op)
        else {
            continue;
        };
        let end = usize::from(start) + usize::from(len);
        let next_end = usize::from(next_start) + usize::from(next_len);
        let union_start = start.min(next_start);
        if usize::from(start) <= next_end
            && usize::from(next_start) <= end
            && let Ok(union_len) = u16::try_from(end.max(next_end) - usize::from(union_start))
        {
            // Leave the second clear addressable by its original PC.
            *first = VmOp::ClearPair {
                start,
                len,
                union_start,
                union_len,
            };
            let _ = ops.next();
        }
    }
}

enum NamedCallKind {
    Compare(CmpKind),
    Arithmetic(ArithKind),
    Null,
    Car,
    Cdr,
    Cons,
    Other,
}

/// Branch and optional register clear following a named call.
struct BranchSpan {
    target: u32,
    clear_start: u16,
    clear_len: u16,
    consumed: u8,
}

struct VmLowerer<'a> {
    instructions: &'a [Instruction],
    canonical_symbols: &'a [Option<CanonicalSymbol>],
    handlers: &'a [HandlerTable],
    restarts: &'a [RestartTable],
    jump_tables: &'a [JumpTable],
    jump_targets: Option<Vec<bool>>,
    allow_spec: bool,
}

impl VmLowerer<'_> {
    fn lower_instruction(&mut self, site: usize) -> Lowered {
        self.select_instruction(site).unwrap_or(Lowered {
            op: VmOp::Slow { pc: site as u32 },
            consumed: 1,
        })
    }

    fn select_instruction(&mut self, site: usize) -> Option<Lowered> {
        let op = match &self.instructions[site] {
            Instruction::Call {
                callee,
                out,
                args,
                associate: false,
                notinline,
                ..
            } if self.allow_spec => {
                return match (callee, out) {
                    (Callee::Named(symbol), Some(out)) if args.len() <= 3 => {
                        self.lower_named_call(site, *symbol, *out, args, *notinline)
                    }
                    _ => None,
                };
            }
            Instruction::Load(out, Immediate::Nil) => VmOp::LoadNil {
                out: self.reg_u16(*out)?,
            },
            Instruction::Load(out, Immediate::True) => VmOp::LoadTrue {
                out: self.reg_u16(*out)?,
            },
            Instruction::Load(out, Immediate::Fixnum(value)) => {
                if self.allow_spec
                    && let Some(lowered) = self.lower_literal(site)
                {
                    return Some(lowered);
                }
                VmOp::LoadFixnum {
                    out: self.reg_u16(*out)?,
                    value: *value,
                }
            }
            Instruction::Load(out, Immediate::Character(ch)) => VmOp::LoadChar {
                out: self.reg_u16(*out)?,
                ch: *ch,
            },
            Instruction::Constant(out, idx) => VmOp::Constant {
                out: self.reg_u16(*out)?,
                idx: u32::try_from(*idx).ok()?,
            },
            Instruction::Copy(out, src) => VmOp::Copy {
                out: self.reg_u16(*out)?,
                src: self.reg_u16(*src)?,
            },
            Instruction::Clear(regs) => {
                if self.allow_spec
                    && let Some(lowered) = self.lower_clear(site)
                {
                    return Some(lowered);
                }
                VmOp::Clear {
                    start: self.reg_u16(regs.start)?,
                    len: u16::try_from(regs.len()).ok()?,
                }
            }
            Instruction::Jump(pc) => VmOp::Jump {
                pc: u32::try_from(*pc).ok()?,
            },
            Instruction::Unless(test, pc) => VmOp::Unless {
                test: self.reg_u16(*test)?,
                pc: u32::try_from(*pc).ok()?,
            },
            Instruction::IfPresent(reg, pc) => VmOp::IfPresent {
                reg: self.reg_u16(*reg)?,
                pc: u32::try_from(*pc).ok()?,
            },
            Instruction::UnlessFixnum(reg, pc) => VmOp::UnlessFixnum {
                reg: self.reg_u16(*reg)?,
                pc: u32::try_from(*pc).ok()?,
            },
            Instruction::UnlessFixnumInRange(reg, low, high, pc) => VmOp::UnlessFixnumInRange {
                reg: self.reg_u16(*reg)?,
                low: *low,
                high: *high,
                pc: u32::try_from(*pc).ok()?,
            },
            Instruction::TryFixnumArithmetic {
                op,
                symbol,
                out,
                args: regs,
            } => VmOp::TryFixnumArithmetic {
                op: *op,
                symbol: u32::try_from(*symbol).ok()?,
                out: self.reg_u16(*out)?,
                start: self.reg_u16(regs.start)?,
                len: u16::try_from(regs.len()).ok()?,
            },
            Instruction::SetValues(regs) if regs.len() == 1 => VmOp::SetValues1 {
                src: self.reg_u16(regs.start)?,
            },
            Instruction::SetValues(regs) => VmOp::SetValues {
                start: self.reg_u16(regs.start)?,
                len: u16::try_from(regs.len()).ok()?,
            },
            Instruction::BindValues(regs) if regs.len() == 1 => VmOp::BindValues1 {
                out: self.reg_u16(regs.start)?,
            },
            Instruction::BindValues(regs) => VmOp::BindValues {
                start: self.reg_u16(regs.start)?,
                len: u16::try_from(regs.len()).ok()?,
            },
            Instruction::OptionalArgument(index, out) => VmOp::OptionalArgument {
                index: u32::try_from(*index).ok()?,
                out: self.reg_u16(*out)?,
            },
            Instruction::KeywordArgument(index, out) => VmOp::KeywordArgument {
                index: u32::try_from(*index).ok()?,
                out: self.reg_u16(*out)?,
            },
            Instruction::ReleaseArguments => VmOp::ReleaseArguments {},
            Instruction::ReadCell {
                out,
                symbol: sym,
                function,
                ..
            } => VmOp::ReadCell {
                out: self.reg_u16(*out)?,
                sym: u32::try_from(*sym).ok()?,
                function: *function,
            },
            Instruction::WriteCell { symbol: sym, src } => VmOp::WriteCell {
                sym: u32::try_from(*sym).ok()?,
                src: self.reg_u16(*src)?,
            },
            Instruction::BindSpecial { symbol, value } => VmOp::BindSpecial {
                symbol: u32::try_from(*symbol).ok()?,
                value: self.reg_u16(*value)?,
            },
            Instruction::Unbind(count) => VmOp::Unbind {
                count: u16::try_from(*count).ok()?,
            },
            Instruction::BoxLocal { reg, .. } => VmOp::BoxLocal {
                reg: self.reg_u16(*reg)?,
            },
            Instruction::Closure {
                out,
                code,
                cells: regs,
                ..
            } => VmOp::Closure {
                out: self.reg_u16(*out)?,
                code: u32::try_from(*code).ok()?,
                start: self.reg_u16(regs.start)?,
                len: u16::try_from(regs.len()).ok()?,
            },
            Instruction::ReadBox {
                out,
                cell: CellLocation::Local(cell),
            } => VmOp::ReadBoxLocal {
                out: self.reg_u16(*out)?,
                cell: self.reg_u16(*cell)?,
            },
            Instruction::Capture(out, capture) => VmOp::Capture {
                out: self.reg_u16(*out)?,
                capture: u16::try_from(*capture).ok()?,
            },
            Instruction::ReadBox {
                out,
                cell: CellLocation::Capture(capture),
            } => VmOp::ReadBoxCapture {
                out: self.reg_u16(*out)?,
                capture: u16::try_from(*capture).ok()?,
            },
            Instruction::WriteBox {
                cell: CellLocation::Local(cell),
                src,
            } => VmOp::WriteBoxLocal {
                cell: self.reg_u16(*cell)?,
                src: self.reg_u16(*src)?,
            },
            Instruction::WriteBox {
                cell: CellLocation::Capture(capture),
                src,
            } => VmOp::WriteBoxCapture {
                capture: u16::try_from(*capture).ok()?,
                src: self.reg_u16(*src)?,
            },
            _ => return None,
        };
        Some(Lowered { op, consumed: 1 })
    }

    fn lower_literal(&mut self, site: usize) -> Option<Lowered> {
        let (kind, out, src, literal, imm, sym) = self.arith_imm(site)?;
        let op = match kind {
            ArithKind::Add => VmOp::AddFixnumImm {
                out,
                src,
                literal,
                imm,
                sym,
            },
            ArithKind::Sub => VmOp::SubFixnumImm {
                out,
                src,
                literal,
                imm,
                sym,
            },
            ArithKind::Mul => VmOp::MulFixnumImm {
                out,
                src,
                literal,
                imm,
                sym,
            },
        };
        Some(Lowered { op, consumed: 2 })
    }

    fn lower_clear(&mut self, site: usize) -> Option<Lowered> {
        let (start, len, target) = self.clear_backedge(site)?;
        Some(Lowered {
            op: VmOp::ClearBackedge { start, len, target },
            consumed: 2,
        })
    }

    fn lower_tail_call(&self, site: usize) -> Lowered {
        let canonical = VmOp::TailCallCanonical { pc: site as u32 };
        let op = match &self.instructions[site] {
            Instruction::Call { callee, args, .. } if self.allow_spec => {
                match (self.reg_u16(args.start), u16::try_from(args.len())) {
                    (Some(start), Ok(argc)) => match *callee {
                        Callee::Named(symbol) => u32::try_from(symbol)
                            .ok()
                            .map_or(canonical, |sym| VmOp::TailCallNamed { sym, start, argc }),
                        Callee::Value(reg) => {
                            self.reg_u16(reg)
                                .map_or(canonical, |callee| VmOp::TailCallValue {
                                    callee,
                                    start,
                                    argc,
                                })
                        }
                    },
                    _ => canonical,
                }
            }
            _ => canonical,
        };
        Lowered { op, consumed: 1 }
    }

    fn lower_named_call(
        &mut self,
        site: usize,
        symbol: usize,
        out: Register,
        args: &Range<Register>,
        notinline: bool,
    ) -> Option<Lowered> {
        let kind = if notinline || !matches!(args.len(), 1 | 2) {
            NamedCallKind::Other
        } else {
            match self.canonical_symbols.get(symbol).copied().flatten() {
                Some(CanonicalSymbol::Null) => NamedCallKind::Null,
                Some(CanonicalSymbol::Car) => NamedCallKind::Car,
                Some(CanonicalSymbol::Cdr) => NamedCallKind::Cdr,
                Some(CanonicalSymbol::Cons) => NamedCallKind::Cons,
                Some(primitive) => {
                    if let Some(kind) = primitive.cmp_kind() {
                        NamedCallKind::Compare(kind)
                    } else if let Some(kind) = primitive.arith_kind() {
                        NamedCallKind::Arithmetic(kind)
                    } else {
                        NamedCallKind::Other
                    }
                }
                None => NamedCallKind::Other,
            }
        };

        let out = self.reg_u16(out)?;
        let sym = u32::try_from(symbol).ok()?;
        let first = self.reg_u16(args.start)?;

        match (args.len(), kind) {
            (2, NamedCallKind::Compare(cmp)) => {
                if let Ok(sym) = u16::try_from(sym)
                    && let Some(span) = self.branch_span(site, out)
                {
                    return Some(Lowered {
                        op: VmOp::CmpBranch {
                            kind: cmp,
                            a: first,
                            sym,
                            out,
                            target: span.target,
                            clear_start: span.clear_start,
                            clear_len: span.clear_len,
                        },
                        consumed: span.consumed,
                    });
                }
            }
            (1, NamedCallKind::Null) => {
                if let Ok(sym) = u16::try_from(sym)
                    && let Some(span) = self.branch_span(site, out)
                {
                    return Some(Lowered {
                        op: VmOp::NullBranch {
                            reg: first,
                            sym,
                            out,
                            target: span.target,
                            clear_start: span.clear_start,
                            clear_len: span.clear_len,
                        },
                        consumed: span.consumed,
                    });
                }
            }
            (1, NamedCallKind::Car) => {
                return Some(Lowered {
                    op: VmOp::CarFast {
                        out,
                        src: first,
                        sym,
                    },
                    consumed: 1,
                });
            }
            (1, NamedCallKind::Cdr) => {
                return Some(Lowered {
                    op: VmOp::CdrFast {
                        out,
                        src: first,
                        sym,
                    },
                    consumed: 1,
                });
            }
            (2, NamedCallKind::Cons) => {
                return Some(Lowered {
                    op: VmOp::ConsFast { out, first, sym },
                    consumed: 1,
                });
            }
            (2, NamedCallKind::Arithmetic(arith)) => {
                return Some(Lowered {
                    op: VmOp::ArithFixnumGuarded {
                        kind: arith,
                        out,
                        first,
                        sym,
                    },
                    consumed: 1,
                });
            }
            _ => {}
        }

        let op = match args.len() {
            0 => VmOp::CallNamed0 { out, sym },
            1 => VmOp::CallNamed1 {
                out,
                sym,
                arg0: first,
            },
            2 => VmOp::CallNamed2 {
                out,
                sym,
                arg_start: first,
            },
            3 => VmOp::CallNamed3 {
                out,
                sym,
                arg_start: first,
            },
            // Keyword-heavy and variadic calls carry their count instead of
            // getting one variant per arity.
            argc => VmOp::CallNamedVariadic {
                out,
                sym,
                arg_start: first,
                argc: u16::try_from(argc).ok()?,
            },
        };
        Some(Lowered { op, consumed: 1 })
    }

    fn jump_targets(&mut self) -> &[bool] {
        self.jump_targets.get_or_insert_with(|| {
            let mut targets = vec![false; self.instructions.len() + 1];
            for inst in self.instructions {
                let target = match inst {
                    Instruction::Jump(pc)
                    | Instruction::Unless(_, pc)
                    | Instruction::IfPresent(_, pc)
                    | Instruction::UnlessFixnum(_, pc)
                    | Instruction::UnlessFixnumInRange(_, _, _, pc)
                    | Instruction::WhenBound { jump: pc, .. }
                    | Instruction::EnterBlock { resume: pc, .. }
                    | Instruction::EnterCatch { resume: pc, .. }
                    | Instruction::EnterProtect { cleanup: pc, .. } => *pc,
                    _ => continue,
                };
                targets[target] = true;
            }
            for entry in self.handlers.iter().flat_map(|table| &table.handlers) {
                if let HandlerTarget::Case { pc, .. } = entry.target {
                    targets[pc] = true;
                }
            }
            for entry in self.restarts.iter().flat_map(|table| &table.restarts) {
                if let RestartTarget::Case { pc, .. } = entry.target {
                    targets[pc] = true;
                }
            }
            for &pc in self.jump_tables.iter().flat_map(|table| &table.targets) {
                targets[pc] = true;
            }
            targets
        })
    }

    fn branch_span(&mut self, pc: usize, out: u16) -> Option<BranchSpan> {
        let after_call = pc + 1;
        let (branch_pc, cleared, consumed) = match self.instructions.get(after_call)? {
            Instruction::Clear(range) if !range.contains(&usize::from(out)) => {
                (after_call + 1, range.clone(), 3)
            }
            _ => (after_call, 0..0, 2),
        };
        let Instruction::Unless(test, target) = *self.instructions.get(branch_pc)? else {
            return None;
        };
        if test != usize::from(out) {
            return None;
        }
        let targets = self.jump_targets();
        if targets[after_call] || targets[branch_pc] {
            return None;
        }
        Some(BranchSpan {
            target: u32::try_from(target).ok()?,
            clear_start: u16::try_from(cleared.start).ok()?,
            clear_len: u16::try_from(cleared.len()).ok()?,
            consumed,
        })
    }

    fn arith_imm(&mut self, site: usize) -> Option<(ArithKind, u16, u16, u16, i32, u32)> {
        let Instruction::Load(tmp, Immediate::Fixnum(value)) = self.instructions[site] else {
            return None;
        };
        let imm = i32::try_from(value).ok()?;
        let _ = crate::heap::Word::fixnum(imm as isize)?;
        let next = site + 1;
        let Instruction::Call {
            callee: Callee::Named(symbol),
            out: Some(out),
            args,
            associate: false,
            notinline: false,
            ..
        } = self.instructions.get(next)?
        else {
            return None;
        };
        if args.len() != 2 {
            return None;
        }
        let symbol = *symbol;
        let out = *out;
        let kind = self
            .canonical_symbols
            .get(symbol)
            .copied()
            .flatten()?
            .arith_kind()?;
        let commutative = matches!(kind, ArithKind::Add | ArithKind::Mul);
        let src = if tmp == args.start + 1 {
            args.start
        } else if commutative && tmp == args.start {
            args.start + 1
        } else {
            return None;
        };
        if self.jump_targets()[next] {
            return None;
        }
        Some((
            kind,
            self.reg_u16(out)?,
            self.reg_u16(src)?,
            self.reg_u16(tmp)?,
            imm,
            u32::try_from(symbol).ok()?,
        ))
    }

    fn clear_backedge(&mut self, site: usize) -> Option<(u16, u16, u32)> {
        let Instruction::Clear(regs) = &self.instructions[site] else {
            return None;
        };
        let next = site + 1;
        let Instruction::Jump(target) = *self.instructions.get(next)? else {
            return None;
        };
        if target > site || self.jump_targets()[next] {
            return None;
        }
        Some((
            self.reg_u16(regs.start)?,
            u16::try_from(regs.len()).ok()?,
            u32::try_from(target).ok()?,
        ))
    }

    fn reg_u16(&self, reg: Register) -> Option<u16> {
        u16::try_from(reg).ok()
    }
}
