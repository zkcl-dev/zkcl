//! Scalar C types and conversion between rooted Lisp values and native argument storage.
use super::{ForeignCallError, memory};
use crate::condition::{Fault, named_word};
use crate::heap::Word;
use crate::package::Package;
use crate::{BigInt, EvalError, Root, Runtime, vm};
use libffi::middle::Type;
use std::ffi::{CString, c_char, c_int, c_long, c_longlong, c_short, c_void};
use zkcl_syntax::foreign::ForeignScalar;

pub(super) fn integer<T: num_traits::PrimInt + TryFrom<isize> + TryFrom<BigInt>>(
    rt: &Runtime,
    v: &Root<'_>,
    at: usize,
) -> Result<T, Fault> {
    integer_word(rt, v.slot.get(), at)
}

pub(super) fn integer_word<T: num_traits::PrimInt + TryFrom<isize> + TryFrom<BigInt>>(
    rt: &Runtime,
    word: Word,
    at: usize,
) -> Result<T, Fault> {
    let result = match word.as_fixnum() {
        Some(n) => T::try_from(n).ok(),
        None => rt.heap.integer(word).and_then(|n| T::try_from(n).ok()),
    };
    if let Some(number) = result {
        return Ok(number);
    }
    // Building the expected type can allocate and move the datum.
    let v = rt.root(word);
    let name = if T::min_value() < T::zero() {
        "SIGNED-BYTE"
    } else {
        "UNSIGNED-BYTE"
    };
    let bits = T::zero().count_zeros();
    let head = rt.intern(&crate::symbol::Name::common_lisp(name), at)?;
    let width = crate::heap::Word::fixnum(bits as isize).expect("primitive width fits a fixnum");
    let expected = vm::methods::list_from_words(rt, vec![head, width], at)?;
    Err(Fault::type_error_specifier(
        rt,
        v.slot.get(),
        expected,
        EvalError::new(at, format!("expected ({name} {bits})")),
    ))
}

pub(super) fn single_float(rt: &Runtime, word: Word, at: usize) -> Result<f32, Fault> {
    rt.heap.single_float(word).ok_or_else(|| {
        Fault::type_error(
            rt,
            word,
            "SINGLE-FLOAT",
            EvalError::new(at, "expected a SINGLE-FLOAT"),
        )
    })
}

pub(super) fn double_float(rt: &Runtime, word: Word, at: usize) -> Result<f64, Fault> {
    rt.heap.double_float(word).ok_or_else(|| {
        Fault::type_error(
            rt,
            word,
            "DOUBLE-FLOAT",
            EvalError::new(at, "expected a DOUBLE-FLOAT"),
        )
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CType {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Float,
    Double,
    CString,
    Pointer,
    Void,
}

impl CType {
    /// C's default argument promotions apply only past the fixed parameters.
    pub(super) fn promoted(self) -> Self {
        match self {
            Self::Float => Self::Double,
            Self::I8 | Self::I16 | Self::U8 => Self::integer_width(size_of::<c_int>(), true),
            Self::U16 => {
                Self::integer_width(size_of::<c_int>(), c_int::MAX as u64 >= u64::from(u16::MAX))
            }
            other => other,
        }
    }
    pub(super) fn parse(rt: &Runtime, value: &Root<'_>, at: usize) -> Result<Self, EvalError> {
        let name = named_word(rt, value.slot.get(), Package::KEYWORD)
            .ok_or_else(|| EvalError::new(at, "a C type must be a keyword"))?;
        let scalar = ForeignScalar::from_keyword(&name)
            .ok_or_else(|| EvalError::new(at, format!("unknown C type :{name}")))?;
        Ok(match scalar {
            ForeignScalar::Int8 => Self::I8,
            ForeignScalar::UInt8 => Self::U8,
            ForeignScalar::Int16 => Self::I16,
            ForeignScalar::UInt16 => Self::U16,
            ForeignScalar::Int32 => Self::I32,
            ForeignScalar::UInt32 => Self::U32,
            ForeignScalar::Int64 => Self::I64,
            ForeignScalar::UInt64 => Self::U64,
            ForeignScalar::Char => Self::integer_width(size_of::<c_char>(), c_char::MIN != 0),
            ForeignScalar::Short => Self::integer_width(size_of::<c_short>(), true),
            ForeignScalar::UShort => Self::integer_width(size_of::<c_short>(), false),
            ForeignScalar::Int => Self::integer_width(size_of::<c_int>(), true),
            ForeignScalar::UInt => Self::integer_width(size_of::<c_int>(), false),
            ForeignScalar::Long => Self::integer_width(size_of::<c_long>(), true),
            ForeignScalar::ULong => Self::integer_width(size_of::<c_long>(), false),
            ForeignScalar::LongLong => Self::integer_width(size_of::<c_longlong>(), true),
            ForeignScalar::ULongLong => Self::integer_width(size_of::<c_longlong>(), false),
            ForeignScalar::IntPtr => Self::integer_width(size_of::<isize>(), true),
            ForeignScalar::UIntPtr => Self::integer_width(size_of::<usize>(), false),
            ForeignScalar::Float => Self::Float,
            ForeignScalar::Double => Self::Double,
            ForeignScalar::CString => Self::CString,
            ForeignScalar::Pointer => Self::Pointer,
            ForeignScalar::Void => Self::Void,
        })
    }

    fn integer_width(bytes: usize, signed: bool) -> Self {
        match (bytes, signed) {
            (1, true) => Self::I8,
            (1, false) => Self::U8,
            (2, true) => Self::I16,
            (2, false) => Self::U16,
            (4, true) => Self::I32,
            (4, false) => Self::U32,
            (8, true) => Self::I64,
            (8, false) => Self::U64,
            _ => unreachable!("unsupported C integer width"),
        }
    }

    pub(super) fn abi(self) -> Type {
        match self {
            Self::I8 => Type::i8(),
            Self::U8 => Type::u8(),
            Self::I16 => Type::i16(),
            Self::U16 => Type::u16(),
            Self::I32 => Type::i32(),
            Self::U32 => Type::u32(),
            Self::I64 => Type::i64(),
            Self::U64 => Type::u64(),
            Self::Float => Type::f32(),
            Self::Double => Type::f64(),
            Self::CString | Self::Pointer => Type::pointer(),
            Self::Void => Type::void(),
        }
    }
}

pub(super) enum Argument {
    I8(i8),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    Float(f32),
    Double(f64),
    Pointer(*mut c_void),
}

impl Argument {
    pub(super) fn promote(self) -> Self {
        fn c_int_arg(n: c_int) -> Argument {
            match size_of::<c_int>() {
                2 => Argument::I16(n as i16),
                4 => Argument::I32(n),
                8 => Argument::I64(n as i64),
                _ => unreachable!("unsupported C int width"),
            }
        }
        match self {
            Self::I8(n) => c_int_arg(n as c_int),
            Self::U8(n) => c_int_arg(n as c_int),
            Self::I16(n) => c_int_arg(n as c_int),
            Self::U16(n) if c_int::MAX as u64 >= u64::from(u16::MAX) => c_int_arg(n as c_int),
            Self::Float(n) => Self::Double(f64::from(n)),
            other => other,
        }
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut c_void {
        match self {
            Self::I8(v) => std::ptr::from_mut(v).cast(),
            Self::U8(v) => std::ptr::from_mut(v).cast(),
            Self::I16(v) => std::ptr::from_mut(v).cast(),
            Self::U16(v) => std::ptr::from_mut(v).cast(),
            Self::I32(v) => std::ptr::from_mut(v).cast(),
            Self::U32(v) => std::ptr::from_mut(v).cast(),
            Self::I64(v) => std::ptr::from_mut(v).cast(),
            Self::U64(v) => std::ptr::from_mut(v).cast(),
            Self::Float(v) => std::ptr::from_mut(v).cast(),
            Self::Double(v) => std::ptr::from_mut(v).cast(),
            Self::Pointer(v) => std::ptr::from_mut(v).cast(),
        }
    }

    pub(super) fn convert(
        rt: &Runtime,
        value: &Root<'_>,
        ty: CType,
        strings: &mut memory::StringBuffers,
        at: usize,
    ) -> Result<Self, Fault> {
        Self::from_slot(rt, value.slot.get(), ty, strings, at)
    }

    pub(super) fn from_slot(
        rt: &Runtime,
        word: Word,
        ty: CType,
        strings: &mut memory::StringBuffers,
        at: usize,
    ) -> Result<Self, Fault> {
        Ok(match ty {
            CType::I8 => Self::I8(integer_word(rt, word, at)?),
            CType::U8 => Self::U8(integer_word(rt, word, at)?),
            CType::I16 => Self::I16(integer_word(rt, word, at)?),
            CType::U16 => Self::U16(integer_word(rt, word, at)?),
            CType::I32 => Self::I32(integer_word(rt, word, at)?),
            CType::U32 => Self::U32(integer_word(rt, word, at)?),
            CType::I64 => Self::I64(integer_word(rt, word, at)?),
            CType::U64 => Self::U64(integer_word(rt, word, at)?),
            CType::Float => Self::Float(single_float(rt, word, at)?),
            CType::Double => Self::Double(double_float(rt, word, at)?),
            CType::Pointer | CType::CString if word == Word::NIL => {
                Self::Pointer(std::ptr::null_mut())
            }
            CType::CString => {
                let s = rt
                    .heap
                    .with_string_slice(word, |text| CString::new(text))
                    .ok_or_else(|| {
                        Fault::type_error(rt, 
                            word,
                            "STRING",
                            EvalError::new(at, "expected a Lisp string"),
                        )
                    })?
                    .map_err(|source| {
                        EvalError::from_error(
                            at,
                            ForeignCallError::InteriorNul {
                                argument: "string",
                                source,
                            },
                        )
                    })?;
                let buffer = memory::StringBuffer::new(s);
                let ptr = buffer.as_ptr();
                strings.push(buffer);
                Self::Pointer(ptr)
            }
            CType::Pointer => Self::Pointer(memory::address_word(rt, word, at)?),
            CType::Void => unreachable!("void parameters rejected when binding"),
        })
    }
}
