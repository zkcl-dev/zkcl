use std::mem::offset_of;
use std::ops::{Add, Sub};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use rustc_hash::FxHashSet;
use zkcl_bytecode::Immediate;

use super::condition_restart::{LispCondition, LispPathname, LispRestart};
use super::cons::Cons;
use super::function::{Closure, Function, HostFunction};
use super::object::{WordSlot, trailing_base, visit_offsets};
use super::string::LispString;
use super::symbol::{Cell, Symbol};
use super::{
    array, class, foreign, generic, hash_table, immix, instance, mop, number, readtable, stream,
    structure, treelist, weak,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Address(usize);

impl Address {
    #[inline(always)]
    pub(super) const fn from_usize(addr: usize) -> Self {
        Self(addr)
    }

    #[inline(always)]
    pub(super) fn from_ptr<T>(ptr: *const T) -> Self {
        Self(ptr as usize)
    }

    #[inline(always)]
    pub(super) const fn as_usize(self) -> usize {
        self.0
    }

    #[inline(always)]
    pub(super) const fn to_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    #[inline(always)]
    pub(super) const fn to_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }
}

impl Add<usize> for Address {
    type Output = Self;
    #[inline(always)]
    fn add(self, rhs: usize) -> Self {
        Self(self.0.wrapping_add(rhs))
    }
}

impl Sub<usize> for Address {
    type Output = Self;
    #[inline(always)]
    fn sub(self, rhs: usize) -> Self {
        Self(self.0.wrapping_sub(rhs))
    }
}

pub(super) trait SlotVisitor {
    fn visit_slot(&mut self, slot: ValueSlot);

    #[inline(always)]
    fn visit_array(&mut self, start: Address, count: usize) {
        for slot in ValueSlot::array(start, count) {
            self.visit_slot(slot);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Word(pub(crate) usize);

impl Word {
    pub const UNBOUND: Self = Self(0);
    pub const NIL: Self = Self(2);
    pub const T: Self = Self(3);
    // Sentinel for Domain-local TLS slots that defer to a symbol's global value cell.
    pub(crate) const NO_TLS_VALUE: Self = Self((1 << 3) | 3);
    pub const FIXNUM_MIN: isize = isize::MIN >> zkcl_target::FIXNUM_SHIFT;
    pub const FIXNUM_MAX: isize = isize::MAX >> zkcl_target::FIXNUM_SHIFT;

    #[inline(always)]
    pub const fn raw(self) -> usize {
        self.0
    }

    #[inline(always)]
    pub(crate) const fn from_raw(raw: usize) -> Self {
        Self(raw)
    }

    pub fn character(value: char) -> Self {
        // NIL occupies tag 2 with a zero payload.
        Self(((value as usize + 1) << 3) | 2)
    }

    pub fn as_character(self) -> Option<char> {
        if self.0 & 7 != 2 || self == Self::NIL {
            return None;
        }
        char::from_u32(((self.0 >> 3) - 1) as u32)
    }

    pub(crate) fn primitive(index: usize) -> Self {
        Self(index << 3 | 4)
    }

    pub fn primitive_index(self) -> Option<usize> {
        (self.0 & 7 == 4).then_some(self.0 >> 3)
    }

    pub(crate) fn package(index: usize) -> Self {
        Self(index << 3 | 5)
    }

    pub fn package_index(self) -> Option<usize> {
        (self.0 & 7 == 5).then_some(self.0 >> 3)
    }

    pub(crate) fn native(index: usize) -> Self {
        Self(index << 3 | 6)
    }

    pub fn native_index(self) -> Option<usize> {
        (self.0 & 7 == 6).then_some(self.0 >> 3)
    }

    pub(crate) fn exit_token(index: usize) -> Option<Self> {
        (index <= usize::MAX >> 3).then_some(Self(index << 3 | 7))
    }

    pub fn is_exit_token(self) -> bool {
        self.0 & 7 == 7
    }

    pub fn fixnum(value: isize) -> Option<Self> {
        (Self::FIXNUM_MIN..=Self::FIXNUM_MAX)
            .contains(&value)
            .then_some(Self((value as usize) << zkcl_target::FIXNUM_SHIFT | 1))
    }

    pub fn as_fixnum(self) -> Option<isize> {
        (self.0 & 7 == 1).then_some((self.0 as isize) >> zkcl_target::FIXNUM_SHIFT)
    }

    /// Signed tagged words preserve the ordering of their fixnum payloads.
    #[inline]
    pub(crate) fn ordered_fixnum_pair(self, rhs: Self) -> Option<(isize, isize)> {
        (((self.0 ^ 1) | (rhs.0 ^ 1)) & 7 == 0).then_some((self.0 as isize, rhs.0 as isize))
    }

    // A fixnum n is stored as 8*n + 1. Keeping one operand scaled makes
    // native signed overflow coincide with overflow of the fixnum payload.
    #[inline]
    fn scaled_fixnum(self) -> Option<isize> {
        (self.0 & 7 == 1).then_some((self.0 & !7) as isize)
    }

    #[inline]
    pub(crate) fn checked_fixnum_add(self, rhs: Self) -> Option<Self> {
        self.as_fixnum()?;
        Some(Self(
            (self.0 as isize).checked_add(rhs.scaled_fixnum()?)? as usize
        ))
    }

    #[inline]
    pub(crate) fn checked_fixnum_sub(self, rhs: Self) -> Option<Self> {
        self.as_fixnum()?;
        Some(Self(
            (self.0 as isize).checked_sub(rhs.scaled_fixnum()?)? as usize
        ))
    }

    #[inline]
    pub(crate) fn checked_fixnum_mul(self, rhs: Self) -> Option<Self> {
        let product = self.scaled_fixnum()?.checked_mul(rhs.as_fixnum()?)?;
        Some(Self(product as usize | 1))
    }

    pub(crate) fn from_immediate(immediate: Immediate) -> Option<Self> {
        immediate
            .encode_word(usize::BITS as u8)
            .map(|raw| Self(raw as usize))
    }

    #[inline(always)]
    pub fn is_immediate(self) -> bool {
        self.0 & 7 != 0 || self.0 == 0
    }

    #[inline(always)]
    pub(crate) fn is_heap_object(self) -> bool {
        self.0 & 7 == 0 && self.0 != 0
    }

    pub(super) fn object_address(self) -> Option<Address> {
        self.is_heap_object().then_some(Address::from_usize(self.0))
    }

    pub(super) fn from_object_address(address: Address) -> Self {
        Self(address.as_usize())
    }
}

pub(super) const GC_FLAG_UNLOGGED: u8 = 1 << 0;
pub(super) const GC_FLAG_MATURE: u8 = 1 << 1;
pub(super) const GC_FLAG_PINNED: u8 = 1 << 2;

#[repr(C)]
pub(super) struct Header {
    // Immix can overwrite this word with a forwarding address after copying.
    pub(super) forwarding: AtomicUsize,
    pub(super) identity: AtomicUsize,
    // Large objects have size zero here. Their allocation records own the layout.
    pub(super) immix_size: u16,
    pub(super) kind: ObjectKind,
    // Packed into tail padding of (immix_size, kind); keeps Header at 3 machine words.
    pub(super) gc_flags: AtomicU8,
}

const _: () = assert!(size_of::<Header>() == 3 * size_of::<usize>());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum ObjectKind {
    Array,
    ArrayStorage,
    Cons,
    Symbol,
    Function,
    Cell,
    Closure,
    String,
    Condition,
    Restart,
    MacroEnvironment,
    Stream,
    Pathname,
    HashTable,
    HashBuckets,
    WeakPointer,
    Structure,
    Class,
    Instance,
    FuncallableInstance,
    InstanceLayout,
    InstanceStorage,
    GenericFunction,
    Method,
    Specializer,
    SlotDefinition,
    Bignum,
    Ratio,
    SingleFloat,
    DoubleFloat,
    Complex,
    Readtable,
    RandomState,
    Foreign,
    SimdPack,
    SimdMask,
    Treelist,
    MutableTreelist,
    TreelistNode,
    TreelistSpan,
    TreelistChunk,
    HostFunction,
}

impl Header {
    pub(super) fn new(kind: ObjectKind, size: usize) -> Self {
        let immix_size = if size <= immix::MAX_IMMIX_OBJECT_BYTES {
            size.next_multiple_of(immix::OBJECT_ALIGN) as u16
        } else {
            0
        };
        Self {
            forwarding: AtomicUsize::new(0),
            identity: AtomicUsize::new(0),
            immix_size,
            kind,
            gc_flags: AtomicU8::new(GC_FLAG_UNLOGGED),
        }
    }

    #[inline(always)]
    pub(super) fn gc_flags(&self) -> u8 {
        self.gc_flags.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub(super) fn is_mature(&self) -> bool {
        self.gc_flags() & GC_FLAG_MATURE != 0
    }

    #[inline(always)]
    pub(super) fn promote_mature(&self) {
        self.gc_flags
            .fetch_or(GC_FLAG_MATURE | GC_FLAG_UNLOGGED, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(super) fn mark_unlogged(&self) {
        self.gc_flags.fetch_or(GC_FLAG_UNLOGGED, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(super) fn try_log(&self) -> bool {
        (self.gc_flags.fetch_and(!GC_FLAG_UNLOGGED, Ordering::Relaxed) & GC_FLAG_UNLOGGED) != 0
    }

    #[inline(always)]
    pub(super) fn is_pinned(&self) -> bool {
        self.gc_flags() & GC_FLAG_PINNED != 0
    }

    #[cfg(test)]
    #[inline(always)]
    pub(super) fn set_pinned(&self, pinned: bool) {
        if pinned {
            self.gc_flags.fetch_or(GC_FLAG_PINNED, Ordering::Relaxed);
        } else {
            self.gc_flags.fetch_and(!GC_FLAG_PINNED, Ordering::Relaxed);
        }
    }
}

pub(super) fn object_address(word: Word, kind: ObjectKind) -> Option<Address> {
    let address = word.object_address()?;
    // SAFETY: only live, rooted words (or words fetched since the last safepoint)
    // reach this module's accessors. All allocated objects begin with a header.
    (unsafe { (*address.to_ptr::<Header>()).kind } == kind).then_some(address)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ValueSlot(pub(super) Address);

impl ValueSlot {
    pub(super) const STRIDE: usize = size_of::<WordSlot>();

    #[inline]
    pub(super) fn array(start: Address, count: usize) -> impl Iterator<Item = Self> {
        (0..count).map(move |index| Self(start + index * Self::STRIDE))
    }
}

pub(super) fn scan_object_slots<SV: SlotVisitor>(
    base: Address,
    visitor: &mut SV,
    weak: &mut weak::WeakObjects,
    foreign_ids: &mut FxHashSet<u64>,
) -> Option<crate::code_store::CodeId> {
    // SAFETY: Immix supplies a live, initialized object header during collection.
    match unsafe { (*base.to_ptr::<Header>()).kind } {
        ObjectKind::String => {
            visitor.visit_slot(ValueSlot(base + offset_of!(LispString, replacement)));
        }
        ObjectKind::MacroEnvironment
        | ObjectKind::Bignum
        | ObjectKind::SingleFloat
        | ObjectKind::DoubleFloat
        | ObjectKind::SimdPack
        | ObjectKind::SimdMask
        | ObjectKind::RandomState => {}
        ObjectKind::Foreign => {
            // SAFETY: the object header establishes the layout.
            foreign_ids.insert(unsafe { (*base.to_ptr::<foreign::LispForeign>()).id });
            foreign::scan_edge(base, visitor);
        }
        ObjectKind::Ratio | ObjectKind::Complex => number::scan_pair(base, visitor),
        ObjectKind::Condition => {
            // SAFETY: The collector supplies a live, initialized condition.
            let count = unsafe { (*base.to_ptr::<LispCondition>()).count };
            visitor.visit_array(trailing_base::<LispCondition>(base), count);
        }
        ObjectKind::Restart => {
            visit_offsets(
                base,
                visitor,
                &[
                    offset_of!(LispRestart, name),
                    offset_of!(LispRestart, report),
                ],
            );
        }
        ObjectKind::Cons => {
            visit_offsets(
                base,
                visitor,
                &[offset_of!(Cons, car), offset_of!(Cons, cdr)],
            );
        }
        ObjectKind::Symbol => {
            visit_offsets(
                base,
                visitor,
                &[
                    offset_of!(Symbol, value),
                    offset_of!(Symbol, function),
                    offset_of!(Symbol, setter),
                    offset_of!(Symbol, name),
                    offset_of!(Symbol, variable_doc),
                    offset_of!(Symbol, symbol_macro),
                ],
            );
        }
        ObjectKind::Stream => stream::trace_slots(base, visitor),
        ObjectKind::Readtable => readtable::trace_slots(base, visitor),
        ObjectKind::Pathname => {
            visitor.visit_array(base + offset_of!(LispPathname, components), 6);
        }
        ObjectKind::WeakPointer => weak.pointers.push(base),
        ObjectKind::HashTable => {
            hash_table::scan_table(base, visitor);
            if hash_table::has_weak_entries(base) {
                weak.tables.push(base);
            }
        }
        ObjectKind::HashBuckets => hash_table::scan_buckets(base, visitor),
        ObjectKind::Structure => structure::scan(base, visitor),
        ObjectKind::Class => class::scan(base, visitor),
        ObjectKind::Instance => instance::scan_instance(base, visitor),
        ObjectKind::FuncallableInstance => instance::scan_funcallable_instance(base, visitor),
        ObjectKind::InstanceLayout => instance::scan_layout(base, visitor),
        ObjectKind::InstanceStorage => instance::scan_storage(base, visitor),
        ObjectKind::GenericFunction => generic::scan_generic(base, visitor),
        ObjectKind::Method => generic::scan_method(base, visitor),
        ObjectKind::Specializer => mop::scan_specializer(base, visitor),
        ObjectKind::SlotDefinition => mop::scan_slot_definition(base, visitor),
        ObjectKind::Array => array::scan_array(base, visitor),
        ObjectKind::ArrayStorage => array::scan_data(base, visitor),
        ObjectKind::Treelist => treelist::scan_treelist(base, visitor),
        ObjectKind::MutableTreelist => treelist::scan_mutable_treelist(base, visitor),
        ObjectKind::TreelistNode => treelist::scan_node(base, visitor),
        ObjectKind::TreelistSpan => treelist::scan_span(base, visitor),
        ObjectKind::TreelistChunk => treelist::scan_chunk(base, visitor),
        ObjectKind::Function => {
            // SAFETY: this object has a function header and inline constant slots.
            let info = unsafe { (*base.to_ptr::<Function>()).info };
            // The trailing slot holds the documentation string.
            visitor.visit_array(base + info.constants_offset(), info.constants + 1);
            return Some(info.code_id);
        }
        ObjectKind::Cell => visitor.visit_slot(ValueSlot(base + offset_of!(Cell, value))),
        ObjectKind::HostFunction => {
            // SAFETY: the object header establishes the layout.
            foreign_ids.insert(unsafe { (*base.to_ptr::<HostFunction>()).id });
            visitor.visit_slot(ValueSlot(base + offset_of!(HostFunction, capture)));
        }
        ObjectKind::Closure => {
            visitor.visit_slot(ValueSlot(base + offset_of!(Closure, code)));
            // SAFETY: this object has a closure header and its declared number of slots.
            let count = unsafe { (*base.to_ptr::<Closure>()).count };
            // Captures and the trailing documentation string.
            visitor.visit_array(trailing_base::<Closure>(base), count + 1);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_arithmetic_matches_the_full_precision_result() {
        let inputs = [
            Word::FIXNUM_MIN,
            Word::FIXNUM_MIN + 1,
            -2,
            -1,
            0,
            1,
            2,
            Word::FIXNUM_MAX - 1,
            Word::FIXNUM_MAX,
        ];
        for a in inputs {
            let left = Word::fixnum(a).unwrap();
            for b in inputs {
                let right = Word::fixnum(b).unwrap();
                let (x, y) = (a as i128, b as i128);
                for (value, exact) in [
                    (left.checked_fixnum_add(right), x + y),
                    (left.checked_fixnum_sub(right), x - y),
                    (left.checked_fixnum_mul(right), x * y),
                ] {
                    assert_eq!(value, isize::try_from(exact).ok().and_then(Word::fixnum));
                }
            }
            assert!(Word::NIL.checked_fixnum_add(left).is_none());
            assert!(left.checked_fixnum_sub(Word::T).is_none());
            assert!(left.checked_fixnum_mul(Word::character('7')).is_none());
        }
    }
}
