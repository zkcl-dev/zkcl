//! Native structure types share one libffi layout for calls and memory queries.

use std::alloc::{Layout, alloc, dealloc};
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ops::Range;
use std::ptr::{self, NonNull};

use libffi::low::ffi_abi_FFI_DEFAULT_ABI;
use libffi::middle::{Cif, CodePtr, Type};
use zkcl_syntax::foreign::MAX_STRUCT_DEPTH;

use super::{CType, CallResult, ForeignCallError, arity, callback, integer, memory};
use crate::condition::{Fault, named_word};
use crate::heap::Word;
use crate::package::Package;
use crate::{EvalError, OutOfMemory, Root, Runtime, vm};

pub(super) fn initialize(rt: &Runtime) -> Result<(), EvalError> {
    vm::install_host_function(rt, "ZK-FFI:FIELD-OFFSET", std::sync::Arc::new(field_offset))
}

#[derive(Clone, Debug)]
pub(super) enum ForeignType {
    Scalar(CType),
    Struct(Box<StructType>),
}

impl ForeignType {
    pub(super) fn parse(rt: &Runtime, value: &Root<'_>, at: usize) -> Result<Self, EvalError> {
        Self::parse_field(rt, value.slot.get(), 0, at)
    }

    pub(super) fn parse_parameters(
        rt: &Runtime,
        values: &[Root<'_>],
        at: usize,
    ) -> Result<Vec<Self>, EvalError> {
        values
            .iter()
            .map(|value| {
                let ty = Self::parse(rt, value, at)?;
                if matches!(ty, Self::Scalar(CType::Void)) {
                    return Err(EvalError::new(at, ":VOID is only valid as a result type"));
                }
                Ok(ty)
            })
            .collect()
    }

    fn parse_field(rt: &Runtime, value: Word, depth: usize, at: usize) -> Result<Self, EvalError> {
        let Some((head, tail)) = rt.heap.cons_fields(value) else {
            return CType::parse(rt, &rt.root(value), at).map(Self::Scalar);
        };
        if named_word(rt, head, Package::KEYWORD).as_deref() != Some("STRUCT") {
            return Err(EvalError::new(at, "expected (:STRUCT field-type ...)"));
        }
        // libffi also recursively clones, classifies, and destroys the type tree.
        // This bounds those native stack frames for deeply nested or circular inputs.
        if depth >= MAX_STRUCT_DEPTH {
            return Err(EvalError::new(
                at,
                format!("C structure nesting exceeds {MAX_STRUCT_DEPTH} levels"),
            ));
        }
        let items = vm::proper_list(rt, tail, at)?;
        if items.is_empty() {
            return Err(EvalError::new(
                at,
                "a C structure requires at least one field",
            ));
        }
        let mut fields = Vec::with_capacity(items.len());
        for item in items {
            let field = Self::parse_field(rt, item, depth + 1, at)?;
            if matches!(field, Self::Scalar(CType::Void)) {
                return Err(EvalError::new(at, ":VOID cannot be a C structure field"));
            }
            fields.push(field);
        }
        Ok(Self::Struct(Box::new(StructType::new(fields, at)?)))
    }

    pub(super) fn abi(&self) -> Type {
        match self {
            Self::Scalar(ty) => ty.abi(),
            Self::Struct(ty) => ty.abi.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct StructType {
    abi: Type,
    layout: Layout,
    return_layout: Layout,
    offsets: Vec<usize>,
    // Internal, nested, and trailing padding that may be uninitialized in C.
    padding: Vec<Range<usize>>,
    has_pointer_fields: bool,
}

impl StructType {
    fn new(fields: Vec<ForeignType>, at: usize) -> Result<Self, EvalError> {
        let mut abi = Type::structure(fields.iter().map(ForeignType::abi));
        let offsets = abi
            .struct_offsets(ffi_abi_FFI_DEFAULT_ABI)
            .map_err(|reason| EvalError::from_error(at, ForeignCallError::StructLayout(reason)))?;
        // SAFETY: struct_offsets has initialized both fields in this owned ffi_type.
        let raw = unsafe { &*abi.as_raw_ptr() };
        let layout =
            Layout::from_size_align(raw.size, usize::from(raw.alignment)).map_err(|source| {
                EvalError::from_error(at, ForeignCallError::StructAddressSpace(source))
            })?;

        // ffi_call requires at least a register-sized return buffer. Eight bytes
        // also covers LLP64 and x32, where ffi_arg can exceed unsigned long.
        let register = Layout::new::<u64>();
        let return_layout = Layout::from_size_align(
            layout.size().max(register.size()),
            layout.align().max(register.align()),
        )
        .map_err(|source| {
            EvalError::from_error(at, ForeignCallError::StructReturnLayout(source))
        })?;

        let mut padding = Vec::new();
        let mut has_pointer_fields = false;
        let mut end = 0;
        for (field, &offset) in fields.into_iter().zip(&offsets) {
            if end < offset {
                padding.push(end..offset);
            }
            let size = match field {
                ForeignType::Scalar(ty) => {
                    has_pointer_fields |= matches!(ty, CType::Pointer | CType::CString);
                    ty.layout(at)?.size()
                }
                ForeignType::Struct(ty) => {
                    let size = ty.size();
                    has_pointer_fields |= ty.has_pointer_fields;
                    padding.extend(
                        ty.padding
                            .into_iter()
                            .map(|gap| offset + gap.start..offset + gap.end),
                    );
                    size
                }
            };
            end = offset + size;
        }
        if end < layout.size() {
            padding.push(end..layout.size());
        }
        Ok(Self {
            abi,
            layout,
            return_layout,
            offsets,
            padding,
            has_pointer_fields,
        })
    }

    pub(super) fn size(&self) -> usize {
        self.layout.size()
    }

    pub(super) fn alignment(&self) -> usize {
        self.layout.align()
    }

    pub(super) fn has_pointer_fields(&self) -> bool {
        self.has_pointer_fields
    }

    pub(super) fn argument(
        &self,
        rt: &Runtime,
        value: &Root<'_>,
        at: usize,
    ) -> Result<StructBuffer, Fault> {
        let source = memory::range(rt, value, self.size(), at)?;
        let storage = StructBuffer::new(self.layout, at)?;
        // SAFETY: the source range is live and readable under the foreign memory
        // contract. Bytewise copying preserves possibly uninitialized padding.
        // The separate, aligned snapshot survives callback reentry and any writes
        // by libffi when the ABI passes a private copy indirectly.
        unsafe {
            ptr::copy_nonoverlapping(source.cast(), storage.bytes.as_ptr(), self.size());
        }
        Ok(storage)
    }

    /// The caller guarantees that the function, CIF, and arguments agree with
    /// the foreign declaration and remain valid through callback reentry.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal FFI aggregate invocation passes ABI call context directly"
    )]
    pub(super) unsafe fn call_result<'rt>(
        &self,
        rt: &'rt Runtime,
        cif: &Cif,
        function: CodePtr,
        args: &mut [*mut c_void],
        origin: memory::Origin,
        at: usize,
        inputs: Option<&callback::CallInputs<'_>>,
    ) -> Result<Root<'rt>, Fault> {
        let mut storage = StructBuffer::new(self.return_layout, at)?;
        callback::with_call_inputs(
            rt,
            at,
            inputs,
            || {
                // SAFETY: the binding supplies a prepared CIF for this structure.
                unsafe {
                    libffi::low::call_return_into(
                        cif.as_raw_ptr(),
                        function,
                        args.as_mut_ptr(),
                        storage.as_mut_ptr(),
                    );
                }
                storage
            },
            |storage, frame| {
                // A C result can leave unused pointer members uninitialized.
                // Keep possible owners without reading any pointer representation here.
                let owners = if self.has_pointer_fields {
                    frame.retain_pointers()?
                } else {
                    Vec::new()
                };
                // SAFETY: ffi_call used this buffer for the prepared struct type.
                unsafe { self.copy_to_lisp(rt, storage.bytes.as_ptr().cast(), origin, owners, at) }
            },
        )
    }

    /// libffi supplies the address of a by-value parameter with this layout.
    pub(super) unsafe fn copy_callback_argument<'rt>(
        &self,
        rt: &'rt Runtime,
        raw: *const c_void,
        at: usize,
        frame: &callback::CallFrame<'rt, '_>,
    ) -> Result<Root<'rt>, Fault> {
        let owners = if self.has_pointer_fields {
            frame.retain_pointers()?
        } else {
            Vec::new()
        };
        // SAFETY: C's incoming object lives throughout the synchronous callback.
        unsafe { self.copy_to_lisp(rt, raw, memory::Origin::default(), owners, at) }
    }

    unsafe fn copy_to_lisp<'rt>(
        &self,
        rt: &'rt Runtime,
        source: *const c_void,
        origin: memory::Origin,
        owners: Vec<Root<'rt>>,
        at: usize,
    ) -> Result<Root<'rt>, Fault> {
        let result = memory::allocate_retaining(rt, self.layout, origin, owners, at)?;
        let destination = memory::range(rt, &result, self.size(), at)?;
        // SAFETY: the source contains this C type and the fresh destination fits it.
        unsafe { self.copy_value(source, destination) };
        Ok(result)
    }

    /// The trampoline supplies writable libffi result storage for this C type.
    pub(super) unsafe fn write_callback_result(
        &self,
        rt: &Runtime,
        value: &Root<'_>,
        result: *mut c_void,
        at: usize,
    ) -> Result<(), Fault> {
        let source = memory::range(rt, value, self.size(), at)?;
        // SAFETY: the source range is readable, and result fits the declared type.
        unsafe { self.copy_value(source, result) };
        Ok(())
    }

    unsafe fn copy_value(&self, source: *const c_void, destination: *mut c_void) {
        // Rust byte values cannot expose uninitialized native padding. Move the
        // object representation first so even overlapping C return storage works,
        // then initialize only the gaps. Pointer and floating representations survive.
        let destination = destination.cast::<MaybeUninit<u8>>();
        unsafe {
            ptr::copy(source.cast(), destination, self.size());
            for gap in &self.padding {
                destination.add(gap.start).write_bytes(0, gap.len());
            }
        }
    }
}

/// Aligned storage for a by-value C struct argument or result, owned for the duration of the call.
pub(super) struct StructBuffer {
    bytes: NonNull<MaybeUninit<u8>>,
    layout: Layout,
}

impl StructBuffer {
    fn new(layout: Layout, at: usize) -> Result<Self, Fault> {
        // SAFETY: C structures are nonempty and have nonzero size.
        let bytes = NonNull::new(unsafe { alloc(layout) })
            .ok_or_else(|| EvalError::out_of_memory(at, OutOfMemory))?
            .cast();
        Ok(Self { bytes, layout })
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut c_void {
        self.bytes.as_ptr().cast()
    }
}

impl Drop for StructBuffer {
    fn drop(&mut self) {
        // SAFETY: this object uniquely owns the allocation with this layout.
        unsafe { dealloc(self.bytes.as_ptr().cast(), self.layout) };
    }
}

fn field_offset<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [ty, index] = args else {
        return Err(arity(
            at,
            "FIELD-OFFSET expects a C structure type and zero-based index",
        ));
    };
    let ForeignType::Struct(ty) = ForeignType::parse(rt, ty, at)? else {
        return Err(EvalError::new(at, "FIELD-OFFSET requires a C structure type").into());
    };
    let index = integer::<usize>(rt, index, at)?;
    let offset = ty
        .offsets
        .get(index)
        .ok_or_else(|| EvalError::new(at, "C structure field index out of bounds"))?;
    Ok(vec![
        rt.integer_from(*offset)
            .map_err(|e| EvalError::out_of_memory(at, e))?,
    ])
}
