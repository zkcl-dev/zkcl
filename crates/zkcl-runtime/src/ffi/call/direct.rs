//! Fixed homogeneous numeric signatures that Rust can call with an exact C prototype.

use std::ffi::c_void;
use std::mem::MaybeUninit;

use libffi::middle::CodePtr;

use super::{ForeignCallTarget, callback};
use crate::condition::Fault;
use crate::ffi::types::{CType, integer_word};
use crate::heap::{RootSlots, Word};
use crate::{EvalError, OutOfMemory, Root, Runtime};

pub(super) fn supports(ty: CType) -> bool {
    !matches!(ty, CType::CString | CType::Pointer | CType::Void)
}

pub(super) fn bind<'rt>(
    ty: CType,
    count: usize,
    target: ForeignCallTarget,
    rt: &'rt Runtime,
    capture: &Root<'_>,
) -> Result<Root<'rt>, OutOfMemory> {
    match ty {
        CType::I8 => bind_number::<i8>(count, target, rt, capture),
        CType::U8 => bind_number::<u8>(count, target, rt, capture),
        CType::I16 => bind_number::<i16>(count, target, rt, capture),
        CType::U16 => bind_number::<u16>(count, target, rt, capture),
        CType::I32 => bind_number::<i32>(count, target, rt, capture),
        CType::U32 => bind_number::<u32>(count, target, rt, capture),
        CType::I64 => bind_number::<i64>(count, target, rt, capture),
        CType::U64 => bind_number::<u64>(count, target, rt, capture),
        CType::Float => bind_number::<f32>(count, target, rt, capture),
        CType::Double => bind_number::<f64>(count, target, rt, capture),
        _ => unreachable!("only numeric signatures qualify for direct calls"),
    }
}

fn bind_number<'rt, T: Number>(
    count: usize,
    target: ForeignCallTarget,
    rt: &'rt Runtime,
    capture: &Root<'_>,
) -> Result<Root<'rt>, OutOfMemory> {
    match count {
        0 => bind_count::<T, 0>(target, rt, capture),
        1 => bind_count::<T, 1>(target, rt, capture),
        2 => bind_count::<T, 2>(target, rt, capture),
        _ => unreachable!("only short signatures qualify for direct calls"),
    }
}

fn bind_count<'rt, T: Number, const N: usize>(
    target: ForeignCallTarget,
    rt: &'rt Runtime,
    capture: &Root<'_>,
) -> Result<Root<'rt>, OutOfMemory> {
    rt.host_function(capture, move |rt, slots, at| {
        call::<T, N>(&target, rt, slots, at)
    })
}

fn call<T: Number, const N: usize>(
    target: &ForeignCallTarget,
    rt: &Runtime,
    slots: &RootSlots,
    at: usize,
) -> Result<Option<Word>, Fault> {
    target.check_arity(N, slots.len(), at)?;
    let mut args = [T::default(); N];
    for (index, arg) in args.iter_mut().enumerate() {
        *arg = T::read(rt, slots.get(index), at)?;
    }
    let address = target.call_address(at)?;
    // Keep the ordinary native callback frame and the target's own library
    // reference. A scalar C function can still invoke a callback saved earlier.
    let value = callback::with_call(
        rt,
        at,
        // SAFETY: binding checked this prototype shape. As for libffi, the
        // opted-in caller supplies a function with the declared C prototype.
        || unsafe { invoke(address, args) },
        |result, _| Ok(T::root(rt, result).map_err(|e| EvalError::out_of_memory(at, e))?),
    )?;
    Ok(Some(value.slot.get()))
}

unsafe fn invoke<T: Number, const N: usize>(address: CodePtr, args: [T; N]) -> T {
    let code = *address.as_fun();
    // SAFETY: Number has only C scalar implementations, and selection matched
    // arity and parameter/result type exactly, with no variadic arguments.
    unsafe {
        match N {
            0 => std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn() -> T>(code)(),
            1 => std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(T) -> T>(code)(
                args[0],
            ),
            2 => std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(T, T) -> T>(
                code,
            )(args[0], args[1]),
            _ => unreachable!(),
        }
    }
}

/// Each slot begins at the alignment of its numeric value. libffi reads only
/// the number of bytes in the prepared argument type.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub(super) struct NumericArgument([MaybeUninit<u8>; 8]);

impl NumericArgument {
    pub(super) const UNINIT: Self = Self([MaybeUninit::uninit(); 8]);

    pub(super) fn as_mut_ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr().cast()
    }
}

pub(super) type Encode = fn(&Runtime, Word, &mut NumericArgument, usize) -> Result<(), Fault>;

pub(super) fn encoder(ty: CType) -> Option<Encode> {
    Some(match ty {
        CType::I8 => encode::<i8>,
        CType::U8 => encode::<u8>,
        CType::I16 => encode::<i16>,
        CType::U16 => encode::<u16>,
        CType::I32 => encode::<i32>,
        CType::U32 => encode::<u32>,
        CType::I64 => encode::<i64>,
        CType::U64 => encode::<u64>,
        CType::Float => encode::<f32>,
        CType::Double => encode::<f64>,
        _ => return None,
    })
}

fn encode<T: Number>(
    rt: &Runtime,
    word: Word,
    slot: &mut NumericArgument,
    at: usize,
) -> Result<(), Fault> {
    let value = T::read(rt, word, at)?;
    // SAFETY: Number is implemented only by the listed native scalar types,
    // whose size and alignment are covered by NumericArgument.
    unsafe { slot.as_mut_ptr().cast::<T>().write(value) };
    Ok(())
}

trait Number: Copy + Default + 'static {
    fn read(rt: &Runtime, word: Word, at: usize) -> Result<Self, Fault>;
    fn root(rt: &Runtime, value: Self) -> Result<Root<'_>, OutOfMemory>;
}

macro_rules! integers {
    ($($ty:ty),+ $(,)?) => {$(
        impl Number for $ty {
            fn read(rt: &Runtime, word: Word, at: usize) -> Result<Self, Fault> {
                integer_word(rt, word, at)
            }

            fn root(rt: &Runtime, value: Self) -> Result<Root<'_>, OutOfMemory> {
                rt.integer_from(value)
            }
        }
    )+};
}
integers!(i8, u8, i16, u16, i32, u32, i64, u64);

impl Number for f32 {
    fn read(rt: &Runtime, word: Word, at: usize) -> Result<Self, Fault> {
        crate::ffi::types::single_float(rt, word, at)
    }

    fn root(rt: &Runtime, value: Self) -> Result<Root<'_>, OutOfMemory> {
        rt.make_single_float(value)
    }
}

impl Number for f64 {
    fn read(rt: &Runtime, word: Word, at: usize) -> Result<Self, Fault> {
        crate::ffi::types::double_float(rt, word, at)
    }

    fn root(rt: &Runtime, value: Self) -> Result<Root<'_>, OutOfMemory> {
        rt.make_double_float(value)
    }
}
