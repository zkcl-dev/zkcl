use num_bigint::{BigInt, Sign};
use num_rational::BigRational;
use num_traits::ToPrimitive;

use crate::number::{MachineReal, Number, NumberKind, Real};

use super::{
    Address, Header, Heap, HeapObject, ObjectKind, OutOfMemory, SlotVisitor, Word, WordSlot,
    object_address, offset_of, visit_offsets,
};

#[repr(C)]
struct LispBignum {
    header: Header,
    len: usize,
    negative: bool,
    // Followed by native-endian u32 digits, least significant first.
}

pub(crate) struct BignumInfo {
    pub(crate) digits: usize,
    pub(crate) negative: bool,
}

// SAFETY: `LispBignum` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Bignum`.
unsafe impl HeapObject for LispBignum {
    const KIND: ObjectKind = ObjectKind::Bignum;
}

#[repr(C)]
struct LispFloat {
    header: Header,
    bits: u64,
}

#[repr(C)]
struct LispNumberPair {
    header: Header,
    left: WordSlot,
    right: WordSlot,
}

impl Heap {
    #[inline]
    pub(crate) fn machine_real(&self, word: Word) -> Option<MachineReal> {
        if let Some(value) = word.as_fixnum() {
            return Some(MachineReal::Fixnum(value));
        }
        let (address, kind) = boxed(word)?;
        Some(match kind {
            ObjectKind::SingleFloat => {
                MachineReal::Single(f32::from_bits(read_float(address) as u32))
            }
            ObjectKind::DoubleFloat => MachineReal::Double(f64::from_bits(read_float(address))),
            _ => return None,
        })
    }

    pub(crate) fn single_float(&self, word: Word) -> Option<f32> {
        object_address(word, ObjectKind::SingleFloat)
            .map(|address| f32::from_bits(read_float(address) as u32))
    }

    pub(crate) fn double_float(&self, word: Word) -> Option<f64> {
        object_address(word, ObjectKind::DoubleFloat)
            .map(|address| f64::from_bits(read_float(address)))
    }

    pub(crate) fn number_kind(&self, word: Word) -> Option<NumberKind> {
        if word.as_fixnum().is_some() {
            return Some(NumberKind::Fixnum);
        }
        let (_, kind) = boxed(word)?;
        Some(match kind {
            ObjectKind::Bignum => NumberKind::Bignum,
            ObjectKind::Ratio => NumberKind::Ratio,
            ObjectKind::SingleFloat => NumberKind::Single,
            ObjectKind::DoubleFloat => NumberKind::Double,
            ObjectKind::Complex => NumberKind::Complex,
            _ => return None,
        })
    }

    pub(crate) fn integer(&self, word: Word) -> Option<BigInt> {
        if let Some(value) = word.as_fixnum() {
            return Some(value.into());
        }
        object_address(word, ObjectKind::Bignum).map(read_bignum)
    }

    pub(crate) fn unsigned_integer64(&self, word: Word) -> Option<u64> {
        if let Some(value) = word.as_fixnum() {
            return u64::try_from(value).ok();
        }
        let address = object_address(word, ObjectKind::Bignum)?;
        with_bignum_digits(address, |negative, digits| match (negative, digits) {
            (false, []) => Some(0),
            (false, &[low]) => Some(u64::from(low)),
            (false, &[low, high]) => Some(u64::from(low) | (u64::from(high) << 32)),
            _ => None,
        })
    }

    /// Inspect the stored sign and number of 32-bit digits without copying them.
    pub(crate) fn bignum_info(&self, word: Word) -> Option<BignumInfo> {
        let number = self.object_ref::<LispBignum>(word)?;
        Some(BignumInfo {
            digits: number.len,
            negative: number.negative,
        })
    }

    pub(crate) fn number(&self, word: Word) -> Option<Number> {
        if let Some(value) = word.as_fixnum() {
            return Some(Number::Real(Real::Integer(value.into())));
        }
        let (address, kind) = boxed(word)?;
        if kind == ObjectKind::Complex {
            let (real, imaginary) = read_pair(address);
            return Some(Number::Complex(self.real(real)?, self.real(imaginary)?));
        }
        self.boxed_real(address, kind).map(Number::Real)
    }

    fn real(&self, word: Word) -> Option<Real> {
        if let Some(value) = word.as_fixnum() {
            return Some(Real::Integer(value.into()));
        }
        let (address, kind) = boxed(word)?;
        self.boxed_real(address, kind)
    }

    fn boxed_real(&self, address: Address, kind: ObjectKind) -> Option<Real> {
        Some(match kind {
            ObjectKind::Bignum => Real::Integer(read_bignum(address)),
            ObjectKind::Ratio => {
                let (numerator, denominator) = read_pair(address);
                // Construction stores a reduced fraction with positive denominator.
                Real::Ratio(BigRational::new_raw(
                    self.integer(numerator)?,
                    self.integer(denominator)?,
                ))
            }
            ObjectKind::SingleFloat => Real::Single(f32::from_bits(read_float(address) as u32)),
            ObjectKind::DoubleFloat => Real::Double(f64::from_bits(read_float(address))),
            _ => return None,
        })
    }

    pub(crate) fn ratio_fields(&self, word: Word) -> Option<(Word, Word)> {
        object_address(word, ObjectKind::Ratio).map(read_pair)
    }

    pub(crate) fn complex_fields(&self, word: Word) -> Option<(Word, Word)> {
        object_address(word, ObjectKind::Complex).map(read_pair)
    }

    /// An upper bound from the configured heap size. Allocation still checks availability.
    pub(crate) fn max_integer_bits(&self) -> u64 {
        (self
            .max_heap_bytes()
            .saturating_sub(size_of::<LispBignum>()) as u64)
            .saturating_mul(8)
    }

    pub(crate) fn allocate_integer(&self, value: &BigInt) -> Result<Word, OutOfMemory> {
        if let Some(word) = value.to_isize().and_then(Word::fixnum) {
            return Ok(word);
        }
        let digits = value.iter_u32_digits();
        let len = digits.len();
        let size = len
            .checked_mul(size_of::<u32>())
            .and_then(|bytes| size_of::<LispBignum>().checked_add(bytes))
            .and_then(|size| size.checked_next_multiple_of(8))
            .ok_or(OutOfMemory)?;
        let address = self.allocate(size)?;
        // SAFETY: The allocation holds the header and all the digits. No Lisp
        // reference or Rust-owned storage is embedded in this movable object.
        unsafe {
            address.to_mut_ptr::<LispBignum>().write(LispBignum {
                header: Header::new(ObjectKind::Bignum, size),
                len,
                negative: value.sign() == Sign::Minus,
            });
            let payload = (address + size_of::<LispBignum>()).to_mut_ptr::<u32>();
            for (index, digit) in digits.enumerate() {
                payload.add(index).write(digit);
            }
        }
        Ok(Word::from_object_address(address))
    }

    /// Allocate a number whose ratios are reduced and whose complex float
    /// components have already been coerced to the same format.
    pub(crate) fn allocate_number(&self, number: &Number) -> Result<Word, OutOfMemory> {
        match number {
            Number::Real(real) => self.allocate_real(real),
            Number::Complex(real, imaginary) if imaginary.is_exact_zero() => {
                self.allocate_real(real)
            }
            Number::Complex(real, imaginary) => {
                let real = self.root_slot(self.allocate_real(real)?);
                let imaginary = self.root_slot(self.allocate_real(imaginary)?);
                self.allocate_complex(|| (real.get(), imaginary.get()))
            }
        }
    }

    fn allocate_real(&self, real: &Real) -> Result<Word, OutOfMemory> {
        match real {
            Real::Integer(value) => self.allocate_integer(value),
            Real::Ratio(value) if value.is_integer() => self.allocate_integer(value.numer()),
            Real::Ratio(value) => {
                let numerator = self.root_slot(self.allocate_integer(value.numer())?);
                let denominator = self.root_slot(self.allocate_integer(value.denom())?);
                self.allocate_ratio(|| (numerator.get(), denominator.get()))
            }
            Real::Single(value) => {
                self.allocate_float(ObjectKind::SingleFloat, u64::from(value.to_bits()))
            }
            Real::Double(value) => self.allocate_float(ObjectKind::DoubleFloat, value.to_bits()),
        }
    }

    pub(crate) fn allocate_machine_float(&self, value: MachineReal) -> Result<Word, OutOfMemory> {
        match value {
            MachineReal::Single(value) => {
                self.allocate_float(ObjectKind::SingleFloat, u64::from(value.to_bits()))
            }
            MachineReal::Double(value) => {
                self.allocate_float(ObjectKind::DoubleFloat, value.to_bits())
            }
            MachineReal::Fixnum(_) => unreachable!("binary floating result is a float"),
        }
    }

    fn allocate_float(&self, kind: ObjectKind, bits: u64) -> Result<Word, OutOfMemory> {
        let size = size_of::<LispFloat>();
        let address = self.allocate(size)?;
        // SAFETY: a fresh allocation with space for a complete float.
        unsafe {
            address.to_mut_ptr::<LispFloat>().write(LispFloat {
                header: Header::new(kind, size),
                bits,
            });
        }
        Ok(Word::from_object_address(address))
    }

    /// Read the validated, rooted fraction components after allocation.
    pub(crate) fn allocate_ratio(
        &self,
        components: impl FnOnce() -> (Word, Word),
    ) -> Result<Word, OutOfMemory> {
        self.allocate_number_pair(ObjectKind::Ratio, components)
    }

    /// Read the validated, rooted complex components after allocation.
    pub(crate) fn allocate_complex(
        &self,
        components: impl FnOnce() -> (Word, Word),
    ) -> Result<Word, OutOfMemory> {
        self.allocate_number_pair(ObjectKind::Complex, components)
    }

    fn allocate_number_pair(
        &self,
        kind: ObjectKind,
        components: impl FnOnce() -> (Word, Word),
    ) -> Result<Word, OutOfMemory> {
        let size = size_of::<LispNumberPair>();
        let address = self.allocate(size)?;
        let (left, right) = components();
        // SAFETY: a fresh allocation with space for two traced fields, read
        // from roots after any forwarding done in allocate.
        unsafe {
            address
                .to_mut_ptr::<LispNumberPair>()
                .write(LispNumberPair {
                    header: Header::new(kind, size),
                    left: WordSlot::new(left),
                    right: WordSlot::new(right),
                });
        }
        Ok(Word::from_object_address(address))
    }
}

fn boxed(word: Word) -> Option<(Address, ObjectKind)> {
    let address = word.object_address()?;
    // SAFETY: the word refers to a live object, and the read cannot collect.
    Some((address, unsafe { (*address.to_ptr::<Header>()).kind }))
}

fn read_bignum(address: Address) -> BigInt {
    with_bignum_digits(address, |negative, digits| {
        let sign = if negative { Sign::Minus } else { Sign::Plus };
        BigInt::from_slice(sign, digits)
    })
}

fn with_bignum_digits<T>(address: Address, read: impl FnOnce(bool, &[u32]) -> T) -> T {
    // SAFETY: The kind was checked. The inline digits stay live and
    // stationary while the caller reads them without collecting.
    unsafe {
        let value = &*address.to_ptr::<LispBignum>();
        let digits = std::slice::from_raw_parts(
            (address + size_of::<LispBignum>()).to_ptr::<u32>(),
            value.len,
        );
        read(value.negative, digits)
    }
}

fn read_float(address: Address) -> u64 {
    // SAFETY: the kind was checked and the read cannot collect.
    unsafe { (*address.to_ptr::<LispFloat>()).bits }
}

fn read_pair(address: Address) -> (Word, Word) {
    // SAFETY: the kind was checked and reading the slots cannot collect.
    let pair = unsafe { &*address.to_ptr::<LispNumberPair>() };
    (pair.left.get(), pair.right.get())
}

pub(super) fn scan_pair<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visit_offsets(
        base,
        visitor,
        &[
            offset_of!(LispNumberPair, left),
            offset_of!(LispNumberPair, right),
        ],
    );
}

#[cfg(test)]
pub(crate) fn check_number_storage(heap: &Heap) {
    let denominator = BigInt::from(1u8) << 100usize;
    let numerator = -(&denominator * 5u8 + 1u8);
    let fraction = Real::Ratio(BigRational::new(numerator.clone(), denominator.clone()));
    let ratio = heap.root_slot(
        heap.allocate_number(&Number::Real(fraction.clone()))
            .unwrap(),
    );
    let complex = heap.root_slot(
        heap.allocate_complex(|| (ratio.get(), ratio.get()))
            .unwrap(),
    );
    let old_complex = complex.get();
    let old_ratio = ratio.get();
    let old_fields = heap.ratio_fields(old_ratio).unwrap();
    drop(ratio); // Both the fraction and its bignums are now reachable only through the complex.

    heap.collect();
    assert_ne!(complex.get(), old_complex);
    let (real, imaginary) = heap.complex_fields(complex.get()).unwrap();
    assert_eq!(real, imaginary);
    assert_ne!(real, old_ratio);
    let fields = heap.ratio_fields(real).unwrap();
    assert_ne!(fields.0, old_fields.0);
    assert_ne!(fields.1, old_fields.1);
    assert_eq!(heap.integer(fields.0), Some(numerator));
    assert_eq!(heap.integer(fields.1), Some(denominator));
    assert_eq!(
        heap.number(complex.get()),
        Some(Number::Complex(fraction.clone(), fraction))
    );

    let seven = Real::Ratio(BigRational::from_integer(7.into()));
    assert_eq!(
        heap.allocate_number(&Number::Real(seven.clone()))
            .unwrap()
            .as_fixnum(),
        Some(7)
    );
    assert_eq!(
        heap.allocate_number(&Number::Complex(seven, Real::Integer(0.into())))
            .unwrap()
            .as_fixnum(),
        Some(7)
    );
    let complex = heap.root_slot(
        heap.allocate_number(&Number::Complex(Real::Single(7.0), Real::Single(-0.0)))
            .unwrap(),
    );
    heap.collect();
    let Some(Number::Complex(Real::Single(real), Real::Single(imaginary))) =
        heap.number(complex.get())
    else {
        panic!("complex single float should retain both components");
    };
    assert_eq!(real, 7.0);
    assert_eq!(imaginary.to_bits(), (-0.0f32).to_bits());
}
