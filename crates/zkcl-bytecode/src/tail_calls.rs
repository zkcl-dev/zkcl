use crate::Instruction;

/// Calls whose continuation can only clear registers and return the current values.
/// The input must have validated branch targets. A jump cycle never returns.
pub fn tail_call_sites(instructions: &[Instruction]) -> impl Iterator<Item = usize> + '_ {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Path {
        Unknown,
        Other,
        Return,
    }

    let mut paths = Vec::new();
    let successor = |pc| match instructions.get(pc) {
        Some(Instruction::Clear(_)) => Some(pc + 1),
        Some(Instruction::Jump(target)) => Some(*target),
        _ => None,
    };
    instructions
        .iter()
        .enumerate()
        .filter_map(move |(pc, instruction)| {
            if !matches!(
                instruction,
                Instruction::Call {
                    out: None,
                    associate: false,
                    ..
                } | Instruction::CallValues { .. }
            ) {
                return None;
            }
            let start = pc + 1;
            if start == instructions.len() {
                return Some(pc);
            }
            let mut next = successor(start)?;
            if next == instructions.len() {
                return Some(pc);
            }
            if paths.is_empty() {
                paths.resize(instructions.len() + 1, Path::Unknown);
                paths[instructions.len()] = Path::Return;
            }
            while paths[next] == Path::Unknown {
                // A revisit before reaching the exit proves this path is cyclic.
                paths[next] = Path::Other;
                next = successor(next)?;
            }
            if paths[next] == Path::Other {
                return None;
            }
            let mut next = start;
            while paths[next] != Path::Return {
                paths[next] = Path::Return;
                next = successor(next).expect("return path has a successor");
            }
            Some(pc)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Callee;

    #[test]
    fn follows_long_return_paths_but_leaves_effects_and_cycles_alone() {
        let mut code = vec![
            Instruction::CallValues { callee: 0, at: 0 },
            Instruction::Jump(6),
            Instruction::CallValues { callee: 0, at: 0 },
            Instruction::Jump(3),
            Instruction::CallValues { callee: 0, at: 0 },
            Instruction::SetValues(0..1),
        ];
        code.extend((0..12).map(|_| Instruction::Clear(0..1)));
        code.push(Instruction::Call {
            callee: Callee::Named(0),
            out: None,
            args: 0..1,
            at: 0,
            associate: true,
            notinline: false,
        });
        assert!(tail_call_sites(&code).next().is_none());
        code.pop();
        assert_eq!(tail_call_sites(&code).collect::<Vec<_>>(), [0]);
    }
}
