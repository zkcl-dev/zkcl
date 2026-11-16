use std::hash::{DefaultHasher, Hasher};

use num_bigint::{BigInt, Sign};
use num_rational::BigRational;
use num_traits::{ToPrimitive, Zero};
use rustc_hash::FxHashSet;

use super::numbers::{numerical_equal, real_rational};
use crate::character::simple_lowercase;
use crate::heap::{ArrayElement, ArrayValue, HashTest, Heap, TreelistIter, Word};
use crate::number::{Number, NumberKind, Real};

pub(super) fn eql(heap: &Heap, left: Word, right: Word) -> bool {
    if left == right {
        return true;
    }
    // Fixnums and characters compare directly as words. Boxed numbers
    // compare by type and value.
    let Some(kind) = heap.number_kind(left) else {
        return false;
    };
    if kind == NumberKind::Fixnum || heap.number_kind(right) != Some(kind) {
        return false;
    }
    match (heap.number(left), heap.number(right)) {
        (Some(Number::Real(left)), Some(Number::Real(right))) => real_eql(&left, &right),
        (Some(Number::Complex(lr, li)), Some(Number::Complex(rr, ri))) => {
            real_eql(&lr, &rr) && real_eql(&li, &ri)
        }
        _ => unreachable!("number kind identifies the numeric representation"),
    }
}

fn real_eql(left: &Real, right: &Real) -> bool {
    match (left, right) {
        (Real::Integer(left), Real::Integer(right)) => left == right,
        (Real::Ratio(left), Real::Ratio(right)) => left == right,
        (Real::Single(left), Real::Single(right)) => left.to_bits() == right.to_bits(),
        (Real::Double(left), Real::Double(right)) => left.to_bits() == right.to_bits(),
        _ => false,
    }
}

pub(crate) fn matches(heap: &Heap, left: Word, right: Word, test: HashTest) -> bool {
    match test {
        HashTest::Eq => left == right,
        HashTest::Eql => eql(heap, left, right),
        // The VM roots the inputs and this walk does not reach a GC safepoint.
        HashTest::Equal | HashTest::Equalp => Comparison {
            heap,
            seen: FxHashSet::default(),
            trail: Vec::new(),
        }
        .compare(left, right, test),
    }
}

pub(super) fn equal(heap: &Heap, left: Word, right: Word) -> bool {
    matches(heap, left, right, HashTest::Equal)
}

pub(super) fn equalp(heap: &Heap, left: Word, right: Word) -> bool {
    matches(heap, left, right, HashTest::Equalp)
}

type ComparedPair = (Word, Word, HashTest);

struct HashEntryScan {
    left: Word,
    right: Word,
    test: HashTest,
    cursor: usize,
}

struct HashKeyLookup {
    table: Word,
    key: Word,
    value: Word,
    test: HashTest,
    hash: usize,
    cursor: usize,
}

enum Check<'a> {
    Pair(ComparedPair),
    TreelistSlots {
        cursors: Box<(TreelistIter<'a>, TreelistIter<'a>)>,
        test: HashTest,
    },
    ArraySlots {
        left: Word,
        right: Word,
        index: usize,
        length: usize,
    },
    Entries(HashEntryScan),
    FindKey(HashKeyLookup),
    CompareEntryValue {
        lookup: HashKeyLookup,
        value: Word,
        trail_len: usize,
    },
}

struct Comparison<'a> {
    heap: &'a Heap,
    seen: FxHashSet<ComparedPair>,
    trail: Vec<ComparedPair>,
}

impl<'a> Comparison<'a> {
    fn compare(&mut self, left: Word, right: Word, test: HashTest) -> bool {
        let mut pending = Vec::new();
        if !self.check_pair((left, right, test), &mut pending) {
            return false;
        }
        while let Some(check) = pending.pop() {
            if self.step(check, &mut pending) {
                continue;
            }

            // A structural key comparison can reject a hash collision. Resume
            // the key lookup after dropping assumptions made for that candidate.
            let mut retry = None;
            while let Some(check) = pending.pop() {
                if let Check::CompareEntryValue {
                    lookup, trail_len, ..
                } = check
                {
                    for pair in self.trail.drain(trail_len..) {
                        self.seen.remove(&pair);
                    }
                    retry = Some(lookup);
                    break;
                }
            }
            let Some(lookup) = retry else {
                return false;
            };
            pending.push(Check::FindKey(lookup));
        }
        true
    }

    fn remember(&mut self, pair: ComparedPair) -> bool {
        if !self.seen.insert(pair) {
            return false;
        }
        self.trail.push(pair);
        true
    }

    fn step(&mut self, check: Check<'a>, pending: &mut Vec<Check<'a>>) -> bool {
        match check {
            Check::Pair(pair) => self.check_pair(pair, pending),
            Check::TreelistSlots { mut cursors, test } => {
                let (left, right) = cursors.as_mut();
                match (left.next(), right.next()) {
                    (Some(left), Some(right)) => {
                        pending.push(Check::TreelistSlots { cursors, test });
                        self.check_pair((left, right, test), pending)
                    }
                    (None, None) => true,
                    _ => false,
                }
            }
            Check::ArraySlots {
                left,
                right,
                index,
                length,
            } => {
                if index < length {
                    pending.push(Check::ArraySlots {
                        left,
                        right,
                        index: index + 1,
                        length,
                    });
                    let left = self.heap.array_value(left, index);
                    let right = self.heap.array_value(right, index);
                    return match (left, right) {
                        (Some(ArrayValue::Word(left)), Some(ArrayValue::Word(right))) => {
                            self.check_pair((left, right, HashTest::Equalp), pending)
                        }
                        (Some(left), Some(right)) => self.array_numbers_equal(left, right),
                        _ => false,
                    };
                }
                true
            }
            Check::Entries(mut scan) => {
                if let Some((key, value)) =
                    self.heap.hash_table_next_entry(scan.left, &mut scan.cursor)
                {
                    let lookup = HashKeyLookup {
                        table: scan.right,
                        key,
                        value,
                        test: scan.test,
                        hash: hash(self.heap, key, scan.test),
                        cursor: 0,
                    };
                    pending.push(Check::Entries(scan));
                    pending.push(Check::FindKey(lookup));
                }
                true
            }
            Check::FindKey(mut lookup) => {
                while let Some((key, value)) = self.heap.hash_table_next_candidate(
                    lookup.table,
                    lookup.hash,
                    &mut lookup.cursor,
                ) {
                    if matches!(lookup.test, HashTest::Eq | HashTest::Eql) {
                        if matches(self.heap, lookup.key, key, lookup.test) {
                            pending.push(Check::Pair((lookup.value, value, HashTest::Equalp)));
                            return true;
                        }
                    } else {
                        let pair = (lookup.key, key, lookup.test);
                        pending.push(Check::CompareEntryValue {
                            lookup,
                            value,
                            trail_len: self.trail.len(),
                        });
                        pending.push(Check::Pair(pair));
                        return true;
                    }
                }
                false
            }
            Check::CompareEntryValue { lookup, value, .. } => {
                pending.push(Check::Pair((lookup.value, value, HashTest::Equalp)));
                true
            }
        }
    }

    fn array_numbers_equal(&self, left: ArrayValue, right: ArrayValue) -> bool {
        let number = |value| match value {
            ArrayValue::Word(word) => self.heap.number(word),
            ArrayValue::Octet(byte) => self
                .heap
                .number(Word::fixnum(isize::from(byte)).expect("octet fixnum")),
            ArrayValue::SingleFloat(value) => Some(Number::Real(Real::Single(value))),
            ArrayValue::DoubleFloat(value) => Some(Number::Real(Real::Double(value))),
        };
        match (number(left), number(right)) {
            (Some(left), Some(right)) => numerical_equal(&left, &right),
            _ => false,
        }
    }

    fn check_pair(&mut self, pair: ComparedPair, pending: &mut Vec<Check<'a>>) -> bool {
        let (left, right, test) = pair;
        if left == right {
            return true;
        }
        let heap = self.heap;
        if matches!(test, HashTest::Equal | HashTest::Equalp) {
            let treelist_comparable = match test {
                HashTest::Equal => heap.is_treelist(left) && heap.is_treelist(right),
                HashTest::Equalp => {
                    heap.treelist_len(left).is_some() && heap.treelist_len(right).is_some()
                }
                _ => false,
            };
            if treelist_comparable {
                if heap.treelist_node(left) == heap.treelist_node(right) {
                    return true;
                }
                if heap.treelist_len(left) != heap.treelist_len(right) {
                    return false;
                }
                if self.remember(pair) {
                    pending.push(Check::TreelistSlots {
                        cursors: Box::new((heap.treelist_iter(left), heap.treelist_iter(right))),
                        test,
                    });
                }
                return true;
            }
        }
        if test == HashTest::Equalp {
            if left.as_fixnum().is_some() && right.as_fixnum().is_some() {
                return false;
            }
            if let Some(left) = heap.number(left) {
                return heap
                    .number(right)
                    .is_some_and(|right| numerical_equal(&left, &right));
            }
        } else if eql(heap, left, right) {
            return true;
        }
        if let (Some((left_car, left_cdr)), Some((right_car, right_cdr))) =
            (heap.cons_fields(left), heap.cons_fields(right))
        {
            if self.remember(pair) {
                pending.push(Check::Pair((left_cdr, right_cdr, test)));
                pending.push(Check::Pair((left_car, right_car, test)));
            }
            return true;
        }
        if let (Some(left_fields), Some(right_fields)) =
            (heap.pathname_fields(left), heap.pathname_fields(right))
        {
            if self.remember(pair) {
                // Pathnames compare using the filesystem's local case.
                pending.extend(
                    left_fields
                        .into_iter()
                        .zip(right_fields)
                        .map(|(left, right)| Check::Pair((left, right, HashTest::Equal))),
                );
            }
            return true;
        }
        let left_array = heap.array_info(left);
        let right_array = heap.array_info(right);
        if let (Some(left_info), Some(right_info)) = (left_array, right_array) {
            let left_length = left_info.fill_pointer.unwrap_or(left_info.size);
            let right_length = right_info.fill_pointer.unwrap_or(right_info.size);
            if left_info.rank == 1
                && right_info.rank == 1
                && left_info.element == ArrayElement::Character
                && right_info.element == ArrayElement::Character
            {
                return strings_equal(heap, left, right, test);
            }
            if test == HashTest::Equalp {
                if left_info.rank != right_info.rank
                    || left_length != right_length
                    || left_info.rank != 1
                        && heap.array_dimensions(left) != heap.array_dimensions(right)
                {
                    return false;
                }
                if self.remember(pair) {
                    pending.push(Check::ArraySlots {
                        left,
                        right,
                        index: 0,
                        length: left_length,
                    });
                }
                return true;
            }
            if left_info.rank == 1
                && right_info.rank == 1
                && left_info.element == ArrayElement::Bit
                && right_info.element == ArrayElement::Bit
            {
                return left_length == right_length
                    && (0..left_length).all(|index| {
                        matches!(
                            (heap.array_value(left, index), heap.array_value(right, index)),
                            (Some(ArrayValue::Word(a)), Some(ArrayValue::Word(b))) if a == b
                        )
                    });
            }
        }
        if test == HashTest::Equalp {
            if let (Some(left), Some(right)) = (left.as_character(), right.as_character()) {
                return simple_lowercase(left) == simple_lowercase(right);
            }
            if let (Some(left_info), Some(right_info)) =
                (heap.hash_table_info(left), heap.hash_table_info(right))
            {
                if left_info.count != right_info.count || left_info.test != right_info.test {
                    return false;
                }
                if self.remember(pair) {
                    pending.push(Check::Entries(HashEntryScan {
                        left,
                        right,
                        test: left_info.test,
                        cursor: 0,
                    }));
                }
                return true;
            }
            if let (Some(left_layout), Some(right_layout)) =
                (heap.structure_layout(left), heap.structure_layout(right))
            {
                if left_layout != right_layout {
                    return false;
                }
                if self.remember(pair) {
                    for index in (0..left_layout.1).rev() {
                        let left = heap.structure_slot(left, index).expect("slot in layout");
                        let right = heap.structure_slot(right, index).expect("slot in layout");
                        pending.push(Check::Pair((left, right, HashTest::Equalp)));
                    }
                }
                return true;
            }
        }
        false
    }
}

fn strings_equal(heap: &Heap, left: Word, right: Word, test: HashTest) -> bool {
    heap.with_two_string_slices(left, right, |left, right| {
        if test != HashTest::Equalp {
            return left == right;
        }
        if left.is_ascii() && right.is_ascii() {
            return left.eq_ignore_ascii_case(right);
        }
        left.chars()
            .map(simple_lowercase)
            .eq(right.chars().map(simple_lowercase))
    })
    .unwrap_or(false)
}

pub(crate) fn hash(heap: &Heap, key: Word, test: HashTest) -> usize {
    if test == HashTest::Eq || test == HashTest::Eql && heap.number_kind(key).is_none() {
        let mut hash = DefaultHasher::new();
        hash.write_usize(heap.identity_hash(key));
        return hash.finish() as usize;
    }
    StructuralHash::new(heap, false).finish(key, test) as usize
}

pub(super) fn sxhash(heap: &Heap, value: Word) -> Word {
    let bits = StructuralHash::new(heap, true).finish(value, HashTest::Equal);
    let masked = bits & ((isize::MAX >> 3) as u64);
    Word::fixnum(masked as isize).expect("masked hash fits a nonnegative fixnum")
}

struct StructuralHash<'a> {
    heap: &'a Heap,
    hash: DefaultHasher,
    nodes_left: usize,
    for_sxhash: bool,
}

impl<'a> StructuralHash<'a> {
    fn new(heap: &'a Heap, for_sxhash: bool) -> Self {
        Self {
            heap,
            hash: DefaultHasher::new(),
            nodes_left: 256,
            for_sxhash,
        }
    }

    fn finish(mut self, value: Word, test: HashTest) -> u64 {
        self.value(value, test, 8);
        self.hash.finish()
    }

    fn value(&mut self, value: Word, test: HashTest, depth: usize) {
        // Bound structural work without depending on sharing or identity.
        // Equal graphs produce the same prefix even when one shares a subtree
        // and the other contains two copies. Circular inputs also terminate.
        if depth == 0 || self.nodes_left == 0 {
            self.hash.write_u8(0);
            return;
        }
        self.nodes_left -= 1;

        if let Some(number) = value.as_fixnum() {
            self.small_integer(number as i64);
        } else if let Some(character) = value.as_character() {
            self.hash.write_u8(2);
            let character = if test == HashTest::Equalp {
                simple_lowercase(character)
            } else {
                character
            };
            self.hash.write(&u32::from(character).to_le_bytes());
        } else if let Some(number) = self.heap.number(value) {
            self.number(&number, test);
        } else if self
            .heap
            .with_string_slice(value, |text| {
                if test == HashTest::Equalp {
                    let dimensions = [text.chars().count()];
                    self.array(
                        &dimensions,
                        text.chars().map(|ch| ArrayValue::Word(Word::character(ch))),
                        HashTest::Equalp,
                        depth - 1,
                    );
                } else {
                    self.hash.write_u8(3);
                    self.text(text);
                }
            })
            .is_some()
        {
        } else if let Some(info) = self.heap.array_info(value)
            && (test == HashTest::Equalp || info.rank == 1 && info.element == ArrayElement::Bit)
        {
            let heap = self.heap;
            let length = info.fill_pointer.unwrap_or(info.size);
            let mut dimensions = heap.array_dimensions(value).expect("array dimensions");
            if info.rank == 1 {
                dimensions[0] = length;
            }
            let slots = (0..length).map(|index| {
                heap.array_value(value, index)
                    .unwrap_or(ArrayValue::Word(Word::NIL))
            });
            self.array(&dimensions, slots, test, depth - 1);
        } else if let Some((head, tail)) = self.heap.cons_fields(value) {
            self.hash.write_u8(4);
            self.value(head, test, depth - 1);
            self.value(tail, test, depth - 1);
        } else if let Some(fields) = self.heap.pathname_fields(value) {
            self.hash.write_u8(5);
            for field in fields {
                self.value(field, HashTest::Equal, depth - 1);
            }
        } else if (test == HashTest::Equalp && self.heap.treelist_len(value).is_some())
            || (test == HashTest::Equal && self.heap.is_treelist(value))
        {
            let len = self.heap.treelist_len(value).expect("treelist");
            self.hash.write_u8(16);
            self.hash.write(&(len as u64).to_le_bytes());
            for elem in self.heap.treelist_iter(value) {
                if self.nodes_left == 0 {
                    break;
                }
                self.value(elem, test, depth - 1);
            }
        } else if test == HashTest::Equalp
            && let Some(info) = self.heap.hash_table_info(value)
        {
            // Tables can be keys, including in a cyclic literal graph.
            // The test stays fixed while the loader fills the entries.
            self.hash.write_u8(6);
            self.hash.write_u8(match info.test {
                HashTest::Eq => 0,
                HashTest::Eql => 1,
                HashTest::Equal => 2,
                HashTest::Equalp => 3,
            });
        } else if test == HashTest::Equalp
            && let Some((class, slots)) = self.heap.structure_layout(value)
        {
            // The class and slot count stay fixed while slots are initialized.
            self.hash.write_u8(10);
            self.hash.write_usize(self.heap.identity_hash(class));
            self.hash.write_usize(slots);
        } else if self.for_sxhash {
            if let Some(name) = self.heap.symbol_name(value) {
                // Package changes are invisible to EQUAL and must not affect SXHASH.
                self.hash.write_u8(7);
                self.text(&name);
            } else {
                // Opaque objects can be EQ; a fixed tag also keeps their
                // containing structures stable across Lisp images.
                self.hash.write_u8(8);
            }
        } else {
            self.hash.write_u8(9);
            self.hash.write_usize(self.heap.identity_hash(value));
        }
    }

    fn array(
        &mut self,
        dimensions: &[usize],
        slots: impl Iterator<Item = ArrayValue>,
        test: HashTest,
        depth: usize,
    ) {
        self.hash.write_u8(11);
        self.hash.write(&(dimensions.len() as u64).to_le_bytes());
        for dimension in dimensions {
            self.hash.write(&(*dimension as u64).to_le_bytes());
        }
        for slot in slots.take(self.nodes_left) {
            match slot {
                ArrayValue::Word(word) => self.value(word, test, depth),
                ArrayValue::Octet(byte) => self.value(
                    Word::fixnum(isize::from(byte)).expect("octet fixnum"),
                    test,
                    depth,
                ),
                _ if depth == 0 => self.hash.write_u8(0),
                ArrayValue::SingleFloat(value) => {
                    self.nodes_left -= 1;
                    self.real(&Real::Single(value), test);
                }
                ArrayValue::DoubleFloat(value) => {
                    self.nodes_left -= 1;
                    self.real(&Real::Double(value), test);
                }
            }
            if self.nodes_left == 0 {
                break;
            }
        }
    }

    fn text(&mut self, text: &str) {
        self.hash.write(&(text.len() as u64).to_le_bytes());
        self.hash.write(text.as_bytes());
    }

    fn small_integer(&mut self, value: i64) {
        self.hash.write_u8(1);
        self.hash.write(&value.to_le_bytes());
    }

    fn integer_bytes(&mut self, value: &BigInt) {
        let (sign, bytes) = value.to_bytes_le();
        self.hash.write_u8(match sign {
            Sign::Minus => 0,
            Sign::NoSign => 1,
            Sign::Plus => 2,
        });
        self.hash.write(&(bytes.len() as u64).to_le_bytes());
        self.hash.write(&bytes);
    }

    fn integer(&mut self, value: &BigInt) {
        if let Some(value) = value.to_i64() {
            self.small_integer(value);
        } else {
            self.hash.write_u8(11);
            self.integer_bytes(value);
        }
    }

    fn ratio(&mut self, value: &BigRational) {
        if value.is_integer() {
            self.integer(value.numer());
        } else {
            self.hash.write_u8(12);
            self.integer_bytes(value.numer());
            self.integer_bytes(value.denom());
        }
    }

    fn real(&mut self, real: &Real, test: HashTest) {
        match real {
            Real::Integer(value) => self.integer(value),
            Real::Ratio(value) => self.ratio(value),
            value if test == HashTest::Equalp => self.ratio(&real_rational(value)),
            Real::Single(value) => {
                self.hash.write_u8(13);
                let bits = if self.for_sxhash && *value == 0.0 {
                    0
                } else {
                    value.to_bits()
                };
                self.hash.write(&bits.to_le_bytes());
            }
            Real::Double(value) => {
                self.hash.write_u8(14);
                let bits = if self.for_sxhash && *value == 0.0 {
                    0
                } else {
                    value.to_bits()
                };
                self.hash.write(&bits.to_le_bytes());
            }
        }
    }

    fn number(&mut self, number: &Number, test: HashTest) {
        match number {
            Number::Real(real) => self.real(real, test),
            Number::Complex(real, imaginary) => {
                if test == HashTest::Equalp {
                    let imaginary = real_rational(imaginary);
                    if imaginary.is_zero() {
                        self.real(real, HashTest::Equalp);
                        return;
                    }
                    self.hash.write_u8(15);
                    self.real(real, HashTest::Equalp);
                    self.ratio(&imaginary);
                } else {
                    self.hash.write_u8(15);
                    self.real(real, test);
                    self.real(imaginary, test);
                }
            }
        }
    }
}
