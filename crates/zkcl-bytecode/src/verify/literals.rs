use std::collections::{HashMap, HashSet};

use num_bigint::BigUint;
use num_integer::Integer;
use num_traits::One;

use super::{CodeVerifier, ControlFlowScratch, Result, ValidationError, ValidationLocation};
use crate::{ARRAY_RANK_LIMIT, ArrayContents, ArrayElement, CompiledFile, LiteralNode};

pub(super) struct LiteralVerifier<'a> {
    file: &'a CompiledFile,
    string_sizes: HashMap<u32, usize>,
    load_form_entries: HashSet<usize>,
    first_runtime_object: Option<usize>,
    first_displacement: Option<usize>,
}

impl<'a> LiteralVerifier<'a> {
    pub(super) fn new(file: &'a CompiledFile) -> Self {
        Self {
            file,
            string_sizes: HashMap::new(),
            load_form_entries: HashSet::new(),
            first_runtime_object: None,
            first_displacement: None,
        }
    }

    pub(super) fn validate_nodes(&mut self, flow: &mut ControlFlowScratch) -> Result {
        for (index, literal) in self.file.nodes.iter().enumerate() {
            self.validate_node(flow, index, literal)?;
        }
        if let Some(start) = self.first_runtime_object {
            super::runtime_objects::validate(self.file, start)?;
        }
        if let Some(start) = self.first_displacement {
            validate_displacements(&self.file.nodes, start)?;
        }
        Ok(())
    }

    fn validate_node(
        &mut self,
        flow: &mut ControlFlowScratch,
        index: usize,
        literal: &LiteralNode,
    ) -> Result {
        let file = self.file;
        let location = ValidationLocation::LiteralNode(index);
        let invalid = |reason| ValidationError::new(location, None, reason);
        let node = |reference: u32| {
            file.nodes
                .get(reference as usize)
                .ok_or_else(|| invalid("literal reference is out of bounds"))
        };
        let check_ref = |reference| node(reference).map(|_| ());
        match literal {
            LiteralNode::RuntimeObject { .. } => {
                self.first_runtime_object.get_or_insert(index);
            }
            LiteralNode::Closure {
                code,
                captures,
                documentation,
            } => {
                let LiteralNode::Function(function) = node(*code)? else {
                    return Err(invalid("closure code must reference a compiled function"));
                };
                if function.captures.len() != captures.len() {
                    return Err(invalid("closure capture count does not match its function"));
                }
                use crate::CaptureKind;
                for (&reference, kind) in captures.iter().zip(&function.captures) {
                    let actual = node(reference)?;
                    if match kind {
                        CaptureKind::MutableCell => !matches!(actual, LiteralNode::LexicalCell(_)),
                        CaptureKind::Value => matches!(actual, LiteralNode::LexicalCell(_)),
                        CaptureKind::ExitToken => true,
                    } {
                        return Err(invalid("closure capture does not match its compiled kind"));
                    }
                }
                if !matches!(
                    node(*documentation)?,
                    LiteralNode::Nil | LiteralNode::String(_)
                ) {
                    return Err(invalid("closure documentation must be a string or NIL"));
                }
            }
            LiteralNode::LexicalCell(value) => check_ref(*value)?,
            LiteralNode::Pair(car, cdr) => {
                check_ref(*car)?;
                check_ref(*cdr)?;
            }
            LiteralNode::Pathname(components) => {
                validate_pathname(node, invalid, components)?;
            }
            LiteralNode::HashTable {
                rehash_size,
                rehash_threshold,
                entries,
                ..
            } => {
                if !rehash_size.valid_size() {
                    return Err(invalid("invalid hash table rehash size"));
                }
                if !rehash_threshold.valid_threshold() {
                    return Err(invalid("invalid hash table rehash threshold"));
                }
                for &(key, value) in entries {
                    check_ref(key)?;
                    check_ref(value)?;
                }
            }
            LiteralNode::LoadForm {
                creation,
                initialization,
            } => {
                if *creation >= file.entries.len() || !self.load_form_entries.insert(*creation) {
                    return Err(invalid("missing or repeated load form creation entry"));
                }
                if let Some(index) = initialization
                    && (*index <= *creation
                        || *index >= file.entries.len()
                        || !self.load_form_entries.insert(*index))
                {
                    return Err(invalid("invalid load form initialization entry"));
                }
            }
            LiteralNode::Function(code) => CodeVerifier {
                file,
                code,
                location,
            }
            .run(flow)?,
            LiteralNode::Fixnum(value) if !file.valid_fixnum(*value) => {
                return Err(invalid("fixnum is outside the VM's tagged range"));
            }
            LiteralNode::Bignum { magnitude, .. } if !valid_bignum_magnitude(magnitude) => {
                return Err(invalid(
                    "bignum magnitude must be nonzero and minimally encoded",
                ));
            }
            LiteralNode::Ratio(numerator, denominator) => {
                let numerator = node(*numerator)?;
                if !matches!(numerator, LiteralNode::Fixnum(value) if *value != 0)
                    && !matches!(
                        numerator,
                        LiteralNode::Bignum { magnitude, .. } if valid_bignum_magnitude(magnitude)
                    )
                {
                    return Err(invalid("ratio numerator must be a nonzero integer"));
                }
                let denominator = node(*denominator)?;
                if !matches!(denominator, LiteralNode::Fixnum(value) if *value > 1)
                    && !matches!(
                        denominator,
                        LiteralNode::Bignum { negative: false, magnitude }
                            if valid_bignum_magnitude(magnitude)
                                && (magnitude.len() > 1 || magnitude[0] > 1)
                    )
                {
                    return Err(invalid(
                        "ratio denominator must be an integer greater than one",
                    ));
                }
                if !ratio_coprime(numerator, denominator) {
                    return Err(invalid("ratio literal must be in lowest terms"));
                }
            }
            LiteralNode::SingleFloat(bits) if !f32::from_bits(*bits).is_finite() => {
                return Err(invalid("single-float literal must be finite"));
            }
            LiteralNode::DoubleFloat(bits) if !f64::from_bits(*bits).is_finite() => {
                return Err(invalid("double-float literal must be finite"));
            }
            LiteralNode::RandomState(state) if *state == [0; 4] => {
                return Err(invalid("random-state literal cannot be zero"));
            }
            LiteralNode::Complex(real, imag) => {
                let real = node(*real)?;
                let imag = node(*imag)?;
                let rational = |node: &LiteralNode| {
                    matches!(
                        node,
                        LiteralNode::Fixnum(_)
                            | LiteralNode::Bignum { .. }
                            | LiteralNode::Ratio(..)
                    )
                };
                if !(rational(real) && rational(imag) && !matches!(imag, LiteralNode::Fixnum(0))
                    || matches!(
                        (real, imag),
                        (LiteralNode::SingleFloat(_), LiteralNode::SingleFloat(_))
                            | (LiteralNode::DoubleFloat(_), LiteralNode::DoubleFloat(_))
                    ))
                {
                    return Err(invalid(
                        "complex literal must have canonical real components",
                    ));
                }
            }
            LiteralNode::Array {
                dimensions,
                element,
                fill_pointer,
                contents,
                ..
            } => {
                let limit = file.target.fixnum_bounds().1 as u64;
                let size = array_size(dimensions, limit)
                    .ok_or_else(|| invalid("array dimensions exceed the VM limits"))?;
                if fill_pointer.is_some_and(|fill| dimensions.len() != 1 || fill > size) {
                    return Err(invalid("invalid array fill pointer"));
                }
                let check_count = |count| {
                    if count != size {
                        return Err(invalid("array element count does not match its dimensions"));
                    }
                    Ok(())
                };
                let wrong_contents = || invalid("array contents do not match the element type");
                match contents {
                    ArrayContents::Elements(references) => {
                        if matches!(
                            element,
                            ArrayElement::Octet
                                | ArrayElement::SingleFloat
                                | ArrayElement::DoubleFloat
                        ) {
                            return Err(wrong_contents());
                        }
                        check_count(references.len())?;
                        for &reference in references {
                            let value = node(reference)?;
                            match element {
                                ArrayElement::Bit
                                    if !matches!(value, LiteralNode::Fixnum(0 | 1)) =>
                                {
                                    return Err(invalid(
                                        "bit array literal must contain only bits",
                                    ));
                                }
                                ArrayElement::Character
                                    if !matches!(value, LiteralNode::Character(_)) =>
                                {
                                    return Err(invalid(
                                        "character array literal must contain only characters",
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                    ArrayContents::Octets(bytes) => {
                        if *element != ArrayElement::Octet {
                            return Err(wrong_contents());
                        }
                        check_count(bytes.len())?;
                    }
                    ArrayContents::SingleFloats(bits) => {
                        if *element != ArrayElement::SingleFloat {
                            return Err(wrong_contents());
                        }
                        check_count(bits.len())?;
                        if bits.iter().any(|&bits| !f32::from_bits(bits).is_finite()) {
                            return Err(invalid("single-float array elements must be finite"));
                        }
                    }
                    ArrayContents::DoubleFloats(bits) => {
                        if *element != ArrayElement::DoubleFloat {
                            return Err(wrong_contents());
                        }
                        check_count(bits.len())?;
                        if bits.iter().any(|&bits| !f64::from_bits(bits).is_finite()) {
                            return Err(invalid("double-float array elements must be finite"));
                        }
                    }
                    ArrayContents::Displaced { target, offset } => {
                        let target_size = match node(*target)? {
                            LiteralNode::Array {
                                dimensions,
                                element: target_element,
                                ..
                            } if element == target_element => array_size(dimensions, limit)
                                .ok_or_else(|| {
                                    invalid("array target dimensions exceed the VM limits")
                                })?,
                            LiteralNode::String(text) if *element == ArrayElement::Character => {
                                *self
                                    .string_sizes
                                    .entry(*target)
                                    .or_insert_with(|| text.chars().count())
                            }
                            LiteralNode::Array { .. } | LiteralNode::String(_) => {
                                return Err(invalid("array displacement element types must match"));
                            }
                            _ => {
                                return Err(invalid("array displacement requires an array target"));
                            }
                        };
                        if offset.checked_add(size).is_none_or(|end| end > target_size) {
                            return Err(invalid("displaced array does not fit in its target"));
                        }
                        self.first_displacement.get_or_insert(index);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    // All literal references, including the entries' constant pools, are bounded
    // before tracing their first use in file entry order.
    pub(super) fn validate_load_form_order(&self) -> Result {
        if self.load_form_entries.is_empty() {
            return Ok(());
        }
        // The first entry to reach a literal (also through a closure or a
        // circular aggregate) must run after its creation entry returned.
        let mut seen = vec![false; self.file.nodes.len()];
        let mut pending = Vec::new();
        for (index, entry) in self.file.entries.iter().enumerate() {
            pending.extend_from_slice(&entry.constants);
            while let Some(id) = pending.pop() {
                if std::mem::replace(&mut seen[id as usize], true) {
                    continue;
                }
                let node = &self.file.nodes[id as usize];
                if matches!(node, LiteralNode::LoadForm { creation, .. } if *creation >= index) {
                    return Err(ValidationError::new(
                        ValidationLocation::Entry(index),
                        None,
                        "literal referenced before its load form creation entry",
                    ));
                }
                node.push_references(&mut pending);
            }
        }
        Ok(())
    }
}

fn array_size(dimensions: &[usize], limit: u64) -> Option<usize> {
    if dimensions.len() >= ARRAY_RANK_LIMIT || dimensions.iter().any(|&size| size as u64 >= limit) {
        return None;
    }
    let count = if dimensions.contains(&0) {
        0
    } else {
        dimensions
            .iter()
            .copied()
            .try_fold(1_usize, usize::checked_mul)?
    };
    ((count as u64) < limit).then_some(count)
}

fn validate_displacements(nodes: &[LiteralNode], first: usize) -> Result {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Visit {
        Unseen,
        Active,
        Done,
    }

    let target = |index: usize| match &nodes[index] {
        LiteralNode::Array {
            contents: ArrayContents::Displaced { target, .. },
            ..
        } => Some(*target as usize),
        _ => None,
    };
    let mut visits = vec![Visit::Unseen; nodes.len()];
    for start in first..nodes.len() {
        if target(start).is_none() {
            continue;
        }
        let mut index = start;
        while let Some(next) = target(index) {
            match visits[index] {
                Visit::Unseen => visits[index] = Visit::Active,
                Visit::Active => {
                    return Err(ValidationError::new(
                        ValidationLocation::LiteralNode(index),
                        None,
                        "circular array displacement",
                    ));
                }
                Visit::Done => break,
            }
            index = next;
        }
        index = start;
        while visits[index] == Visit::Active {
            visits[index] = Visit::Done;
            index = target(index).expect("active displacement");
        }
    }
    Ok(())
}

fn validate_pathname<'a>(
    get: impl Fn(u32) -> Result<&'a LiteralNode>,
    invalid: impl Fn(&'static str) -> ValidationError,
    components: &[u32; 6],
) -> Result {
    let marker = |node: &'a LiteralNode| -> Option<&'a str> {
        match node {
            LiteralNode::InternedSymbol { name, .. } | LiteralNode::UninternedSymbol(name) => {
                Some(name.as_str())
            }
            LiteralNode::RuntimeObject { kind, slots, .. } if kind == "symbol" => {
                let LiteralNode::String(name) = get(*slots.get(3)?).ok()? else {
                    return None;
                };
                Some(name.as_str())
            }
            _ => None,
        }
    };
    let valid_segment = |text: &str| !text.contains(['/', '\0']);
    let valid_logical_host = |text: &str| {
        !text.is_empty()
            && text.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && !text.bytes().any(|c| c.is_ascii_lowercase())
    };
    let valid_logical_word = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'*'))
            && !text.contains("**")
            && !text.bytes().any(|c| c.is_ascii_lowercase())
    };

    let [host, device, directory, name, file_type, version] = *components;

    let logical = match get(host)? {
        LiteralNode::Nil => false,
        LiteralNode::String(host_name) => {
            if !valid_logical_host(host_name) {
                return Err(invalid("invalid logical pathname host component"));
            }
            true
        }
        other if marker(other) == Some("UNSPECIFIC") => false,
        _ => return Err(invalid("invalid pathname host component")),
    };

    match get(device)? {
        LiteralNode::Nil if !logical => {}
        other if marker(other) == Some("UNSPECIFIC") => {}
        _ => return Err(invalid("invalid pathname device component")),
    }

    match get(directory)? {
        LiteralNode::Nil => {}
        LiteralNode::Pair(head, tail) => {
            if !matches!(marker(get(*head)?), Some("ABSOLUTE" | "RELATIVE")) {
                return Err(invalid("invalid pathname directory marker"));
            }
            let mut cursor = *tail;
            let mut seen = HashSet::new();
            loop {
                match get(cursor)? {
                    LiteralNode::Nil => break,
                    LiteralNode::Pair(elem, next) => {
                        if !seen.insert(cursor) {
                            return Err(invalid("circular pathname directory list"));
                        }
                        match get(*elem)? {
                            LiteralNode::String(text)
                                if valid_segment(text)
                                    && !text.is_empty()
                                    && !matches!(text.as_str(), "." | "..")
                                    && (!logical || valid_logical_word(text)) => {}
                            other
                                if matches!(marker(other), Some("WILD" | "WILD-INFERIORS"))
                                    || (!logical
                                        && matches!(marker(other), Some("UP" | "BACK"))) => {}
                            _ => return Err(invalid("invalid pathname directory element")),
                        }
                        cursor = *next;
                    }
                    _ => return Err(invalid("pathname directory must be a proper list")),
                }
            }
        }
        _ => return Err(invalid("invalid pathname directory component")),
    }

    for leaf in [name, file_type] {
        match get(leaf)? {
            LiteralNode::Nil => {}
            LiteralNode::String(text)
                if valid_segment(text) && (!logical || valid_logical_word(text)) => {}
            other if marker(other) == Some("WILD") => {}
            other if !logical && marker(other) == Some("UNSPECIFIC") => {}
            _ => return Err(invalid("invalid pathname name or type component")),
        }
    }

    match get(version)? {
        LiteralNode::Nil => {}
        LiteralNode::Fixnum(v) if *v > 0 => {}
        other if matches!(marker(other), Some("NEWEST" | "WILD")) => {}
        other if !logical && marker(other) == Some("UNSPECIFIC") => {}
        _ => return Err(invalid("invalid pathname version component")),
    }

    Ok(())
}

fn valid_bignum_magnitude(magnitude: &[u8]) -> bool {
    magnitude.last().is_some_and(|&byte| byte != 0)
}

fn ratio_coprime(numerator: &LiteralNode, denominator: &LiteralNode) -> bool {
    if let (LiteralNode::Fixnum(numerator), LiteralNode::Fixnum(denominator)) =
        (numerator, denominator)
    {
        return numerator.unsigned_abs().gcd(&denominator.unsigned_abs()) == 1;
    }
    let magnitude = |node: &LiteralNode| match node {
        LiteralNode::Fixnum(value) => BigUint::from(value.unsigned_abs()),
        LiteralNode::Bignum { magnitude, .. } => BigUint::from_bytes_le(magnitude),
        _ => unreachable!("ratio components have been checked"),
    };
    magnitude(numerator).gcd(&magnitude(denominator)).is_one()
}
