use num_bigint::{BigInt, BigUint, Sign};
use num_traits::ToPrimitive;

use crate::condition::Fault;
use crate::heap::ArrayElement;
use crate::{EvalError, Root, Runtime};

pub(super) fn integer<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let invalid = || EvalError::new(at, "invalid bit field backing, offset or width");
    let [input, start, width, little, signed] = args else {
        return Err(invalid().into());
    };
    let natural = |argument: &Root<'_>| match argument.as_fixnum() {
        Some(value) => usize::try_from(value).ok(),
        None => runtime.integer(argument).and_then(|value| value.to_usize()),
    };
    let start = natural(start).ok_or_else(invalid)?;
    let width = natural(width)
        .filter(|&width| width > 0)
        .ok_or_else(invalid)?;
    let end = start.checked_add(width).ok_or_else(invalid)?;
    let little = !little.is_nil();
    let signed = !signed.is_nil();
    let input = input.slot.get();
    let info = runtime
        .heap
        .array_info(input)
        .filter(|info| info.rank == 1)
        .ok_or_else(invalid)?;
    let (packed, padding) = match info.element {
        ArrayElement::Octet if !little || (start.is_multiple_of(8) && width.is_multiple_of(8)) => {
            let low = start / 8;
            let mut bytes = runtime
                .heap
                .octet_array_range(input, low, end.div_ceil(8) - low)
                .ok_or_else(invalid)?;
            bytes[0] &= u8::MAX >> (start % 8);
            (bytes, (8 - end % 8) % 8)
        }
        ArrayElement::Bit if end <= info.size && (!little || width.is_multiple_of(8)) => {
            let mut bytes = vec![0; width.div_ceil(8)];
            for offset in 0..width {
                let value = runtime
                    .heap
                    .array_value(input, start + offset)
                    .ok_or_else(invalid)?;
                let bit = value
                    .expect_word()
                    .as_fixnum()
                    .expect("bit is an immediate integer");
                bytes[offset / 8] |= (bit as u8) << (7 - offset % 8);
            }
            (bytes, (8 - width % 8) % 8)
        }
        _ => return Err(invalid().into()),
    };
    let result = if width <= 64 {
        let magnitude = if little {
            packed.iter().rev().fold(0u128, append_byte)
        } else {
            packed.iter().fold(0u128, append_byte) >> padding
        };
        let value = if signed && magnitude >> (width - 1) != 0 {
            magnitude as i128 - (1i128 << width)
        } else {
            magnitude as i128
        };
        runtime.integer_from(value)
    } else {
        let magnitude = if little {
            BigUint::from_bytes_le(&packed)
        } else {
            BigUint::from_bytes_be(&packed) >> padding
        };
        let negative = signed && magnitude.bits() == width as u64;
        let mut value = BigInt::from_biguint(Sign::Plus, magnitude);
        if negative {
            value -= BigInt::from(1) << width;
        }
        runtime.make_integer(&value)
    };
    Ok(vec![
        result.map_err(|cause| EvalError::out_of_memory(at, cause))?,
    ])
}

fn append_byte(value: u128, &octet: &u8) -> u128 {
    (value << 8) | u128::from(octet)
}

#[cfg(test)]
mod tests {
    use crate::Runtime;

    #[test]
    fn bit_layout_reads_unaligned_signed_and_wide_fields() {
        let runtime = Runtime::start().unwrap();
        runtime.load_str(r#"
            (let ((packet (zk-bits:octets #xd2 3 0 65 66 67 #x7e)))
              (assert (equal '(2 (65 66 67))
                       (zk:match packet ((zk-bits:bits (u4be #xd) (u4be kind) (u16le count)
                                                      (:bytes count payload) (u8be #x7e))
                                         (list kind (coerce payload 'list))) (_ nil)))))
            (let ((view (zk:match (zk-bits:octets #xab #xcd) ((zk-bits:bits (u4be #xa) (:rest-bits tail)) tail) (_ nil))))
              (assert (= #xbcd (zk:match view ((zk-bits:bits (u12be value)) value) (_ 0))))
              (assert (= -1075 (zk:match view ((zk-bits:bits (i12be value)) value) (_ 0)))))
            (let ((bytes (make-array 10 :element-type '(unsigned-byte 8) :initial-element 255)))
              (assert (= -1 (zk:match bytes ((zk-bits:bits (i80le value)) value) (_ 0))))
              (assert (= (1- (ash 1 80)) (zk:match bytes ((zk-bits:bits (u80be value)) value) (_ 0)))))
        "#).unwrap();
    }

    #[test]
    fn rust_octet_buffers_are_rooted_and_use_active_vector_length() {
        let runtime = Runtime::start().unwrap();
        let octets = runtime.make_octets(&[0, 128, 255]).unwrap();
        runtime.collect();
        assert_eq!(runtime.octets(&octets).as_deref(), Some(&[0, 128, 255][..]));
        let view = runtime.eval("(make-array 3 :element-type '(unsigned-byte 8) :fill-pointer 1 :initial-element 42)").unwrap();
        assert_eq!(runtime.octets(&view[0]).as_deref(), Some(&[42][..]));
        let general = runtime.eval("#(1 2 3)").unwrap();
        assert!(runtime.octets(&general[0]).is_none());
    }
}
