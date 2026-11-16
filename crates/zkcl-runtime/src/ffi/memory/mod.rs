//! Byte-addressable native storage for an opted-in FFI runtime.

mod octets;
mod utf8;

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::any::Any;
use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::{CStr, CString, c_void};
use std::ptr::{self, NonNull};
use std::rc::Rc;

use smallvec::SmallVec;

use super::aggregate::ForeignType;
use super::{Argument, CType, CallResult, ForeignCallError, arity, integer, string};
use crate::condition::Fault;
use crate::heap::{ForeignEdge, Word};
use crate::{EvalError, OutOfMemory, Root, Runtime, vm};

pub(super) fn install(rt: &Runtime) -> Result<(), EvalError> {
    type Native = for<'rt> fn(&'rt Runtime, &[Root<'rt>], usize) -> CallResult<'rt>;
    let operations: &[(&str, Native)] = &[
        ("ZK-FFI:ALLOCATE", allocate),
        ("ZK-FFI:FREE", free),
        ("ZK-FFI:POINTER-P", pointer_p),
        ("ZK-FFI:POINTER-ADDRESS", pointer_address),
        ("ZK-FFI:MAKE-POINTER", make_pointer),
        ("ZK-FFI:INC-POINTER", inc_pointer),
        ("ZK-FFI:TYPE-SIZE", type_size),
        ("ZK-FFI:TYPE-ALIGNMENT", type_alignment),
        ("ZK-FFI:MEM-REF", mem_ref),
        ("ZK-FFI:MEM-SET", mem_set),
        ("ZK-FFI::%READ-OCTETS", octets::read),
        ("ZK-FFI::%WRITE-OCTETS", octets::write),
        ("ZK-FFI:READ-C-STRING", read_c_string),
        ("ZK-FFI:WRITE-C-STRING", write_c_string),
        ("ZK-FFI:UTF8-PREFIX", utf8::prefix),
        ("ZK-FFI:WRITE-UTF8", utf8::write),
    ];
    for &(name, op) in operations {
        vm::install_host_function(rt, name, std::sync::Arc::new(op))?;
    }
    Ok(())
}

struct Allocation {
    base: NonNull<u8>,
    live: Cell<bool>,
    size: usize,
    layout: Layout,
}

impl Allocation {
    fn rank(&self, raw: *mut c_void) -> AliasRank {
        let Some(offset) = raw.addr().checked_sub(self.base.as_ptr().addr()) else {
            return AliasRank::Unknown;
        };
        if offset > self.size {
            return AliasRank::Unknown;
        }
        if !self.live.get() {
            return AliasRank::Freed;
        }
        if offset > 0 && offset == self.size {
            AliasRank::OnePast
        } else {
            AliasRank::Allocated
        }
    }

    fn release(&self) {
        if self.live.replace(false) {
            // SAFETY: this is the base address and original global allocator layout.
            // All aliases share the same cell, preventing a second deallocation.
            unsafe { dealloc(self.base.as_ptr(), self.layout) };
        }
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        self.release();
    }
}

/// Writable native bytes for a temporary C string. The allocation stays at
/// the same address if native code hands it (or an interior pointer) back.
pub(super) struct StringBuffer(Rc<Allocation>);
pub(super) type StringBuffers = SmallVec<[StringBuffer; 2]>;

impl StringBuffer {
    pub(super) fn new(text: CString) -> Self {
        let bytes = text.into_bytes_with_nul().into_boxed_slice();
        let size = bytes.len();
        let layout = Layout::for_value(bytes.as_ref());
        // Ownership transfers to Allocation, which frees the buffer with the
        // global allocator. There is no CString left for C writes to invalidate.
        let base = NonNull::from(Box::leak(bytes)).cast::<u8>();
        Self(Rc::new(Allocation {
            base,
            live: Cell::new(true),
            size,
            layout,
        }))
    }

    pub(super) fn as_ptr(&self) -> *mut c_void {
        self.0.base.as_ptr().cast()
    }

    pub(super) fn pointer<'rt>(
        &self,
        rt: &'rt Runtime,
        raw: *mut c_void,
        at: usize,
    ) -> Result<Root<'rt>, Fault> {
        debug_assert!(self.0.rank(raw) != AliasRank::Unknown);
        Pointer {
            raw: raw.cast(),
            allocation: Some(Rc::clone(&self.0)),
            origin: Origin::default(),
        }
        .root(rt, at)
    }
}

/// Keep the executable/library together with the GC edge for any Lisp callback
/// it owns. Both must be propagated across derived pointers and native results.
#[derive(Clone, Default)]
pub(super) struct Origin {
    owner: Option<Rc<dyn Any>>,
    edge: Option<ForeignEdge>,
    // The edge's list starts with these known pointer-field owners.
    pointer_count: usize,
}

impl Origin {
    pub(super) fn owned(owner: Rc<dyn Any>) -> Self {
        Self {
            owner: Some(owner),
            ..Self::default()
        }
    }

    pub(super) fn wrap<'rt>(
        &self,
        rt: &'rt Runtime,
        raw: *mut c_void,
        at: usize,
    ) -> Result<Root<'rt>, Fault> {
        Pointer::external(raw, self.clone()).root(rt, at)
    }
}

#[derive(Clone)]
pub(super) struct Pointer {
    raw: *mut u8,
    allocation: Option<Rc<Allocation>>,
    origin: Origin,
}

impl Pointer {
    pub(super) fn external(raw: *mut c_void, origin: Origin) -> Self {
        Self {
            raw: raw.cast(),
            allocation: None,
            origin,
        }
    }

    pub(super) fn raw(&self) -> *mut c_void {
        self.raw.cast()
    }

    pub(super) fn origin(&self) -> &Origin {
        &self.origin
    }

    pub(super) fn root(self, rt: &Runtime, at: usize) -> Result<Root<'_>, Fault> {
        if self.raw.is_null() {
            return Ok(rt.nil());
        }
        let edge = self.origin.edge.clone();
        Ok(rt
            .foreign_with_edge(self, edge)
            .map_err(|e| EvalError::out_of_memory(at, e))?)
    }

    pub(super) fn get(rt: &Runtime, value: &Root<'_>, at: usize) -> Result<Self, Fault> {
        Self::inspect(rt, value.slot.get(), at, |pointer| Ok(pointer.clone()))
    }

    /// `word` comes from this runtime's rooted slots. `read` inspects the
    /// pointer without allocating in the Lisp heap or reentering the runtime.
    fn inspect<T>(
        rt: &Runtime,
        word: Word,
        at: usize,
        read: impl FnOnce(&Self) -> Result<T, EvalError>,
    ) -> Result<T, Fault> {
        let invalid = || EvalError::new(at, "expected a foreign pointer or NIL");
        if word == Word::NIL {
            return Ok(read(&Self::external(ptr::null_mut(), Origin::default()))?);
        }
        let id = rt.heap.foreign_id(word).ok_or_else(invalid)?;
        let objects = rt.foreign_objects.borrow();
        let value = objects
            .get(&id)
            .ok_or_else(invalid)?
            .value
            .try_borrow()
            .map_err(|_| invalid())?;
        let pointer = value.downcast_ref::<Self>().ok_or_else(invalid)?;
        Ok(read(pointer)?)
    }

    /// Remaining accessible bytes for an allocation, including a zero-length
    /// range at its one-past address. External ranges are supplied by the caller.
    fn remaining(&self, at: usize) -> Result<Option<usize>, EvalError> {
        let Some(allocation) = &self.allocation else {
            return Ok(None);
        };
        self.check_live(at)?;
        let remaining = self
            .raw
            .addr()
            .checked_sub(allocation.base.as_ptr().addr())
            .and_then(|offset| allocation.size.checked_sub(offset))
            .ok_or_else(|| EvalError::new(at, "pointer lies outside the foreign allocation"))?;
        Ok(Some(remaining))
    }

    pub(super) fn check_live(&self, at: usize) -> Result<(), EvalError> {
        if self
            .allocation
            .as_ref()
            .is_some_and(|allocation| !allocation.live.get())
        {
            return Err(EvalError::new(at, "foreign allocation has been freed"));
        }
        Ok(())
    }

    pub(super) fn check_function_address(&self, at: usize) -> Result<(), EvalError> {
        if self.remaining(at)? == Some(0) {
            return Err(EvalError::new(
                at,
                "function address is at the end of a foreign allocation",
            ));
        }
        Ok(())
    }

    fn offset(mut self, delta: isize, at: usize) -> Result<Self, EvalError> {
        self.raw
            .addr()
            .checked_add_signed(delta)
            .ok_or_else(|| EvalError::new(at, "foreign pointer address overflow"))?;
        self.raw = self.raw.wrapping_offset(delta);
        Ok(self)
    }

    fn access(&self, size: usize, at: usize) -> Result<*mut u8, EvalError> {
        if self.raw.is_null() {
            return Err(EvalError::new(at, "cannot dereference a null pointer"));
        }
        if self
            .remaining(at)?
            .is_some_and(|remaining| size > remaining)
            || size > isize::MAX as usize
            || self.raw.addr().checked_add(size).is_none()
        {
            return Err(EvalError::new(at, "foreign memory access is out of bounds"));
        }
        Ok(self.raw)
    }

    fn rank(&self, raw: *mut c_void) -> AliasRank {
        if let Some(allocation) = &self.allocation {
            return allocation.rank(raw);
        }
        if self.raw.cast::<c_void>() != raw {
            return AliasRank::Unknown;
        }
        if self.origin.owner.is_some() || self.origin.edge.is_some() {
            AliasRank::OwnedExternal
        } else {
            AliasRank::External
        }
    }
}

pub(super) fn from_raw<'rt>(
    rt: &'rt Runtime,
    ptr: *mut c_void,
    owner: Option<Rc<dyn Any>>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    Origin {
        owner,
        ..Origin::default()
    }
    .wrap(rt, ptr, at)
}

pub(super) fn from_callback<'rt>(
    rt: &'rt Runtime,
    ptr: *mut c_void,
    owner: Rc<dyn Any>,
    edge: &ForeignEdge,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    Origin {
        owner: Some(owner),
        edge: Some(edge.clone()),
        pointer_count: 0,
    }
    .wrap(rt, ptr, at)
}

/// The caller must keep the Lisp root alive and may only use this range while
/// the foreign allocation is live (including across any possible reentry).
pub(super) fn range(
    rt: &Runtime,
    value: &Root<'_>,
    size: usize,
    at: usize,
) -> Result<*mut c_void, Fault> {
    Ok(Pointer::get(rt, value, at)?.access(size, at)?.cast())
}

pub(super) fn address(rt: &Runtime, value: &Root<'_>, at: usize) -> Result<*mut c_void, Fault> {
    address_word(rt, value.slot.get(), at)
}

pub(super) fn address_word(rt: &Runtime, word: Word, at: usize) -> Result<*mut c_void, Fault> {
    Pointer::inspect(rt, word, at, |pointer| {
        pointer.remaining(at)?;
        Ok(pointer.raw())
    })
}

// One-past can equal the start of the next C object. A callback may also
// free an input and allocate fresh storage at its old address during the call.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AliasRank {
    Unknown,
    Freed,
    OnePast,
    External,
    OwnedExternal,
    Allocated,
}

pub(super) struct AliasSearch<'rt> {
    raw: *mut c_void,
    best: Option<(AliasRank, AliasSource<'rt>)>,
}

enum AliasSource<'rt> {
    LispPointer(Root<'rt>),
    StringBuffer(Rc<Allocation>),
}

impl<'rt> AliasSearch<'rt> {
    pub(super) fn new(raw: *mut c_void) -> Self {
        Self { raw, best: None }
    }

    fn rank(&self) -> AliasRank {
        self.best
            .as_ref()
            .map_or(AliasRank::Unknown, |(rank, _)| *rank)
    }

    fn note(&mut self, pointer: &Pointer, source: &Root<'rt>) {
        let rank = pointer.rank(self.raw);
        if rank > self.rank() {
            self.best = Some((rank, AliasSource::LispPointer(source.clone())));
        }
    }

    pub(super) fn found_allocation(&self) -> bool {
        self.rank() == AliasRank::Allocated
    }

    pub(super) fn consider(
        &mut self,
        rt: &'rt Runtime,
        input: &Root<'rt>,
        at: usize,
    ) -> Result<(), Fault> {
        if self.found_allocation() {
            return Ok(());
        }
        let pointer = Pointer::get(rt, input, at)?;
        self.note(&pointer, input);
        if pointer.origin.pointer_count == 0 || self.found_allocation() {
            return Ok(());
        }

        // Nested by-value results can share entire lists of pointer-field
        // owners. Walk each list iteratively once, including any older results.
        let mut pending = vec![pointer.origin];
        let mut visited = HashSet::new();
        while let Some(origin) = pending.pop() {
            let mut list = origin
                .edge
                .as_ref()
                .and_then(|edge| edge.bind(rt))
                .expect("traced pointer list");
            // Root construction/inspection below cannot allocate in the Lisp heap.
            if !visited.insert(list.slot.get()) {
                continue;
            }
            for _ in 0..origin.pointer_count {
                let (root, rest) = rt.cons_fields(&list).expect("traced pointer list cell");
                let pointer = Pointer::get(rt, &root, at)?;
                self.note(&pointer, &root);
                if self.found_allocation() {
                    return Ok(());
                }
                if pointer.origin.pointer_count > 0 {
                    pending.push(pointer.origin);
                }
                list = rest;
            }
        }
        Ok(())
    }

    pub(super) fn consider_strings(&mut self, buffers: &[StringBuffer]) {
        let current = self.rank();
        let best = buffers
            .iter()
            .map(|buffer| (buffer, buffer.0.rank(self.raw)))
            .filter(|&(_, rank)| rank > current)
            .max_by_key(|&(_, rank)| rank);
        if let Some((buffer, rank)) = best {
            self.best = Some((rank, AliasSource::StringBuffer(Rc::clone(&buffer.0))));
        }
    }

    fn pointer(&self, rt: &Runtime, at: usize) -> Result<Option<Pointer>, Fault> {
        let pointer = match &self.best {
            None => return Ok(None),
            Some((_, AliasSource::LispPointer(root))) => Pointer::get(rt, root, at)?,
            Some((_, AliasSource::StringBuffer(allocation))) => Pointer {
                raw: self.raw.cast(),
                allocation: Some(Rc::clone(allocation)),
                origin: Origin::default(),
            },
        };
        // Only the best candidate's deallocation is relevant.
        pointer.remaining(at)?;
        Ok(Some(pointer))
    }

    pub(super) fn finish(self, rt: &'rt Runtime, at: usize) -> Result<Option<Root<'rt>>, Fault> {
        let Some(mut pointer) = self.pointer(rt, at)? else {
            return Ok(None);
        };
        if pointer.raw() == self.raw
            && let Some((_, AliasSource::LispPointer(root))) = self.best
        {
            return Ok(Some(root));
        }
        pointer.raw = self.raw.cast();
        Ok(Some(pointer.root(rt, at)?))
    }

    pub(super) fn read_string(self, rt: &'rt Runtime, at: usize) -> Result<Root<'rt>, Fault> {
        let mut pointer = self
            .pointer(rt, at)?
            .unwrap_or_else(|| Pointer::external(self.raw, Origin::default()));
        pointer.raw = self.raw.cast();
        decode(rt, &pointer, None, at)
    }
}

fn allocate<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [size] = args else {
        return Err(arity(at, "ALLOCATE expects a byte count"));
    };
    let size = integer::<usize>(rt, size, at)?;
    let alignment = align_of::<u64>()
        .max(align_of::<f64>())
        .max(align_of::<*mut c_void>());
    let layout = Layout::from_size_align(size, alignment)
        .map_err(|source| EvalError::from_error(at, ForeignCallError::AllocationLayout(source)))?;
    Ok(vec![allocate_with_origin(
        rt,
        layout,
        Origin::default(),
        at,
    )?])
}

/// Publish all pointer-field owners through a single traced edge. The Rust
/// root keeps the list live while allocating the Lisp wrapper can trigger GC.
pub(super) fn allocate_retaining<'rt>(
    rt: &'rt Runtime,
    layout: Layout,
    mut origin: Origin,
    roots: Vec<Root<'rt>>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    if roots.is_empty() {
        return allocate_with_origin(rt, layout, origin, at);
    }
    let mut list = match &origin.edge {
        Some(edge) => {
            let previous = edge.bind(rt).expect("origin belongs to this runtime");
            if origin.pointer_count > 0 {
                previous
            } else {
                rt.cons(&previous, &rt.nil())
                    .map_err(|e| EvalError::out_of_memory(at, e))?
            }
        }
        None => rt.nil(),
    };
    origin.pointer_count += roots.len();
    for root in roots {
        list = rt
            .cons(&root, &list)
            .map_err(|e| EvalError::out_of_memory(at, e))?;
    }
    let edge = ForeignEdge::empty(rt);
    origin.edge = Some(edge.clone());
    let result = allocate_with_origin(rt, layout, origin, at)?;
    edge.set(&list);
    Ok(result)
}

pub(super) fn allocate_with_origin<'rt>(
    rt: &'rt Runtime,
    layout: Layout,
    origin: Origin,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let size = layout.size();
    let layout = if size == 0 {
        Layout::from_size_align(1, layout.align()).expect("valid alignment")
    } else {
        layout
    };
    // SAFETY: the layout is nonzero and valid. Allocation::release owns the result.
    let raw = unsafe { alloc_zeroed(layout) };
    let base = NonNull::new(raw).ok_or_else(|| EvalError::out_of_memory(at, OutOfMemory))?;
    let allocation = Rc::new(Allocation {
        base,
        live: Cell::new(true),
        size,
        layout,
    });
    Pointer {
        raw,
        allocation: Some(allocation),
        origin,
    }
    .root(rt, at)
}

fn free<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [value] = args else {
        return Err(arity(at, "FREE expects an allocation pointer"));
    };
    if !value.is_nil() {
        let p = Pointer::get(rt, value, at)?;
        let Some(allocation) = &p.allocation else {
            return Err(EvalError::new(at, "FREE requires a pointer from ALLOCATE").into());
        };
        p.remaining(at)?;
        if p.raw != allocation.base.as_ptr() {
            return Err(EvalError::new(at, "FREE requires the base of the allocation").into());
        }
        allocation.release();
    }
    Ok(vec![rt.nil()])
}

fn pointer_p<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [value] = args else {
        return Err(arity(at, "POINTER-P expects one argument"));
    };
    let yes = value.is_nil() || value.with_owned_foreign_ref::<Pointer, _>(rt, |_| ()).is_ok();
    Ok(vec![if yes { rt.t() } else { rt.nil() }])
}

fn pointer_address<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [p] = args else {
        return Err(arity(at, "POINTER-ADDRESS expects one pointer"));
    };
    Ok(vec![
        rt.integer_from(address(rt, p, at)?.expose_provenance())
            .map_err(|e| EvalError::out_of_memory(at, e))?,
    ])
}

fn make_pointer<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [value] = args else {
        return Err(arity(at, "MAKE-POINTER expects an address"));
    };
    let raw = ptr::with_exposed_provenance_mut(integer::<usize>(rt, value, at)?);
    Ok(vec![from_raw(rt, raw, None, at)?])
}

fn inc_pointer<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [p, offset] = args else {
        return Err(arity(at, "INC-POINTER expects a pointer and byte offset"));
    };
    let p = Pointer::get(rt, p, at)?.offset(integer::<isize>(rt, offset, at)?, at)?;
    p.remaining(at)?;
    Ok(vec![p.root(rt, at)?])
}

impl CType {
    pub(super) fn layout(self, at: usize) -> Result<Layout, EvalError> {
        Ok(match self {
            Self::I8 => Layout::new::<i8>(),
            Self::U8 => Layout::new::<u8>(),
            Self::I16 => Layout::new::<i16>(),
            Self::U16 => Layout::new::<u16>(),
            Self::I32 => Layout::new::<i32>(),
            Self::U32 => Layout::new::<u32>(),
            Self::I64 => Layout::new::<i64>(),
            Self::U64 => Layout::new::<u64>(),
            Self::Float => Layout::new::<f32>(),
            Self::Double => Layout::new::<f64>(),
            Self::Pointer | Self::CString => Layout::new::<*mut c_void>(),
            Self::Void => return Err(EvalError::new(at, ":VOID has no object layout")),
        })
    }
}

fn type_size<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [ty] = args else {
        return Err(arity(at, "TYPE-SIZE expects one C type"));
    };
    let size = match ForeignType::parse(rt, ty, at)? {
        ForeignType::Scalar(ty) => ty.layout(at)?.size(),
        ForeignType::Struct(ty) => ty.size(),
    };
    Ok(vec![
        rt.integer_from(size)
            .map_err(|e| EvalError::out_of_memory(at, e))?,
    ])
}

fn type_alignment<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [ty] = args else {
        return Err(arity(at, "TYPE-ALIGNMENT expects one C type"));
    };
    let align = match ForeignType::parse(rt, ty, at)? {
        ForeignType::Scalar(ty) => ty.layout(at)?.align(),
        ForeignType::Struct(ty) => ty.alignment(),
    };
    Ok(vec![
        rt.integer_from(align)
            .map_err(|e| EvalError::out_of_memory(at, e))?,
    ])
}

fn mem_ref<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let (value, ty, offset) = match args {
        [p, ty] => (p, ty, 0),
        [p, ty, offset] => (p, ty, integer(rt, offset, at)?),
        _ => {
            return Err(arity(
                at,
                "MEM-REF expects a pointer, C type and optional byte offset",
            ));
        }
    };
    let p = Pointer::get(rt, value, at)?.offset(offset, at)?;
    let ty = CType::parse(rt, ty, at)?;
    let raw = p.access(ty.layout(at)?.size(), at)?;
    // SAFETY: access checks the managed range. For external storage the opted-in
    // caller guarantees readable initialized bytes of the specified C type.
    let value = unsafe {
        match ty {
            CType::I8 => rt.integer_from(raw.cast::<i8>().read_unaligned()),
            CType::U8 => rt.integer_from(raw.read_unaligned()),
            CType::I16 => rt.integer_from(raw.cast::<i16>().read_unaligned()),
            CType::U16 => rt.integer_from(raw.cast::<u16>().read_unaligned()),
            CType::I32 => rt.integer_from(raw.cast::<i32>().read_unaligned()),
            CType::U32 => rt.integer_from(raw.cast::<u32>().read_unaligned()),
            CType::I64 => rt.integer_from(raw.cast::<i64>().read_unaligned()),
            CType::U64 => rt.integer_from(raw.cast::<u64>().read_unaligned()),
            CType::Float => rt.make_single_float(raw.cast::<f32>().read_unaligned()),
            CType::Double => rt.make_double_float(raw.cast::<f64>().read_unaligned()),
            CType::Pointer | CType::CString => {
                let address = raw.cast::<*mut c_void>().read_unaligned();
                let mut owners = AliasSearch::new(address);
                owners.consider(rt, value, at)?;
                // C can also initialize the slot with independently owned native
                // storage. Preserve the source library/edge for those pointers.
                let mut target = owners.pointer(rt, at)?.unwrap_or_else(|| Pointer {
                    raw: address.cast(),
                    allocation: None,
                    origin: p.origin.clone(),
                });
                target.raw = address.cast();
                return Ok(vec![if ty == CType::CString {
                    decode(rt, &target, None, at)?
                } else {
                    target.root(rt, at)?
                }]);
            }
            CType::Void => unreachable!("object layout checked above"),
        }
    }
    .map_err(|e| EvalError::out_of_memory(at, e))?;
    Ok(vec![value])
}

fn mem_set<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [p, ty, offset, value] = args else {
        return Err(arity(
            at,
            "MEM-SET expects a pointer, C type, byte offset and value",
        ));
    };
    let p = Pointer::get(rt, p, at)?.offset(integer::<isize>(rt, offset, at)?, at)?;
    let ty = CType::parse(rt, ty, at)?;
    let raw = p.access(ty.layout(at)?.size(), at)?;
    // A char* slot stores an existing pointer. A temporary CString created for
    // a function argument would be dangling immediately after this operation.
    let ty = if ty == CType::CString {
        CType::Pointer
    } else {
        ty
    };
    let arg = Argument::convert(rt, value, ty, &mut StringBuffers::new(), at)?;
    // SAFETY: access checks the managed range. The opted-in caller guarantees
    // any external storage is writable. The conversion uses the same scalar type.
    unsafe {
        match arg {
            Argument::I8(x) => raw.cast::<i8>().write_unaligned(x),
            Argument::U8(x) => raw.write_unaligned(x),
            Argument::I16(x) => raw.cast::<i16>().write_unaligned(x),
            Argument::U16(x) => raw.cast::<u16>().write_unaligned(x),
            Argument::I32(x) => raw.cast::<i32>().write_unaligned(x),
            Argument::U32(x) => raw.cast::<u32>().write_unaligned(x),
            Argument::I64(x) => raw.cast::<i64>().write_unaligned(x),
            Argument::U64(x) => raw.cast::<u64>().write_unaligned(x),
            Argument::Float(x) => raw.cast::<f32>().write_unaligned(x),
            Argument::Double(x) => raw.cast::<f64>().write_unaligned(x),
            Argument::Pointer(x) => raw.cast::<*mut c_void>().write_unaligned(x),
        }
    }
    Ok(vec![value.clone()])
}

fn decode<'rt>(
    rt: &'rt Runtime,
    p: &Pointer,
    length: Option<usize>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    if p.raw.is_null() {
        return Ok(rt.nil());
    }
    let bytes = match length {
        Some(len) => {
            let raw = p.access(len, at)?;
            // SAFETY: declared byte count checked above for managed storage.
            unsafe { std::slice::from_raw_parts(raw, len) }
        }
        None => match p.remaining(at)? {
            Some(remaining) => {
                // A C function may have left uninitialized padding after its
                // terminator. Only read up to the first NUL or the known bound.
                let len = (0..remaining)
                    .find(|&i| unsafe { p.raw.add(i).read() } == 0)
                    .ok_or_else(|| {
                        EvalError::new(at, "no NUL terminator within the foreign allocation")
                    })?;
                // SAFETY: each of these bytes was read above.
                unsafe { std::slice::from_raw_parts(p.raw, len) }
            }
            // SAFETY: for external pointers the opted-in caller supplies a
            // readable NUL-terminated string that lives through this operation.
            None => unsafe { CStr::from_ptr(p.raw.cast()) }.to_bytes(),
        },
    };
    let text = std::str::from_utf8(bytes)
        .map_err(|e| EvalError::from_error(at, ForeignCallError::ReturnedString(e)))?;
    Ok(rt
        .make_string(text)
        .map_err(|e| EvalError::out_of_memory(at, e))?)
}

pub(super) fn read_string_pointer<'rt>(
    rt: &'rt Runtime,
    value: &Root<'rt>,
    length: Option<usize>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    decode(rt, &Pointer::get(rt, value, at)?, length, at)
}

fn read_c_string<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let (p, length) = match args {
        [p] => (p, None),
        [p, len] => (p, Some(integer::<usize>(rt, len, at)?)),
        _ => {
            return Err(arity(
                at,
                "READ-C-STRING expects a pointer and optional byte count",
            ));
        }
    };
    Ok(vec![read_string_pointer(rt, p, length, at)?])
}

fn write_c_string<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let (value, text, payload_limit) = match args {
        [p, text] => (p, text, usize::MAX),
        [p, text, capacity] => {
            let size = integer::<usize>(rt, capacity, at)?;
            let limit = size.checked_sub(1).ok_or_else(|| {
                EvalError::new(at, "a C string buffer needs space for a terminating NUL")
            })?;
            (p, text, limit)
        }
        _ => {
            return Err(arity(
                at,
                "WRITE-C-STRING expects a pointer, a string, and optionally a capacity in bytes",
            ));
        }
    };
    let text = string(rt, text, at)?;
    let length = text.floor_char_boundary(payload_limit.min(text.len()));
    let text = std::ffi::CString::new(text).map_err(|source| {
        EvalError::from_error(
            at,
            ForeignCallError::InteriorNul {
                argument: "string",
                source,
            },
        )
    })?;
    let destination = Pointer::get(rt, value, at)?.access(length + 1, at)?;
    // SAFETY: the complete destination is checked before writing, and text
    // owns a separate allocation. Capacity includes the final NUL.
    unsafe {
        ptr::copy_nonoverlapping(text.as_bytes().as_ptr(), destination, length);
        destination.add(length).write(0);
    }
    Ok(vec![value.clone()])
}

#[cfg(test)]
mod tests {
    use std::alloc::LayoutError;
    use std::error::Error;

    use crate::Runtime;

    #[test]
    fn oversized_allocation_preserves_the_layout_error() {
        // SAFETY: this input exercises layout validation without calling native code.
        let runtime = unsafe { Runtime::builder().allow_foreign_calls() }
            .start()
            .unwrap();
        let error = runtime
            .load_str(&format!("(zk-ffi:allocate {})", usize::MAX))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("foreign allocation is too large")
        );
        assert!(
            std::iter::successors(error.source(), |&cause| cause.source())
                .any(|cause| cause.is::<LayoutError>())
        );
    }
}
