use crate::{ArrayContents, BytecodeFunction, CompiledFile, LiteralNode};

impl CompiledFile {
    /// Code in native entry index order: top-level entries followed by literal functions.
    /// As with [`Self::nested_function_nodes`], the file must be validated first.
    pub fn code_entries(&self) -> impl Iterator<Item = &BytecodeFunction> {
        self.entries
            .iter()
            .chain(self.nested_function_nodes().map(|(_, function)| function))
    }

    /// Yield literal function nodes in the order used to assign native entry indices.
    /// Start from the constants of top-level entries in file order and visit each
    /// literal in depth-first preorder. Append any remaining function nodes in
    /// literal table order. Top-level code itself precedes all of these entries.
    ///
    /// Call this on a validated file. Out-of-range literal references can panic.
    pub fn nested_function_nodes(&self) -> impl Iterator<Item = (usize, &BytecodeFunction)> {
        let mut pending: Vec<u32> = self
            .entries
            .iter()
            .rev()
            .flat_map(|entry| entry.constants.iter().rev().copied())
            .collect();
        let mut remaining = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(id, node)| matches!(node, LiteralNode::Function(_)).then_some(id));
        let mut visited = vec![false; self.nodes.len()];

        std::iter::from_fn(move || {
            while let Some(id) = pending
                .pop()
                .map(|id| id as usize)
                .or_else(|| remaining.next())
            {
                if std::mem::replace(&mut visited[id], true) {
                    continue;
                }
                let node = &self.nodes[id];
                node.push_references(&mut pending);
                if let LiteralNode::Function(code) = node {
                    return Some((id, code));
                }
            }
            None
        })
    }
}

impl LiteralNode {
    /// Push this literal's children in reverse visitation order. Cyclic
    /// aggregates are permitted, so callers keep their own visited set.
    pub fn push_references(&self, pending: &mut Vec<u32>) {
        match self {
            LiteralNode::RuntimeObject { slots, .. } => pending.extend(slots.iter().rev().copied()),
            LiteralNode::Function(code) => pending.extend(code.constants.iter().rev().copied()),
            LiteralNode::Closure {
                code,
                captures,
                documentation,
            } => {
                pending.push(*documentation);
                pending.extend(captures.iter().rev().copied());
                pending.push(*code);
            }
            LiteralNode::LexicalCell(value) => pending.push(*value),
            LiteralNode::Pair(a, b) | LiteralNode::Ratio(a, b) | LiteralNode::Complex(a, b) => {
                pending.extend([*b, *a]);
            }
            LiteralNode::Pathname(parts) => pending.extend(parts.iter().rev().copied()),
            LiteralNode::HashTable { entries, .. } => {
                for &(key, value) in entries.iter().rev() {
                    pending.extend([value, key]);
                }
            }
            LiteralNode::Array { contents, .. } => match contents {
                ArrayContents::Elements(elements) => pending.extend(elements.iter().rev().copied()),
                ArrayContents::Octets(_)
                | ArrayContents::SingleFloats(_)
                | ArrayContents::DoubleFloats(_) => {}
                ArrayContents::Displaced { target, .. } => pending.push(*target),
            },
            LiteralNode::Unbound
            | LiteralNode::ExternalResource { .. }
            | LiteralNode::Nil
            | LiteralNode::True
            | LiteralNode::Fixnum(_)
            | LiteralNode::Character(_)
            | LiteralNode::String(_)
            | LiteralNode::InternedSymbol { .. }
            | LiteralNode::UninternedSymbol(_)
            | LiteralNode::Package(_)
            | LiteralNode::Primitive(_)
            | LiteralNode::BuiltinNative(_)
            | LiteralNode::Bignum { .. }
            | LiteralNode::SingleFloat(_)
            | LiteralNode::DoubleFloat(_)
            | LiteralNode::RandomState(_)
            | LiteralNode::SimdPack(_)
            | LiteralNode::SimdMask(_)
            | LiteralNode::LoadForm { .. } => {}
        }
    }
}
