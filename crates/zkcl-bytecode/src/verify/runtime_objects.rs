//! Validate image object invariants before the runtime allocates any shells.
//!
//! A shell's slot count alone does not establish the types of its outgoing
//! edges. Keep this validation on literal descriptions, before any typed heap
//! access, and allow cycles through ordinary Lisp values.

use std::collections::HashSet;

use super::{Result, ValidationError, ValidationLocation, is_symbol};
use crate::{ArrayElement, Class, CompiledFile, LiteralNode};

pub(super) fn validate(file: &CompiledFile, start: usize) -> Result {
    let validator = Validator {
        nodes: &file.nodes,
        max_weight: file.target.fixnum_bounds().1 as u64,
    };
    let mut symbols = HashSet::new();
    let mut classes = HashSet::new();
    let mut owners = HashSet::new();
    // Every target shape must be checked before the second pass reads its data.
    for (index, node) in file.nodes.iter().enumerate().skip(start) {
        if let LiteralNode::RuntimeObject { kind, data, slots } = node {
            validator.shape(index, kind, data, slots)?;
            let unique = match kind.as_str() {
                "symbol" => symbols.insert(data[0]),
                "class" => classes.insert(data[1]),
                "mutable-treelist" => owners.insert(data[0]),
                _ => true,
            };
            if !unique {
                return Err(error(index, "duplicate runtime image object identity"));
            }
        }
    }
    let mut spans = HashSet::new();
    for (index, node) in file.nodes.iter().enumerate().skip(start) {
        if let LiteralNode::RuntimeObject { kind, data, slots } = node {
            validator.edges(index, kind, data, slots, &mut spans)?;
        }
    }
    Ok(())
}

fn error(index: usize, reason: &'static str) -> ValidationError {
    ValidationError::new(ValidationLocation::LiteralNode(index), None, reason)
}

struct Validator<'a> {
    nodes: &'a [LiteralNode],
    max_weight: u64,
}

impl Validator<'_> {
    fn shape(&self, index: usize, kind: &str, data: &[u64], slots: &[u32]) -> Result {
        let count = |at: usize| data.get(at).and_then(|&n| usize::try_from(n).ok());
        let (data_len, slot_count) = match kind {
            "symbol" => (7, Some(6)),
            "condition" => (2, count(1)),
            "restart" => (0, Some(2)),
            "macro-environment" => (1, Some(0)),
            "readtable" => (1, Some(2)),
            "class" => (2, Some(6)),
            "instance" => (0, Some(2)),
            "funcallable-instance" => (0, Some(4)),
            "instance-storage" | "structure" => (1, count(0).and_then(|n| n.checked_add(1))),
            "instance-layout" => (2, count(0).and_then(|n| n.checked_mul(2))),
            "generic-function" => (4, Some(15)),
            "method" => (0, Some(10)),
            "specializer" => (0, Some(3)),
            "slot-definition" => (1, Some(12)),
            "stream" => (12, Some(1)),
            "weak-pointer" => (0, Some(1)),
            "treelist" => (0, Some(1)),
            "mutable-treelist" => (1, Some(1)),
            "treelist-node" => (3, Some(3)),
            "treelist-span" => (4, Some(1)),
            "treelist-chunk" => (1, Some(32)),
            "builtin-native" | "package" => (1, Some(0)),
            _ => return Err(error(index, "unknown runtime image object kind")),
        };
        let bad = || error(index, "invalid runtime image object shape");
        if data.len() != data_len
            || slot_count != Some(slots.len())
            || data.iter().any(|&n| usize::try_from(n).is_err())
            || slots.iter().any(|&id| id as usize >= self.nodes.len())
        {
            return Err(bad());
        }
        let flag = |at: usize| data[at] <= 1;
        let valid = match kind {
            "symbol" => {
                data[0] >= 2 && data[1] <= ((usize::MAX >> 3) as u64 + 1) && (2..7).all(flag)
            }
            "condition" => Class::all()
                .find(|&class| class as u64 == data[0])
                .is_some_and(|class| data[1] == condition_slots(class) as u64),
            "readtable" => data[0] <= 3,
            "class" => data[0] <= 3,
            "instance-layout" => data[1] <= data[0],
            "generic-function" => data[0] <= data[1] && flag(2) && flag(3),
            "slot-definition" => flag(0),
            "stream" => {
                data[0] <= 15
                    && data[1] <= 5
                    && data[2] <= u16::MAX as u64
                    && data[3] <= 2
                    && (8..11).all(flag)
                    && (!(3..=6).contains(&data[0]) || data[9] == 1)
                    && u32::try_from(data[11])
                        .ok()
                        .and_then(char::from_u32)
                        .is_some()
            }
            "treelist-node" => {
                data[0] != u64::MAX
                    && data[1] > 0
                    && data[1] <= self.max_weight
                    && data[2] <= u8::MAX as u64
            }
            "treelist-span" => {
                data[0] != u64::MAX && data[1] <= self.max_weight && data[2] < 32 && data[3] <= 32
            }
            "treelist-chunk" => data[0] != u64::MAX,
            "mutable-treelist" => data[0] > 0 && data[0] < u64::MAX,
            "package" => data[0] <= (usize::MAX >> 3) as u64,
            _ => true,
        };
        if valid { Ok(()) } else { Err(bad()) }
    }

    fn node(&self, id: u32) -> &LiteralNode {
        &self.nodes[id as usize]
    }
    fn object(&self, id: u32, expected: &str) -> Option<(&[u64], &[u32])> {
        match self.node(id) {
            LiteralNode::RuntimeObject { kind, data, slots } if kind == expected => {
                Some((data, slots))
            }
            _ => None,
        }
    }
    fn nil(&self, id: u32) -> bool {
        matches!(self.node(id), LiteralNode::Nil)
    }
    fn unbound(&self, id: u32) -> bool {
        matches!(self.node(id), LiteralNode::Unbound)
    }
    fn kind_or_nil(&self, id: u32, kind: &str) -> bool {
        self.nil(id) || self.object(id, kind).is_some()
    }
    fn symbol(&self, id: u32) -> bool {
        is_symbol(self.node(id))
    }
    fn string(&self, id: u32) -> bool {
        match self.node(id) {
            LiteralNode::String(_) => true,
            LiteralNode::Array {
                element: ArrayElement::Character,
                dimensions,
                ..
            } => dimensions.len() == 1,
            _ => false,
        }
    }
    fn documentation(&self, id: u32) -> bool {
        self.nil(id) || self.string(id)
    }
    fn callable(&self, id: u32) -> bool {
        matches!(
            self.node(id),
            LiteralNode::Function(_)
                | LiteralNode::Closure { .. }
                | LiteralNode::Primitive(_)
                | LiteralNode::BuiltinNative(_)
                | LiteralNode::ExternalResource { .. }
        ) || ["builtin-native", "generic-function", "funcallable-instance"]
            .iter()
            .any(|kind| self.object(id, kind).is_some())
    }
    fn callable_or_empty(&self, id: u32) -> bool {
        self.nil(id) || self.unbound(id) || self.callable(id)
    }
    fn meta_instance(&self, id: u32) -> bool {
        self.nil(id)
            || self.object(id, "instance").is_some()
            || self.object(id, "funcallable-instance").is_some()
    }
    fn specializer(&self, id: u32) -> bool {
        self.object(id, "class").is_some() || self.object(id, "specializer").is_some()
    }
    fn pair(&self, id: u32) -> Option<(u32, u32)> {
        if let LiteralNode::Pair(a, b) = self.node(id) {
            Some((*a, *b))
        } else {
            None
        }
    }

    fn list(&self, index: usize, mut id: u32, mut item: impl FnMut(u32) -> bool) -> Result {
        let mut seen = HashSet::new();
        while !self.nil(id) {
            if !seen.insert(id) {
                return Err(error(index, "circular runtime image metadata list"));
            }
            let (head, tail) = self
                .pair(id)
                .ok_or_else(|| error(index, "improper runtime image metadata list"))?;
            if !item(head) {
                return Err(error(index, "invalid runtime image metadata list element"));
            }
            id = tail;
        }
        Ok(())
    }

    fn edges(
        &self,
        index: usize,
        kind: &str,
        data: &[u64],
        slots: &[u32],
        spans: &mut HashSet<(u32, u8)>,
    ) -> Result {
        let bad = || error(index, "invalid runtime image object edge");
        let require = |valid| if valid { Ok(()) } else { Err(bad()) };
        match kind {
            "symbol" => {
                require(matches!(self.node(slots[3]), LiteralNode::String(_)))?;
                require(self.callable_or_empty(slots[1]) && self.callable_or_empty(slots[2]))?;
                require(self.documentation(slots[4]))?;
            }
            "restart" => require(
                self.symbol(slots[0]) && (self.documentation(slots[1]) || self.callable(slots[1])),
            )?,
            "readtable" => {
                require(data[0] <= 3)?;
                for &list in slots {
                    self.list(index, list, |entry| {
                        self.pair(entry).is_some_and(|(key, _)| {
                            matches!(self.node(key), LiteralNode::Character(_))
                        })
                    })?;
                }
                self.readtable_definitions(index, slots[0])?;
                self.list(index, slots[1], |entry| {
                    self.pair(entry).is_some_and(|(_, syntax)| {
                        [
                            "WHITESPACE",
                            "INVALID",
                            "CONSTITUENT",
                            "SINGLE-ESCAPE",
                            "MULTIPLE-ESCAPE",
                            "MACRO",
                        ]
                        .iter()
                        .any(|name| self.keyword_named(syntax, name))
                    })
                })?;
            }
            "class" => {
                require(
                    self.symbol(slots[0]) && self.symbol(slots[1]) && self.meta_instance(slots[2]),
                )?;
                self.list(index, slots[3], |id| self.object(id, "class").is_some())?;
                self.list(index, slots[4], |id| self.object(id, "method").is_some())?;
                self.list(index, slots[5], |_| true)?;
            }
            "instance" | "funcallable-instance" => {
                require(
                    self.object(slots[0], "class").is_some()
                        && self.object(slots[1], "instance-storage").is_some(),
                )?;
                if kind == "funcallable-instance" {
                    require(self.callable_or_empty(slots[2]) && self.documentation(slots[3]))?;
                }
            }
            "instance-storage" => {
                let Some((layout, _)) = self.object(slots[0], "instance-layout") else {
                    return Err(bad());
                };
                require(data[0] == layout[1])?;
            }
            "instance-layout" => {
                for (position, pair) in slots.chunks_exact(2).enumerate() {
                    require(self.symbol(pair[0]))?;
                    require(if (position as u64) < data[1] {
                        self.unbound(pair[1])
                    } else {
                        self.object(pair[1], "lexical-cell").is_some()
                            || matches!(self.node(pair[1]), LiteralNode::LexicalCell(_))
                    })?;
                }
            }
            "generic-function" => {
                require(self.symbol(slots[0]) || self.pair(slots[0]).is_some())?;
                if !self.unbound(slots[1]) {
                    self.list(index, slots[1], |_| true)?;
                }
                if !self.unbound(slots[2]) {
                    self.list(index, slots[2], |id| self.symbol(id))?;
                }
                let mut count = 0;
                let mut positions = HashSet::new();
                self.list(index, slots[3], |id| {
                    count += 1;
                    matches!(self.node(id),LiteralNode::Fixnum(n) if *n>=0 && (*n as u64)<data[0] && positions.insert(*n))
                })?;
                require(count == 0 || count as u64 == data[0])?;
                self.list(index, slots[4], |id| self.symbol(id))?;
                self.list(index, slots[5], |_| true)?;
                require(self.documentation(slots[6]))?;
                self.list(index, slots[7], |id| self.object(id, "method").is_some())?;
                require(
                    self.callable_or_empty(slots[8])
                        && self.meta_instance(slots[9])
                        && self.kind_or_nil(slots[10], "class"),
                )?;
                self.list(index, slots[12], |_| true)?;
                self.list(index, slots[13], |_| true)?;
            }
            "method" => {
                self.list(index, slots[0], |_| true)?;
                self.list(index, slots[1], |id| self.specializer(id))?;
                self.list(index, slots[2], |_| true)?;
                require(
                    self.callable_or_empty(slots[3])
                        && self.documentation(slots[4])
                        && self.kind_or_nil(slots[5], "generic-function")
                        && self.kind_or_nil(slots[6], "slot-definition")
                        && self.callable_or_empty(slots[7])
                        && self.meta_instance(slots[8]),
                )?;
            }
            "specializer" => {
                self.list(index, slots[1], |id| self.object(id, "method").is_some())?;
                require(self.meta_instance(slots[2]))?;
            }
            "slot-definition" => {
                require(
                    self.meta_instance(slots[0])
                        && self.kind_or_nil(slots[1], "class")
                        && self.symbol(slots[2]),
                )?;
                self.list(index, slots[3], |id| self.symbol(id))?;
                require(self.callable_or_empty(slots[5]) && self.documentation(slots[11]))?;
                self.list(index, slots[9], |_| true)?;
                self.list(index, slots[10], |_| true)?;
            }
            "structure" => require(self.symbol(slots[0]))?,
            "stream" => self.stream(index, data, slots[0])?,
            "treelist" | "mutable-treelist" => {
                require(self.kind_or_nil(slots[0], "treelist-node"))?;
                if let Some((root, _)) = self.object(slots[0], "treelist-node") {
                    require(root[2] == 0)?;
                }
            }
            "treelist-node" => {
                require(
                    self.kind_or_nil(slots[0], "treelist-span")
                        && self.kind_or_nil(slots[1], "treelist-node")
                        && self.kind_or_nil(slots[2], "treelist-span"),
                )?;
                let mut weight = 0u64;
                for (position, &id) in slots.iter().enumerate() {
                    if self.nil(id) {
                        continue;
                    }
                    let child_kind = if position == 1 {
                        "treelist-node"
                    } else {
                        "treelist-span"
                    };
                    let (child, _) = self.object(id, child_kind).ok_or_else(bad)?;
                    weight = weight.checked_add(child[1]).ok_or_else(bad)?;
                    if position == 1 {
                        require(child[2] == data[2] + 1)?;
                    } else {
                        self.span(index, id, data[2] as u8, spans)?;
                    }
                }
                require(weight == data[1])?;
            }
            "treelist-span" => require(self.object(slots[0], "treelist-chunk").is_some())?,
            _ => {}
        }
        Ok(())
    }

    fn span(&self, index: usize, id: u32, depth: u8, visited: &mut HashSet<(u32, u8)>) -> Result {
        let mut pending = vec![(id, depth)];
        while let Some((id, depth)) = pending.pop() {
            if !visited.insert((id, depth)) {
                continue;
            }
            let bad = || error(index, "invalid runtime image treelist topology");
            let (data, slots) = self.object(id, "treelist-span").ok_or_else(bad)?;
            let (_, chunk) = self.object(slots[0], "treelist-chunk").ok_or_else(bad)?;
            let len = data[3] as usize;
            let mut weight = 0u64;
            if depth == 0 {
                weight = data[3];
            } else {
                for offset in 0..len {
                    let child = chunk[(data[2] as usize + offset) & 31];
                    let (child_data, _) = self.object(child, "treelist-span").ok_or_else(bad)?;
                    weight = weight.checked_add(child_data[1]).ok_or_else(bad)?;
                    pending.push((child, depth - 1));
                }
            }
            if weight != data[1] {
                return Err(bad());
            }
        }
        Ok(())
    }

    fn readtable_definitions(&self, index: usize, mut list: u32) -> Result {
        while let Some((entry, next)) = self.pair(list) {
            let (_, definition) = self.pair(entry).expect("validated readtable entry");
            if !self.nil(definition) {
                let (head, tail) = self
                    .pair(definition)
                    .ok_or_else(|| error(index, "invalid image readtable definition"))?;
                if self.keyword_named(head, "DISPATCH")
                    || self.keyword_named(head, "STANDARD-DISPATCH")
                {
                    let (_, entries) = self
                        .pair(tail)
                        .ok_or_else(|| error(index, "invalid image dispatch definition"))?;
                    self.list(index, entries, |entry| {
                        self.pair(entry).is_some_and(|(key, function)| {
                            matches!(self.node(key), LiteralNode::Character(_))
                                && self.callable(function)
                        })
                    })?;
                } else if !self.callable(head) {
                    return Err(error(index, "invalid image reader macro function"));
                }
            }
            list = next;
        }
        Ok(())
    }

    fn keyword_named(&self, id: u32, expected: &str) -> bool {
        match self.node(id) {
            LiteralNode::InternedSymbol { package, name } => {
                package == "KEYWORD" && name == expected
            }
            LiteralNode::RuntimeObject { kind, data, slots }
                if kind == "symbol" && data[1] == 3 =>
            {
                matches!(self.node(slots[3]),LiteralNode::String(name) if name==expected)
            }
            _ => false,
        }
    }

    fn stream(&self, index: usize, data: &[u64], contents: u32) -> Result {
        let bad = || error(index, "invalid runtime image stream contents");
        match data[0] {
            0 => {
                let LiteralNode::String(text) = self.node(contents) else {
                    return Err(bad());
                };
                let [cursor, start, end] = [data[4] as usize, data[5] as usize, data[6] as usize];
                if start > cursor
                    || cursor > end
                    || end > text.len()
                    || ![start, cursor, end]
                        .iter()
                        .all(|&at| text.is_char_boundary(at))
                {
                    return Err(bad());
                }
            }
            2 => {
                if !self.string(contents) {
                    return Err(bad());
                }
            }
            1 => self.list(index, contents, |id| self.string(id))?,
            11 | 12 => self.list(index, contents, |id| self.stream_object(id))?,
            13 | 15 => {
                if !self.pair(contents).is_some_and(|(input, output)| {
                    self.stream_object(input) && self.stream_object(output)
                }) {
                    return Err(bad());
                }
            }
            14 => {
                if !self.symbol(contents) {
                    return Err(bad());
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn stream_object(&self, id: u32) -> bool {
        self.object(id, "stream").is_some()
            || self.object(id, "instance").is_some()
            || matches!(self.node(id), LiteralNode::ExternalResource { .. })
    }
}

fn condition_slots(class: Class) -> usize {
    let own = match class.name() {
        "SIMPLE-CONDITION" | "TYPE-ERROR" | "ARITHMETIC-ERROR" => 2,
        "CELL-ERROR" | "UNBOUND-SLOT" | "STREAM-ERROR" | "PACKAGE-ERROR" | "FILE-ERROR"
        | "PRINT-NOT-READABLE" => 1,
        _ => 0,
    };
    own + class
        .direct_superclasses()
        .iter()
        .map(|&parent| condition_slots(parent))
        .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(kind: &str, data: &[u64], slots: &[u32]) -> LiteralNode {
        LiteralNode::RuntimeObject {
            kind: kind.into(),
            data: data.to_vec(),
            slots: slots.to_vec(),
        }
    }

    // Serialize without encode(), which intentionally refuses malformed data,
    // so these exercise the public decoder's boundary without allocating shells.
    fn decode(
        nodes: Vec<LiteralNode>,
    ) -> std::result::Result<CompiledFile, crate::CompiledFileError> {
        let file = CompiledFile {
            target: Default::default(),
            source_files: Vec::new(),
            nodes,
            entries: Vec::new(),
        };
        let bytes = postcard::to_extend(&file, crate::FASL_MAGIC.to_vec()).unwrap();
        CompiledFile::decode(&bytes)
    }

    #[test]
    fn decoder_rejects_runtime_object_type_confusion_and_storage_mismatch() {
        for nodes in [
            vec![
                LiteralNode::Nil,
                LiteralNode::Fixnum(42),
                object("instance", &[], &[0, 1]),
            ],
            vec![
                LiteralNode::Nil,
                object("instance-layout", &[1, 1], &[0, 2]),
                LiteralNode::Unbound,
                object("instance-storage", &[2], &[1, 0, 0]),
            ],
            vec![
                LiteralNode::Nil,
                LiteralNode::Pair(0, 0),
                object("instance-layout", &[1, 0], &[0, 1]),
            ],
            vec![
                LiteralNode::Nil,
                object("slot-definition", &[0], &[0; 12]),
                object("class", &[1, 1], &[0, 0, 1, 0, 0, 0]),
            ],
        ] {
            assert!(decode(nodes).is_err());
        }
    }

    #[test]
    fn decoder_rejects_runtime_object_truncated_shape_and_reference_bounds() {
        for nodes in [
            vec![object("class", &[1], &[])],
            vec![object("instance-storage", &[u64::MAX], &[])],
            vec![object("instance-layout", &[u64::MAX, 0], &[])],
            vec![object("weak-pointer", &[], &[1])],
            vec![LiteralNode::Pair(8, 0), object("readtable", &[0], &[0, 0])],
            vec![object("not-an-image-object", &[], &[])],
        ] {
            assert!(decode(nodes).is_err());
        }
    }

    #[test]
    fn decoder_rejects_cyclic_private_lists_and_string_stream_byte_offsets() {
        let cyclic = vec![
            LiteralNode::Nil,
            LiteralNode::Pair(0, 1),
            object("readtable", &[0], &[1, 0]),
        ];
        assert!(decode(cyclic).is_err());
        for cursor in [1, 3] {
            assert!(
                decode(vec![
                    LiteralNode::String("é".into()),
                    object("stream", &[0, 1, 0, 0, cursor, 0, 2, 0, 0, 0, 0, 0], &[0])
                ])
                .is_err()
            );
        }
        assert!(
            decode(vec![
                LiteralNode::String("é".into()),
                object("stream", &[0, 1, 0, 0, 2, 0, 2, 0, 0, 0, 0, 0], &[0])
            ])
            .is_ok()
        );
    }

    #[test]
    fn decoder_rejects_invalid_treelist_topology_without_rejecting_value_cycles() {
        let mut chunk = vec![0; 32];
        chunk[0] = 1;
        // The treelist itself is a legitimate Lisp value in its leaf chunk.
        let valid = vec![
            LiteralNode::Nil,
            object("treelist", &[], &[2]),
            object("treelist-node", &[0, 1, 0], &[3, 0, 0]),
            object("treelist-span", &[0, 1, 0, 1], &[4]),
            object("treelist-chunk", &[0], &chunk),
        ];
        assert!(decode(valid.clone()).is_ok());
        let mut wrong_weight = valid.clone();
        wrong_weight[2] = object("treelist-node", &[0, 2, 0], &[3, 0, 0]);
        assert!(decode(wrong_weight).is_err());
        let mut wrong_chunk = valid.clone();
        wrong_chunk[3] = object("treelist-span", &[0, 1, 0, 1], &[2]);
        assert!(decode(wrong_chunk).is_err());
        let mut cyclic_middle = valid;
        cyclic_middle[2] = object("treelist-node", &[0, 1, 0], &[0, 2, 0]);
        assert!(decode(cyclic_middle).is_err());
    }

    #[test]
    fn decoder_accepts_uninitialized_metaobjects_and_shared_cells() {
        let mut generic_slots = vec![0; 15];
        generic_slots[1] = 1;
        generic_slots[2] = 1;
        let mut method_slots = vec![0; 10];
        method_slots[3] = 1;
        assert!(
            decode(vec![
                LiteralNode::Nil,
                LiteralNode::Unbound,
                object("generic-function", &[0, 0, 1, 1], &generic_slots),
                object("method", &[], &method_slots),
                object("slot-definition", &[1], &[0; 12]),
                LiteralNode::LexicalCell(0),
                object("instance-layout", &[1, 0], &[0, 5])
            ])
            .is_ok()
        );
    }
}
