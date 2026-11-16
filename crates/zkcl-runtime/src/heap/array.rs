use std::ptr;
use std::sync::atomic::AtomicBool;

use super::word::{GC_FLAG_MATURE, GC_FLAG_UNLOGGED};
use super::{
    Address, AtomicUsize, Header, Heap, HeapObject, ObjectKind, Ordering, OutOfMemory, SlotVisitor,
    ValueSlot, Word, WordSlot, offset_of, trailing_base,
};
use crate::number::{Number, Real};

pub(crate) use zkcl_bytecode::ArrayElement;

#[repr(C)]
struct Array {
    header: Header,
    backing: WordSlot,
    rank: usize,
    total: AtomicUsize,
    element: ArrayElement,
    nil_element: AtomicBool,
    fill: AtomicUsize,
    adjustable: bool,
    displacement: AtomicUsize,
}

// SAFETY: `Array` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Array`.
unsafe impl HeapObject for Array {
    const KIND: ObjectKind = ObjectKind::Array;
}

impl Array {
    fn dimensions(&self, address: Address) -> &[AtomicUsize] {
        // SAFETY: The address is this array's allocation, including its trailing dimensions.
        let dimensions = trailing_base::<Self>(address).to_ptr::<AtomicUsize>();
        unsafe { std::slice::from_raw_parts(dimensions, self.rank) }
    }
}

// The trailing storage can contain 64-bit numeric elements.
#[repr(C, align(8))]
struct ArrayStorage {
    header: Header,
    count: usize,
    element: ArrayElement,
    // Large general arrays store a dirty-card bitmap after their element slots.
}

// SAFETY: `ArrayStorage` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::ArrayStorage`.
unsafe impl HeapObject for ArrayStorage {
    const KIND: ObjectKind = ObjectKind::ArrayStorage;
}

const BITS_PER_WORD: usize = usize::BITS as usize;
const CARD_SLOTS: usize = 512 / size_of::<AtomicUsize>();
const CARD_ARRAY_MIN_SLOTS: usize = super::immix::MAX_IMMIX_OBJECT_BYTES / ValueSlot::STRIDE;

impl ArrayStorage {
    #[inline]
    fn card_word(&self, base: Address, card: usize) -> &AtomicUsize {
        let index = self.count + card / BITS_PER_WORD;
        let slots = trailing_base::<Self>(base).to_ptr::<AtomicUsize>();
        // SAFETY: only general arrays with an initialized card bitmap use this
        // method, and `card` was derived from an in-bounds element index.
        unsafe { &*slots.add(index) }
    }
}

/// One payload card remembered across the next stop-the-world collection.
pub(super) struct RememberedCard {
    owner: Address,
    index: usize,
}

impl RememberedCard {
    fn storage(&self) -> &ArrayStorage {
        // SAFETY: only a mature large general array can register a card, and
        // the collector drains the log before reclaiming any of those arrays.
        unsafe { &*self.owner.to_ptr::<ArrayStorage>() }
    }

    pub(super) fn clear(&self) {
        // The entire card log is drained with the mutator stopped. Neighboring
        // cards in this bitmap word retain their independent log entries.
        let word = self.storage().card_word(self.owner, self.index);
        let bit = 1 << (self.index % BITS_PER_WORD);
        word.fetch_and(!bit, Ordering::Relaxed);
    }

    pub(super) fn visit_slots(&self, visitor: &mut impl SlotVisitor) {
        let first = self.index * CARD_SLOTS;
        let count = (self.storage().count - first).min(CARD_SLOTS);
        let start = trailing_base::<ArrayStorage>(self.owner) + first * ValueSlot::STRIDE;
        for slot in ValueSlot::array(start, count) {
            visitor.visit_slot(slot);
        }
    }
}

#[derive(Clone, Copy)]
struct NumericSpan {
    backing: Address,
    start: usize,
}

impl NumericSpan {
    fn overlaps(self, other: Self, len: usize) -> bool {
        self.backing == other.backing && self.start.abs_diff(other.start) < len
    }

    fn raw_f32(self) -> *mut f32 {
        self.backing.to_mut_ptr::<f32>().wrapping_add(self.start)
    }

    fn raw_bytes(self, width: usize) -> *mut u8 {
        // The index is in a materialized allocation whose size included
        // a checked multiplication by its element width.
        self.backing
            .to_mut_ptr::<u8>()
            .wrapping_add(self.start * width)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum F32SpanError {
    Bounds { array: usize, index: usize },
    PartialOverlap { input: usize },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SimpleVectorError {
    NotSimpleVector,
    OutOfBounds { len: usize },
}

/// A row-major element as stored, with floats still unboxed.
#[derive(Clone, Copy)]
pub(crate) enum ArrayValue {
    Word(Word),
    Octet(u8),
    SingleFloat(f32),
    DoubleFloat(f64),
}

impl ArrayValue {
    pub(crate) fn from_word(heap: &Heap, element: ArrayElement, value: Word) -> Option<Self> {
        match element {
            ArrayElement::General => Some(Self::Word(value)),
            ArrayElement::Bit => {
                matches!(value.as_fixnum(), Some(0 | 1)).then_some(Self::Word(value))
            }
            ArrayElement::Character => value.as_character().map(|_| Self::Word(value)),
            ArrayElement::Octet => u8::try_from(value.as_fixnum()?).ok().map(Self::Octet),
            ArrayElement::SingleFloat => heap.single_float(value).map(Self::SingleFloat),
            ArrayElement::DoubleFloat => heap.double_float(value).map(Self::DoubleFloat),
        }
    }

    pub(crate) fn boxed(self, heap: &Heap) -> Result<Word, OutOfMemory> {
        let real = match self {
            Self::SingleFloat(value) => Real::Single(value),
            Self::DoubleFloat(value) => Real::Double(value),
            other => return Ok(other.word_without_boxing().expect("stored word or octet")),
        };
        heap.allocate_number(&Number::Real(real))
    }

    #[inline]
    pub(crate) fn word_without_boxing(self) -> Option<Word> {
        match self {
            Self::Word(word) => Some(word),
            Self::Octet(byte) => Some(Word::fixnum(byte.into()).expect("octet fits in fixnum")),
            Self::SingleFloat(_) | Self::DoubleFloat(_) => None,
        }
    }

    pub(crate) fn expect_word(self) -> Word {
        let Self::Word(word) = self else {
            unreachable!("validated array storage type");
        };
        word
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ArrayLayout {
    pub(crate) rank: usize,
    pub(crate) size: usize,
    pub(crate) element: ArrayElement,
    pub(crate) nil_element: bool,
    pub(crate) fill_pointer: Option<usize>,
    pub(crate) adjustable: bool,
    pub(crate) displacement: Option<(Word, usize)>,
}

impl ArrayLayout {
    pub(crate) fn simple(self) -> bool {
        !self.adjustable && self.displacement.is_none() && self.fill_pointer.is_none()
    }
}

struct Layout<'a> {
    dimensions: &'a [usize],
    element: ArrayElement,
    adjustable: bool,
    fill_pointer: Option<usize>,
}

impl Layout<'_> {
    fn size(&self) -> Result<usize, OutOfMemory> {
        if self.dimensions.contains(&0) {
            return Ok(0);
        }
        self.dimensions
            .iter()
            .try_fold(1_usize, |total, &dimension| {
                total.checked_mul(dimension).ok_or(OutOfMemory)
            })
    }
}

impl Heap {
    /// The callback reads validated, rooted inputs without allocating.
    pub(crate) fn array(
        &self,
        dimensions: &[usize],
        element: ArrayElement,
        adjustable: bool,
        fill_pointer: Option<usize>,
        mut values: impl FnMut(usize) -> Word,
    ) -> Result<Word, OutOfMemory> {
        self.array_from_values(dimensions, element, adjustable, fill_pointer, |index| {
            ArrayValue::from_word(self, element, values(index)).expect("validated array element")
        })
    }

    /// Reads elements without allocating, including during the UTF-8 sizing pass.
    pub(crate) fn array_from_values(
        &self,
        dimensions: &[usize],
        element: ArrayElement,
        adjustable: bool,
        fill_pointer: Option<usize>,
        mut values: impl FnMut(usize) -> ArrayValue,
    ) -> Result<Word, OutOfMemory> {
        let layout = Layout {
            dimensions,
            element,
            adjustable,
            fill_pointer,
        };
        let count = layout.size()?;
        if element == ArrayElement::Character {
            let data = self.string_from_characters(count, |index| {
                values(index)
                    .expect_word()
                    .as_character()
                    .expect("validated character")
            })?;
            if dimensions.len() == 1 && !adjustable && fill_pointer.is_none() {
                return Ok(data);
            }
            let data = self.root_slot(data);
            return self.array_header(layout, count, None, false, || data.get());
        }
        let data = match element {
            ArrayElement::General => {
                let bitmap_words = if count >= CARD_ARRAY_MIN_SLOTS {
                    count.div_ceil(CARD_SLOTS * BITS_PER_WORD)
                } else {
                    0
                };
                let slots = count.checked_add(bitmap_words).ok_or(OutOfMemory)?;
                self.allocate_array_data(count, element, slots, |index| {
                    AtomicUsize::new(if index < count {
                        values(index).expect_word().0
                    } else {
                        0
                    })
                })?
            }
            ArrayElement::Bit => {
                self.allocate_array_data(count, element, count.div_ceil(BITS_PER_WORD), |word| {
                    let start = word * BITS_PER_WORD;
                    let end = start + (count - start).min(BITS_PER_WORD);
                    let bits = (start..end).fold(0, |bits, index| {
                        let bit = values(index)
                            .expect_word()
                            .as_fixnum()
                            .expect("validated bit");
                        debug_assert!(matches!(bit, 0 | 1));
                        bits | ((bit as usize) << (index - start))
                    });
                    AtomicUsize::new(bits)
                })?
            }
            ArrayElement::Octet => self.allocate_array_data(count, element, count, |index| {
                let ArrayValue::Octet(value) = values(index) else {
                    unreachable!("validated octet array storage");
                };
                value
            })?,
            ArrayElement::SingleFloat => {
                self.allocate_array_data(count, element, count, |index| {
                    let ArrayValue::SingleFloat(value) = values(index) else {
                        unreachable!("validated single-float array storage");
                    };
                    value.to_bits()
                })?
            }
            ArrayElement::DoubleFloat => {
                self.allocate_array_data(count, element, count, |index| {
                    let ArrayValue::DoubleFloat(value) = values(index) else {
                        unreachable!("validated double-float array storage");
                    };
                    value.to_bits()
                })?
            }
            ArrayElement::Character => unreachable!("character storage is UTF-8"),
        };
        let data = self.root_slot(data);
        self.array_header(layout, count, None, false, || data.get())
    }

    fn allocate_array_data<T>(
        &self,
        count: usize,
        element: ArrayElement,
        slots: usize,
        value: impl FnMut(usize) -> T,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_with_trailing(
            slots,
            |header| ArrayStorage {
                header,
                count,
                element,
            },
            value,
        )
    }

    pub(crate) fn octet_vector(&self, bytes: &[u8]) -> Result<Word, OutOfMemory> {
        self.array_from_values(&[bytes.len()], ArrayElement::Octet, false, None, |index| {
            ArrayValue::Octet(bytes[index])
        })
    }

    pub(crate) fn octet_vector_contents(&self, word: Word) -> Option<Vec<u8>> {
        let info = self.array_info(word)?;
        if info.rank != 1 || info.element != ArrayElement::Octet {
            return None;
        }
        self.octet_array_range(word, 0, info.fill_pointer.unwrap_or(info.size))
    }

    pub(crate) fn octet_array_range(
        &self,
        word: Word,
        start: usize,
        count: usize,
    ) -> Option<Vec<u8>> {
        self.with_array_octet_slice(word, start, count, <[u8]>::to_vec)
    }

    /// Resolve a checked, possibly displaced octet range. The address is valid
    /// only until the next safepoint or modification of the array's backing.
    pub(crate) fn octet_array_address(
        &self,
        word: Word,
        start: usize,
        count: usize,
    ) -> Option<*mut u8> {
        Some(
            self.numeric_span(word, ArrayElement::Octet, start, count)?
                .raw_bytes(1),
        )
    }

    /// Copies active array elements into the corresponding little-endian pack lanes.
    /// A failed range check leaves `bytes` unchanged and returns the first invalid lane.
    pub(crate) fn load_numeric_pack(
        &self,
        word: Word,
        element: ArrayElement,
        start: usize,
        mask: u64,
        bytes: &mut [u8],
    ) -> Result<(), usize> {
        let width = numeric_width(element);
        let Some((span, len)) = self.numeric_pack_span(word, element, start, mask, bytes.len())?
        else {
            return Ok(());
        };
        let base = span.raw_bytes(width).cast_const();
        if cfg!(target_endian = "little") && mask.count_ones() as usize == len {
            // SAFETY: All lanes in this prefix are active. The complete source range
            // was checked, the destination is independent of the heap, and no call can collect.
            unsafe { ptr::copy_nonoverlapping(base, bytes.as_mut_ptr(), len * width) };
        } else {
            for lane in active_lanes(mask) {
                let offset = lane * width;
                // SAFETY: The checked span covers every active lane. This copies that
                // lane only, into the independent destination, without a safepoint.
                unsafe {
                    copy_numeric_lane(base.add(offset), bytes.as_mut_ptr().add(offset), width)
                };
            }
        }
        Ok(())
    }

    /// Checks all active lanes before copying any little-endian lane into the array.
    pub(crate) fn store_numeric_pack(
        &self,
        word: Word,
        element: ArrayElement,
        start: usize,
        mask: u64,
        bytes: &[u8],
    ) -> Result<(), usize> {
        let width = numeric_width(element);
        let Some((span, len)) = self.numeric_pack_span(word, element, start, mask, bytes.len())?
        else {
            return Ok(());
        };
        let base = span.raw_bytes(width);
        if cfg!(target_endian = "little") && mask.count_ones() as usize == len {
            // SAFETY: All lanes in this prefix are active. Its live owned backing was
            // checked, the source is outside the heap, and the copy cannot collect.
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), base, len * width) };
        } else {
            for lane in active_lanes(mask) {
                let offset = lane * width;
                // SAFETY: The checked span covers every active lane and only the owning
                // mutator can touch it; this copy cannot collect or re-enter the runtime.
                unsafe { copy_numeric_lane(bytes.as_ptr().add(offset), base.add(offset), width) };
            }
        }
        Ok(())
    }

    fn numeric_pack_span(
        &self,
        word: Word,
        element: ArrayElement,
        start: usize,
        mask: u64,
        bytes: usize,
    ) -> Result<Option<(NumericSpan, usize)>, usize> {
        if mask == 0 {
            return Ok(None);
        }
        let len = u64::BITS as usize - mask.leading_zeros() as usize;
        assert!(len <= bytes / numeric_width(element));
        if let Some(span) = self.numeric_span(word, element, start, len) {
            return Ok(Some((span, len)));
        }
        // Each displacement is affine with a nonnegative offset. A span through
        // the highest active lane proves all active lanes; on failure, find the
        // first active lane whose prefix does not fit.
        Err(active_lanes(mask)
            .find(|&lane| self.numeric_span(word, element, start, lane + 1).is_none())
            .expect("highest active lane did not fit"))
    }

    pub(crate) fn gather_numeric_pack(
        &self,
        word: Word,
        element: ArrayElement,
        indices: &[Option<usize>],
        mask: u64,
        bytes: &mut [u8],
    ) -> Result<(), usize> {
        let width = numeric_width(element);
        let spans = self.numeric_pack_indices(word, element, indices, mask, bytes.len())?;
        for lane in active_lanes(mask) {
            let span = spans[lane].expect("checked active lane");
            // SAFETY: The source element is checked and live. A checked lane in
            // the independent destination receives exactly one element's bytes.
            unsafe {
                copy_numeric_lane(
                    span.raw_bytes(width),
                    bytes.as_mut_ptr().add(lane * width),
                    width,
                )
            };
        }
        Ok(())
    }

    pub(crate) fn scatter_numeric_pack(
        &self,
        word: Word,
        element: ArrayElement,
        indices: &[Option<usize>],
        mask: u64,
        bytes: &[u8],
    ) -> Result<(), usize> {
        let width = numeric_width(element);
        let spans = self.numeric_pack_indices(word, element, indices, mask, bytes.len())?;
        for lane in active_lanes(mask) {
            let span = spans[lane].expect("checked active lane");
            // SAFETY: The destination element and input lane were checked before
            // any writes. No reference to the payload escapes, and no call can collect.
            unsafe {
                copy_numeric_lane(
                    bytes.as_ptr().add(lane * width),
                    span.raw_bytes(width),
                    width,
                )
            };
        }
        Ok(())
    }

    fn numeric_pack_indices(
        &self,
        word: Word,
        element: ArrayElement,
        indices: &[Option<usize>],
        mask: u64,
        bytes: usize,
    ) -> Result<[Option<NumericSpan>; 64], usize> {
        assert!(
            u64::BITS as usize - mask.leading_zeros() as usize <= bytes / numeric_width(element)
        );
        let mut spans = [None; 64];
        for lane in active_lanes(mask) {
            let index = indices[lane].ok_or(lane)?;
            spans[lane] = Some(self.numeric_span(word, element, index, 1).ok_or(lane)?);
        }
        Ok(spans)
    }

    fn numeric_span(
        &self,
        word: Word,
        element: ArrayElement,
        start: usize,
        len: usize,
    ) -> Option<NumericSpan> {
        let (data, start) = self.array_storage(word, start, len)?;
        let (backing, stored_element) = self.array_data_payload(data)?;
        (element == stored_element).then_some(NumericSpan { backing, start })
    }

    pub(crate) fn check_f32_array_spans(
        &self,
        ranges: [(Word, usize); 3],
        len: usize,
    ) -> Result<(), F32SpanError> {
        self.f32_array_spans(ranges, len).map(|_| ())
    }

    fn f32_array_spans(
        &self,
        ranges: [(Word, usize); 3],
        len: usize,
    ) -> Result<[NumericSpan; 3], F32SpanError> {
        let resolve = |array: usize| {
            let (word, start) = ranges[array];
            let element = ArrayElement::SingleFloat;
            if let Some(span) = self.numeric_span(word, element, start, len) {
                return Ok(span);
            }
            let (mut low, mut high) = (0, len);
            while high - low > 1 {
                let middle = low + (high - low) / 2;
                if self.numeric_span(word, element, start, middle).is_some() {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            // A nonzero low was accepted, including the checked start + low.
            Err(F32SpanError::Bounds {
                array,
                index: start + low,
            })
        };
        let [output, left, right] = [resolve(0)?, resolve(1)?, resolve(2)?];
        for (input, source) in [(1, left), (2, right)] {
            if output.start != source.start && output.overlaps(source, len) {
                return Err(F32SpanError::PartialOverlap { input });
            }
        }
        Ok([output, left, right])
    }

    /// Runs a context-free leaf over checked spans in output, left, right order.
    ///
    /// # Safety
    ///
    /// The leaf may access only the supplied lengths and may write only through the
    /// output pointer. The output can exactly alias either input. The leaf must avoid
    /// allocation, Lisp or host calls, array adjustment, and retaining any pointer or reference.
    pub(crate) unsafe fn visit_f32_array_spans<T, R>(
        &self,
        ranges: [(Word, usize); 3],
        len: usize,
        argument: T,
        leaf: unsafe fn(*mut f32, *const f32, *const f32, T, usize) -> R,
    ) -> Result<R, F32SpanError> {
        let no_gc = self.immix.borrow();
        let [output, left, right] = self.f32_array_spans(ranges, len)?;
        // SAFETY: All three spans were checked, including aliasing. Collection
        // needs a mutable Immix borrow, and the leaf obeys the stated access rules.
        let result = unsafe {
            leaf(
                output.raw_f32(),
                left.raw_f32(),
                right.raw_f32(),
                argument,
                len,
            )
        };
        drop(no_gc);
        Ok(result)
    }

    pub(crate) fn displaced_array(
        &self,
        dimensions: &[usize],
        element: ArrayElement,
        adjustable: bool,
        fill_pointer: Option<usize>,
        offset: usize,
        target: impl FnOnce() -> Word,
    ) -> Result<Word, OutOfMemory> {
        let layout = Layout {
            dimensions,
            element,
            adjustable,
            fill_pointer,
        };
        let count = layout.size()?;
        self.array_header(layout, count, Some(offset), false, target)
    }

    fn array_header(
        &self,
        layout: Layout<'_>,
        total: usize,
        displacement: Option<usize>,
        nil_element: bool,
        backing: impl FnOnce() -> Word,
    ) -> Result<Word, OutOfMemory> {
        let rank = layout.dimensions.len();
        self.alloc_with_trailing(
            rank,
            |header| Array {
                header,
                backing: WordSlot::new(backing()),
                rank,
                total: AtomicUsize::new(total),
                element: layout.element,
                nil_element: AtomicBool::new(nil_element),
                fill: AtomicUsize::new(layout.fill_pointer.unwrap_or(usize::MAX)),
                adjustable: layout.adjustable,
                displacement: AtomicUsize::new(displacement.unwrap_or(usize::MAX)),
            },
            |axis| AtomicUsize::new(layout.dimensions[axis]),
        )
    }

    pub(crate) fn array_info(&self, word: Word) -> Option<ArrayLayout> {
        if let Some(size) = self.string_data_count(word) {
            return Some(ArrayLayout {
                rank: 1,
                size,
                element: ArrayElement::Character,
                nil_element: false,
                fill_pointer: None,
                adjustable: false,
                displacement: None,
            });
        }
        let array = self.object_ref::<Array>(word)?;
        let fill = array.fill.load(Ordering::Relaxed);
        let displacement = array.displacement.load(Ordering::Relaxed);
        Some(ArrayLayout {
            rank: array.rank,
            size: array.total.load(Ordering::Relaxed),
            element: array.element,
            nil_element: array.nil_element.load(Ordering::Relaxed),
            fill_pointer: (fill != usize::MAX).then_some(fill),
            adjustable: array.adjustable,
            displacement: (displacement != usize::MAX).then(|| (array.backing.get(), displacement)),
        })
    }

    pub(crate) fn array_dimensions(&self, word: Word) -> Option<Vec<usize>> {
        if let Some(size) = self.string_data_count(word) {
            return Some(vec![size]);
        }
        let (address, array) = self.object_with_address::<Array>(word)?;
        Some(
            array
                .dimensions(address)
                .iter()
                .map(|dimension| dimension.load(Ordering::Relaxed))
                .collect(),
        )
    }

    pub(crate) fn replace_adjustable_array(&self, original: Word, replacement: Word) -> Option<()> {
        let (original_address, original) = self.object_with_address::<Array>(original)?;
        let (replacement_address, replacement) = self.object_with_address::<Array>(replacement)?;
        if !original.adjustable
            || original.rank != replacement.rank
            || original.element != replacement.element
            || original.nil_element.load(Ordering::Relaxed)
                != replacement.nil_element.load(Ordering::Relaxed)
        {
            return None;
        }
        for (dimension, replacement_dimension) in original
            .dimensions(original_address)
            .iter()
            .zip(replacement.dimensions(replacement_address))
        {
            dimension.store(
                replacement_dimension.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
        for (field, replacement_field) in [
            (&original.total, &replacement.total),
            (&original.fill, &replacement.fill),
            (&original.displacement, &replacement.displacement),
        ] {
            field.store(replacement_field.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.write_slot_object(original, &original.backing, replacement.backing.get());
        Some(())
    }

    /// Allocates an array whose element type is NIL. A rank-1 array stores
    /// characters and is therefore a string; other ranks have no elements.
    pub(crate) fn nil_element_array(
        &self,
        dimensions: &[usize],
        adjustable: bool,
        fill_pointer: Option<usize>,
        count: usize,
        values: impl Fn(usize) -> char,
    ) -> Result<Word, OutOfMemory> {
        if dimensions.len() == 1 {
            let layout = Layout {
                dimensions,
                element: ArrayElement::Character,
                adjustable,
                fill_pointer,
            };
            let data = self.string_from_characters(count, values)?;
            let data = self.root_slot(data);
            return self.array_header(layout, count, None, true, || data.get());
        }
        let word = self.array(
            dimensions,
            ArrayElement::General,
            adjustable,
            fill_pointer,
            |_| Word::NIL,
        )?;
        self.mark_nil_element(word).expect("fresh array header");
        Ok(word)
    }

    pub(crate) fn mark_nil_element(&self, word: Word) -> Option<()> {
        self.object_ref::<Array>(word)?
            .nil_element
            .store(true, Ordering::Relaxed);
        Some(())
    }

    /// Returns the stored element without allocating or reaching a safepoint.
    pub(crate) fn array_value(&self, word: Word, index: usize) -> Option<ArrayValue> {
        let (data, index) = self.array_storage(word, index, 1)?;
        if let Some(character) = self.string_data_character(data, index) {
            return Some(ArrayValue::Word(Word::character(character)));
        }
        let (payload, element) = self.array_data_payload(data)?;
        // SAFETY: The array traversal checked the index. Each payload's type
        // matches its header, and these reads cannot collect.
        Some(unsafe { read_array_data(payload, element, index) })
    }

    /// Reads an owned simple rank-one array with fixed-width storage.
    #[inline]
    pub(crate) fn simple_array_vector_value(&self, word: Word, index: usize) -> Option<ArrayValue> {
        let (data, element) = self
            .simple_array_vector_data(word, index, |element| element != ArrayElement::Character)?;
        // SAFETY: The owned storage and index were established above. This read cannot collect.
        Some(unsafe { read_array_data(trailing_base::<ArrayStorage>(data), element, index) })
    }

    /// Stores a correctly typed Lisp value and returns its unboxed representation on success.
    #[inline]
    pub(crate) fn write_simple_array_vector(
        &self,
        word: Word,
        index: usize,
        value: Word,
    ) -> Option<ArrayValue> {
        let (data, element) = self
            .simple_array_vector_data(word, index, |element| element != ArrayElement::Character)?;
        let value = ArrayValue::from_word(self, element, value)?;
        // SAFETY: The owned storage and index were established above. This write cannot collect.
        unsafe { self.write_array_data(data, element, index, value) }.then_some(value)
    }

    /// Reads an owned simple rank-one float array without allocating.
    pub(crate) fn simple_float_vector_value(&self, word: Word, index: usize) -> Option<ArrayValue> {
        let (data, element) = self.simple_array_vector_data(word, index, float_array_element)?;
        // SAFETY: The payload, format, and index were checked above. There is no safepoint.
        Some(unsafe { read_array_data(trailing_base::<ArrayStorage>(data), element, index) })
    }

    /// Writes an owned simple rank-one float array without allocating.
    pub(crate) fn write_simple_float_vector(
        &self,
        word: Word,
        index: usize,
        value: ArrayValue,
    ) -> bool {
        let Some((data, element)) = self.simple_array_vector_data(word, index, float_array_element)
        else {
            return false;
        };
        // SAFETY: The payload, format, and index were checked above. There is no safepoint.
        unsafe {
            write_float_array_data(trailing_base::<ArrayStorage>(data), element, index, value)
        }
    }

    /// May collect while boxing a float. Root any words the caller needs afterwards.
    pub(crate) fn array_slot(&self, word: Word, index: usize) -> Result<Option<Word>, OutOfMemory> {
        self.array_value(word, index)
            .map(|value| value.boxed(self))
            .transpose()
    }

    pub(crate) fn set_array_slot(
        &self,
        word: Word,
        index: usize,
        value: Word,
    ) -> Result<bool, OutOfMemory> {
        self.write_array_value(word, index, |element| {
            ArrayValue::from_word(self, element, value)
        })
    }

    pub(crate) fn set_array_value(
        &self,
        word: Word,
        index: usize,
        value: ArrayValue,
    ) -> Result<bool, OutOfMemory> {
        self.write_array_value(word, index, |_| Some(value))
    }

    fn write_array_value(
        &self,
        word: Word,
        index: usize,
        value: impl FnOnce(ArrayElement) -> Option<ArrayValue>,
    ) -> Result<bool, OutOfMemory> {
        let Some((data, index)) = self.array_storage(word, index, 1) else {
            return Ok(false);
        };
        if self.string_data_count(data).is_some() {
            let Some(ArrayValue::Word(value)) = value(ArrayElement::Character) else {
                return Ok(false);
            };
            let Some(character) = value.as_character() else {
                return Ok(false);
            };
            return self.set_string_data_character(data, index, character);
        }
        let (address, storage) = self
            .object_with_address::<ArrayStorage>(data)
            .expect("array storage");
        let element = storage.element;
        let Some(value) = value(element) else {
            return Ok(false);
        };
        // SAFETY: The index and payload layout were established above. No
        // safepoint occurs while writing these slots.
        Ok(unsafe { self.write_array_data(address, element, index, value) })
    }

    /// The caller supplies live ArrayStorage of `element` type and an in-bounds index.
    #[inline]
    unsafe fn write_array_data(
        &self,
        data: Address,
        element: ArrayElement,
        index: usize,
        value: ArrayValue,
    ) -> bool {
        let payload = trailing_base::<ArrayStorage>(data);
        unsafe {
            match (element, value) {
                (ArrayElement::General, ArrayValue::Word(value)) => {
                    Self::store_word_slot_unbarriered(payload + index * ValueSlot::STRIDE, value);
                    self.array_write_barrier(data, index, value);
                }
                (ArrayElement::Bit, ArrayValue::Word(value)) => {
                    let slot = &*payload.to_ptr::<AtomicUsize>().add(index / BITS_PER_WORD);
                    let mask = 1 << (index % BITS_PER_WORD);
                    match value.as_fixnum() {
                        Some(0) => slot.fetch_and(!mask, Ordering::Relaxed),
                        Some(1) => slot.fetch_or(mask, Ordering::Relaxed),
                        _ => return false,
                    };
                }
                (ArrayElement::Octet, ArrayValue::Octet(value)) => {
                    payload.to_mut_ptr::<u8>().add(index).write(value);
                }
                (element, value) => {
                    return write_float_array_data(payload, element, index, value);
                }
            }
        }
        true
    }

    #[inline]
    fn array_write_barrier(&self, owner: Address, index: usize, value: Word) {
        let Some(target) = value.object_address() else {
            return;
        };
        // SAFETY: every caller just stored into this general array's backing.
        let storage = unsafe { &*owner.to_ptr::<ArrayStorage>() };
        let flags = storage.header.gc_flags();
        if flags & GC_FLAG_MATURE == 0 {
            return;
        }
        // A pointer written by the mutator stays valid until the next safepoint.
        let target = unsafe { &*target.to_ptr::<Header>() };
        if storage.count < CARD_ARRAY_MIN_SLOTS {
            if flags & GC_FLAG_UNLOGGED != 0 && !target.is_mature() {
                self.record_remembered_object(owner, &storage.header);
            }
        } else {
            let card = index / CARD_SLOTS;
            let word = storage.card_word(owner, card);
            let bit = 1 << (card % BITS_PER_WORD);
            if word.load(Ordering::Relaxed) & bit == 0
                && !target.is_mature()
                && word.fetch_or(bit, Ordering::Relaxed) & bit == 0
            {
                self.remembered
                    .borrow_mut()
                    .cards
                    .push(RememberedCard { owner, index: card });
            }
        }
    }

    /// Writes a character range from UTF-8 storage that survives a Lisp safepoint.
    pub(crate) fn set_array_string_range(
        &self,
        word: Word,
        start: usize,
        replacement: &str,
    ) -> Result<bool, OutOfMemory> {
        let length = replacement.chars().count();
        let Some((data, start)) = self.array_storage(word, start, length) else {
            return Ok(false);
        };
        self.replace_string_data_range(data, start, length, replacement)
    }

    pub(super) fn array_storage(
        &self,
        mut word: Word,
        mut start: usize,
        length: usize,
    ) -> Option<(Word, usize)> {
        loop {
            let end = start.checked_add(length)?;
            if let Some(count) = self.string_data_count(word) {
                return (end <= count).then_some((word, start));
            }
            let array = self.object_ref::<Array>(word)?;
            if end > array.total.load(Ordering::Relaxed) {
                return None;
            }
            let backing = array.backing.get();
            let displacement = array.displacement.load(Ordering::Relaxed);
            if displacement == usize::MAX {
                return Some((backing, start));
            }
            word = backing;
            start = displacement.checked_add(start)?;
        }
    }

    pub(crate) fn set_array_fill_pointer(&self, word: Word, fill: usize) -> Option<()> {
        let array = self.object_ref::<Array>(word)?;
        if fill > array.total.load(Ordering::Relaxed)
            || array.fill.load(Ordering::Relaxed) == usize::MAX
        {
            return None;
        }
        array.fill.store(fill, Ordering::Relaxed);
        Some(())
    }

    fn array_data_payload(&self, data: Word) -> Option<(Address, ArrayElement)> {
        let (address, header) = self.object_with_address::<ArrayStorage>(data)?;
        Some((trailing_base::<ArrayStorage>(address), header.element))
    }

    #[inline(always)]
    fn simple_array_vector_data(
        &self,
        word: Word,
        index: usize,
        accepts: impl FnOnce(ArrayElement) -> bool,
    ) -> Option<(Address, ArrayElement)> {
        let array = self.object_ref::<Array>(word)?;
        let element = array.element;
        if !accepts(element)
            || array.rank != 1
            || array.adjustable
            || (element == ArrayElement::General && array.nil_element.load(Ordering::Relaxed))
            || array.displacement.load(Ordering::Relaxed) != usize::MAX
            || array.fill.load(Ordering::Relaxed) != usize::MAX
            || index >= array.total.load(Ordering::Relaxed)
        {
            return None;
        }
        // The accepted elements use ArrayStorage. An owned array's storage has
        // the same element type and size as its header.
        let data = array
            .backing
            .get()
            .object_address()
            .expect("owned array data");
        Some((data, element))
    }

    fn simple_general_vector_slot(
        &self,
        word: Word,
        index: usize,
    ) -> Result<(Address, &AtomicUsize), SimpleVectorError> {
        let array = self
            .object_ref::<Array>(word)
            .ok_or(SimpleVectorError::NotSimpleVector)?;
        if array.rank != 1
            || array.adjustable
            || array.element != ArrayElement::General
            || array.nil_element.load(Ordering::Relaxed)
            || array.displacement.load(Ordering::Relaxed) != usize::MAX
            || array.fill.load(Ordering::Relaxed) != usize::MAX
        {
            return Err(SimpleVectorError::NotSimpleVector);
        }
        let len = array.total.load(Ordering::Relaxed);
        if index >= len {
            return Err(SimpleVectorError::OutOfBounds { len });
        }
        let (data_addr, storage) = self
            .object_with_address::<ArrayStorage>(array.backing.get())
            .expect("owned simple-vector backing storage");
        debug_assert_eq!(storage.element, ArrayElement::General);
        debug_assert!(index < storage.count);
        let payload = trailing_base::<ArrayStorage>(data_addr);
        // SAFETY: `index < len <= storage.count`, and `ArrayStorage` for `General` holds `AtomicUsize` slots.
        let slot = unsafe { &*payload.to_ptr::<AtomicUsize>().add(index) };
        Ok((data_addr, slot))
    }

    pub(crate) fn atomic_load_svref(
        &self,
        word: Word,
        index: usize,
        order: Ordering,
    ) -> Result<Word, SimpleVectorError> {
        let (_, slot) = self.simple_general_vector_slot(word, index)?;
        Ok(Word(slot.load(order)))
    }

    pub(crate) fn atomic_store_svref(
        &self,
        word: Word,
        index: usize,
        value: Word,
        order: Ordering,
    ) -> Result<(), SimpleVectorError> {
        let (storage_addr, slot) = self.simple_general_vector_slot(word, index)?;
        slot.store(value.0, order);
        self.array_write_barrier(storage_addr, index, value);
        Ok(())
    }

    pub(crate) fn atomic_cas_svref(
        &self,
        word: Word,
        index: usize,
        expected: Word,
        new: Word,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Result<Word, Word>, SimpleVectorError> {
        let (storage_addr, slot) = self.simple_general_vector_slot(word, index)?;
        let result = slot
            .compare_exchange(expected.0, new.0, success, failure)
            .map(Word)
            .map_err(Word);
        if result.is_ok() {
            self.array_write_barrier(storage_addr, index, new);
        }
        Ok(result)
    }
}

fn float_array_element(element: ArrayElement) -> bool {
    matches!(
        element,
        ArrayElement::SingleFloat | ArrayElement::DoubleFloat
    )
}

fn numeric_width(element: ArrayElement) -> usize {
    match element {
        ArrayElement::Octet => size_of::<u8>(),
        ArrayElement::SingleFloat => size_of::<u32>(),
        ArrayElement::DoubleFloat => size_of::<u64>(),
        _ => unreachable!("owned fixed-width numeric array"),
    }
}

fn active_lanes(mut mask: u64) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let lane = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        Some(lane)
    })
}

// Copies between native-endian owned backing and little-endian pack storage.
// Both pointers must be valid for `width` non-overlapping bytes.
unsafe fn copy_numeric_lane(source: *const u8, target: *mut u8, width: usize) {
    if cfg!(target_endian = "little") {
        unsafe { ptr::copy_nonoverlapping(source, target, width) };
    } else {
        for i in 0..width {
            unsafe { target.add(i).write(source.add(width - 1 - i).read()) };
        }
    }
}

// The caller supplies live ArrayStorage storage of `element` type and an in-bounds index.
#[inline]
unsafe fn write_float_array_data(
    payload: Address,
    element: ArrayElement,
    index: usize,
    value: ArrayValue,
) -> bool {
    unsafe {
        match (element, value) {
            (ArrayElement::SingleFloat, ArrayValue::SingleFloat(value)) => {
                payload
                    .to_mut_ptr::<u32>()
                    .add(index)
                    .write(value.to_bits());
            }
            (ArrayElement::DoubleFloat, ArrayValue::DoubleFloat(value)) => {
                payload
                    .to_mut_ptr::<u64>()
                    .add(index)
                    .write(value.to_bits());
            }
            _ => return false,
        }
    }
    true
}

// The caller supplies live ArrayStorage storage of `element` type and an in-bounds index.
#[inline]
unsafe fn read_array_data(payload: Address, element: ArrayElement, index: usize) -> ArrayValue {
    unsafe {
        match element {
            ArrayElement::General => {
                let slot = &*payload.to_ptr::<AtomicUsize>().add(index);
                ArrayValue::Word(Word(slot.load(Ordering::Relaxed)))
            }
            ArrayElement::Bit => {
                let slot = &*payload.to_ptr::<AtomicUsize>().add(index / BITS_PER_WORD);
                let bit = (slot.load(Ordering::Relaxed) >> (index % BITS_PER_WORD)) & 1;
                ArrayValue::Word(Word::fixnum(bit as isize).expect("bit fits in fixnum"))
            }
            ArrayElement::Octet => ArrayValue::Octet(payload.to_ptr::<u8>().add(index).read()),
            ArrayElement::SingleFloat => {
                ArrayValue::SingleFloat(f32::from_bits(payload.to_ptr::<u32>().add(index).read()))
            }
            ArrayElement::DoubleFloat => {
                ArrayValue::DoubleFloat(f64::from_bits(payload.to_ptr::<u64>().add(index).read()))
            }
            ArrayElement::Character => unreachable!("character storage is UTF-8"),
        }
    }
}

pub(super) fn scan_array<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visitor.visit_slot(ValueSlot(base + offset_of!(Array, backing)));
}

pub(super) fn scan_data<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    // SAFETY: The collector supplies live array data with all slots initialized.
    let data = unsafe { &*base.to_ptr::<ArrayStorage>() };
    if data.element != ArrayElement::General {
        return;
    }
    visitor.visit_array(trailing_base::<ArrayStorage>(base), data.count);
}

#[cfg(test)]
mod tests {
    use super::super::object_address;
    use super::*;

    #[test]
    fn simple_vector_write_remembers_a_young_reference() {
        let heap = Heap::start(Heap::builder()).unwrap();
        let array = heap.root_slot(
            heap.array(&[1], ArrayElement::General, false, None, |_| Word::NIL)
                .unwrap(),
        );
        heap.collect_minor();

        let value = heap.root_slot(heap.cons(|| (Word::T, Word::NIL)).unwrap());
        let weak = heap.root_slot(heap.weak_pointer(&value).unwrap());
        assert!(
            heap.write_simple_array_vector(array.get(), 0, value.get())
                .is_some()
        );
        drop(value);
        heap.collect_minor();

        let value = heap
            .simple_array_vector_value(array.get(), 0)
            .unwrap()
            .expect_word();
        assert_eq!(heap.weak_pointer_value(weak.get()), Some(value));
        assert_eq!(heap.cons_fields(value), Some((Word::T, Word::NIL)));
    }

    #[test]
    fn evacuation_preserves_the_bits_in_packed_float_arrays() {
        let mut config = Heap::builder_with_heap_size(16 << 20);
        config.always_defrag = true;
        config.stress_bytes = Some(1);
        let heap = Heap::start(config).unwrap();

        let single_bits = [(-0.0f32).to_bits(), 1, 0x0080_0000];
        let singles = heap.root_slot(
            heap.array_from_values(&[3], ArrayElement::SingleFloat, false, None, |i| {
                ArrayValue::SingleFloat(f32::from_bits(single_bits[i]))
            })
            .unwrap(),
        );
        let doubles = heap.root_slot(
            heap.array_from_values(&[3], ArrayElement::DoubleFloat, false, None, |_| {
                ArrayValue::DoubleFloat(-0.0)
            })
            .unwrap(),
        );

        let cons = heap.root_slot(heap.cons(|| (Word::NIL, Word::NIL)).unwrap());
        let previous_cons = cons.get();
        let tagged_bits = previous_cons.0 as u64;
        assert!(
            heap.set_array_value(doubles.get(), 1, ArrayValue::DoubleFloat(f64::from_bits(1)))
                .unwrap()
        );
        assert!(
            heap.set_array_value(
                doubles.get(),
                2,
                ArrayValue::DoubleFloat(f64::from_bits(tagged_bits))
            )
            .unwrap()
        );

        for (array, width) in [(&singles, 4), (&doubles, 8)] {
            let (data, _) = heap.array_storage(array.get(), 0, 3).unwrap();
            let address = object_address(data, ObjectKind::ArrayStorage).unwrap();
            // SAFETY: The kind check found an initialized live ArrayStorage head.
            assert_eq!(
                usize::from(unsafe { (*address.to_ptr::<ArrayStorage>()).header.immix_size }),
                (size_of::<ArrayStorage>() + 3 * width)
                    .next_multiple_of(crate::heap::immix::OBJECT_ALIGN)
            );
        }
        let previous_single_storage = heap.array_storage(singles.get(), 0, 3).unwrap().0;
        let previous_double_storage = heap.array_storage(doubles.get(), 0, 3).unwrap().0;
        heap.collect();
        assert_ne!(cons.get(), previous_cons);
        assert_ne!(
            heap.array_storage(singles.get(), 0, 3).unwrap().0,
            previous_single_storage
        );
        assert_ne!(
            heap.array_storage(doubles.get(), 0, 3).unwrap().0,
            previous_double_storage
        );

        let mut packed = [0u8; 12];
        heap.load_numeric_pack(
            singles.get(),
            ArrayElement::SingleFloat,
            0,
            0b111,
            &mut packed,
        )
        .unwrap();
        assert_eq!(
            packed.as_slice(),
            single_bits.map(u32::to_le_bytes).as_flattened()
        );

        for (index, bits) in single_bits.into_iter().enumerate() {
            let value = heap.array_slot(singles.get(), index).unwrap().unwrap();
            assert_eq!(heap.single_float(value).unwrap().to_bits(), bits);
        }
        for (index, bits) in [(-0.0f64).to_bits(), 1, tagged_bits]
            .into_iter()
            .enumerate()
        {
            let value = heap.array_slot(doubles.get(), index).unwrap().unwrap();
            assert_eq!(heap.double_float(value).unwrap().to_bits(), bits);
        }
        assert!(heap.array_slot(doubles.get(), 3).unwrap().is_none());
    }

    #[test]
    fn numeric_pack_checks_the_current_displaced_bounds_before_access() {
        let heap = Heap::start(Heap::builder_with_heap_size(16 << 20)).unwrap();
        let element = ArrayElement::Octet;
        let owner = heap.root_slot(
            heap.array_from_values(&[8], element, true, None, |i| ArrayValue::Octet(i as u8))
                .unwrap(),
        );
        let displaced = heap.root_slot(
            heap.displaced_array(&[4], element, false, None, 4, || owner.get())
                .unwrap(),
        );

        let mut output = [0xee; 4];
        heap.load_numeric_pack(displaced.get(), element, 1, 0b0101, &mut output)
            .unwrap();
        assert_eq!(output, [5, 0xee, 7, 0xee]);

        let shorter = heap
            .array_from_values(&[6], element, true, None, |i| ArrayValue::Octet(i as u8))
            .unwrap();
        heap.replace_adjustable_array(owner.get(), shorter).unwrap();

        let saved = output;
        assert_eq!(
            heap.load_numeric_pack(displaced.get(), element, 1, 0b0101, &mut output),
            Err(2)
        );
        assert_eq!(output, saved);
        assert_eq!(
            heap.store_numeric_pack(displaced.get(), element, 1, 0b0101, &[9; 4]),
            Err(2)
        );
        assert_eq!(
            heap.octet_array_range(owner.get(), 0, 6).unwrap(),
            [0, 1, 2, 3, 4, 5]
        );

        heap.store_numeric_pack(displaced.get(), element, 1, 1, &[9; 4])
            .unwrap();
        assert_eq!(
            heap.octet_array_range(owner.get(), 0, 6).unwrap(),
            [0, 1, 2, 3, 4, 9]
        );
        heap.load_numeric_pack(displaced.get(), element, usize::MAX, 0, &mut output)
            .unwrap();
        assert_eq!(output, saved);
    }

    #[test]
    fn scatter_validates_before_writing_and_commits_repeated_indices_in_lane_order() {
        let heap = Heap::start(Heap::builder_with_heap_size(16 << 20)).unwrap();
        let element = ArrayElement::Octet;
        let array = heap.octet_vector(&[1, 2, 3]).unwrap();
        let indices = [Some(2), None, Some(2), Some(0)];
        let bytes = [11, 12, 13, 14];

        assert_eq!(
            heap.scatter_numeric_pack(array, element, &indices, 0b1111, &bytes),
            Err(1)
        );
        assert_eq!(heap.octet_array_range(array, 0, 3).unwrap(), [1, 2, 3]);
        heap.scatter_numeric_pack(array, element, &indices, 0b1101, &bytes)
            .unwrap();
        assert_eq!(heap.octet_array_range(array, 0, 3).unwrap(), [14, 2, 13]);

        let mut output = [99; 4];
        heap.gather_numeric_pack(array, element, &indices, 0b1101, &mut output)
            .unwrap();
        assert_eq!(output, [13, 99, 13, 14]);
    }

    #[test]
    fn bulk_float_spans_compare_resolved_backing_ranges() {
        let heap = Heap::start(Heap::builder_with_heap_size(16 << 20)).unwrap();
        let element = ArrayElement::SingleFloat;
        let owner = heap.root_slot(
            heap.array_from_values(&[10], element, false, None, |i| {
                ArrayValue::SingleFloat(i as f32)
            })
            .unwrap(),
        );
        let output = heap.root_slot(
            heap.displaced_array(&[5], element, false, None, 1, || owner.get())
                .unwrap(),
        );

        let exact = [(output.get(), 0), (owner.get(), 1), (owner.get(), 5)];
        assert_eq!(heap.check_f32_array_spans(exact, 4), Ok(()));
        let partial = [(output.get(), 0), (owner.get(), 2), (owner.get(), 5)];
        assert_eq!(
            heap.check_f32_array_spans(partial, 4),
            Err(F32SpanError::PartialOverlap { input: 1 })
        );
        let short = [(output.get(), 0), (owner.get(), 1), (owner.get(), 8)];
        assert_eq!(
            heap.check_f32_array_spans(short, 4),
            Err(F32SpanError::Bounds {
                array: 2,
                index: 10
            })
        );
    }

    #[test]
    fn minor_collection_scans_each_dirty_array_card_once() {
        let heap = Heap::start(Heap::builder()).unwrap();
        let len = CARD_ARRAY_MIN_SLOTS + 1;
        let array = heap.root_slot(
            heap.array(&[len], ArrayElement::General, false, None, |_| Word::NIL)
                .unwrap(),
        );
        let displaced = heap.root_slot(
            heap.displaced_array(&[2], ArrayElement::General, false, None, len - 2, || {
                array.get()
            })
            .unwrap(),
        );
        let old = heap.root_slot(heap.string("mature").unwrap());
        heap.collect_minor();

        let first = heap.string("first card").unwrap();
        let second = heap.string("same card").unwrap();
        let last = heap.root_slot(heap.string("partial last card").unwrap());
        let last_weak = heap.root_slot(heap.weak_pointer(&last).unwrap());
        heap.atomic_store_svref(array.get(), 0, old.get(), Ordering::Relaxed)
            .unwrap();
        assert!(heap.remembered.borrow().cards.is_empty());
        heap.atomic_store_svref(array.get(), 1, first, Ordering::Relaxed)
            .unwrap();
        assert_eq!(
            heap.atomic_cas_svref(
                array.get(),
                2,
                Word::NIL,
                second,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ),
            Ok(Ok(Word::NIL))
        );
        assert!(heap.set_array_slot(displaced.get(), 1, last.get()).unwrap());
        drop(last);
        assert_eq!(heap.remembered.borrow().cards.len(), 2);
        assert!(heap.remembered.borrow().objects.is_empty());
        heap.collect_minor();
        assert_eq!(
            heap.weak_pointer_value(last_weak.get()),
            heap.array_slot(array.get(), len - 1).unwrap(),
        );

        for (index, text) in [
            (1, "first card"),
            (2, "same card"),
            (len - 1, "partial last card"),
        ] {
            let value = heap.array_slot(array.get(), index).unwrap().unwrap();
            assert_eq!(heap.string_contents(value).as_deref(), Some(text));
        }
        let young = heap.string("write before a major collection").unwrap();
        assert!(heap.set_array_slot(array.get(), 1, young).unwrap());
        assert_eq!(heap.remembered.borrow().cards.len(), 1);
        heap.collect();

        let young = heap.string("write after the major collection").unwrap();
        assert!(heap.set_array_slot(array.get(), 1, young).unwrap());
        assert_eq!(heap.remembered.borrow().cards.len(), 1);
        heap.collect_minor();
        let value = heap.array_slot(array.get(), 1).unwrap().unwrap();
        assert_eq!(
            heap.string_contents(value).as_deref(),
            Some("write after the major collection")
        );
    }
}
