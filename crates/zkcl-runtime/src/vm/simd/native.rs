//! Checked entry points for the runtime's Fearless SIMD leaves.

use zkcl_bytecode::SimdPack;

#[derive(Clone, Copy)]
pub(super) enum Arithmetic {
    Add,
    Sub,
    Mul,
}

pub(super) type F32ScaledAdd =
    unsafe fn(out: *mut f32, a: *const f32, b: *const f32, scale: u32, len: usize) -> usize;

enum PackBackend {
    Baseline,
    #[cfg(target_arch = "x86_64")]
    Avx2(fearless_simd::x86::Avx2),
    #[cfg(target_arch = "x86_64")]
    Avx512(fearless_simd::x86::Avx512),
}

fn pack_backend(pack: &SimdPack, require_fma: bool) -> PackBackend {
    #[cfg(target_arch = "x86_64")]
    {
        let width = pack.shape().byte_len();
        if width > 16 || require_fma {
            let level = fearless_simd::Level::new();
            if width == 64
                && let Some(avx512) = level.as_avx512()
            {
                return PackBackend::Avx512(avx512);
            }
            if let Some(avx2) = level.as_avx2() {
                return PackBackend::Avx2(avx2);
            }
        }
    }
    let _ = (pack, require_fma);
    PackBackend::Baseline
}

type PackArithmetic = unsafe fn(PackBackend, Arithmetic, &SimdPack, &SimdPack) -> [u8; 64];
type PackFma = unsafe fn(PackBackend, &SimdPack, &SimdPack, &SimdPack) -> [u8; 64];

struct Kernels {
    fp_controls_match: fn() -> bool,
    packed_float: PackArithmetic,
    packed_integer: PackArithmetic,
    packed_fma: PackFma,
    u8x16_eq: unsafe fn([u8; 16], [u8; 16]) -> u16,
    f32_scaled_add: F32ScaledAdd,
}

fn kernels() -> Option<&'static Kernels> {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    if arch::available() {
        return Some(&Kernels {
            fp_controls_match: arch::fp_controls_match,
            packed_float: leaves::packed_float,
            packed_integer: leaves::packed_integer,
            packed_fma: leaves::packed_fma,
            u8x16_eq: leaves::u8x16_eq,
            f32_scaled_add: leaves::f32_scaled_add,
        });
    }
    None
}

fn floating_kernels() -> Option<&'static Kernels> {
    // FP controls are thread-local and can change during a host call.
    kernels().filter(|kernels| (kernels.fp_controls_match)())
}

pub(super) fn packed_float(op: Arithmetic, a: &SimdPack, b: &SimdPack) -> Option<[u8; 64]> {
    // Packs serialize each lane in little endian. Direct vector bitcasts use native endian.
    if cfg!(target_endian = "big") {
        return None;
    }
    let kernels = kernels()?;
    let backend = pack_backend(a, false);
    if !(kernels.fp_controls_match)() {
        return None;
    }
    // SAFETY: Selection checked the ISA and the current thread's FP controls.
    Some(unsafe { (kernels.packed_float)(backend, op, a, b) })
}

pub(super) fn packed_integer(op: Arithmetic, a: &SimdPack, b: &SimdPack) -> Option<[u8; 64]> {
    if cfg!(target_endian = "big") {
        return None;
    }
    let kernels = kernels()?;
    let backend = pack_backend(a, false);
    // SAFETY: Selection checked the ISA. This leaf only emits integer arithmetic.
    Some(unsafe { (kernels.packed_integer)(backend, op, a, b) })
}

pub(super) fn packed_fma(a: &SimdPack, b: &SimdPack, c: &SimdPack) -> Option<[u8; 64]> {
    if cfg!(target_endian = "big") {
        return None;
    }
    let kernels = kernels()?;
    let backend = pack_backend(a, true);
    #[cfg(target_arch = "x86_64")]
    if matches!(backend, PackBackend::Baseline) {
        return None;
    }
    if !(kernels.fp_controls_match)() {
        return None;
    }
    // SAFETY: Selection checked hardware FMA support and the current thread's FP controls.
    Some(unsafe { (kernels.packed_fma)(backend, a, b, c) })
}

pub(super) fn u8x16_eq(a: [u8; 16], b: [u8; 16]) -> u16 {
    if let Some(kernels) = kernels() {
        // SAFETY: Selection checked the ISA. Integer SIMD is independent of FP controls.
        return unsafe { (kernels.u8x16_eq)(a, b) };
    }
    a.into_iter()
        .zip(b)
        .enumerate()
        .fold(0, |mask, (i, (a, b))| mask | (u16::from(a == b) << i))
}

/// Select the leaf for `out[i] = a[i] * scale + b[i]`, with separate rounding.
/// It returns the number of elements written. The caller can replay from there
/// in scalar order, starting at a failed group or the tail.
///
/// The returned leaf must be called on this thread before its FP controls can
/// change. Its pointers must give element-aligned access to `len` elements for
/// the entire call, with initialized inputs and writable output. Each input
/// range must be disjoint from or exactly equal to the output range. The heap
/// borrow must keep storage stable and prevent competing access to the output
/// or modification of the inputs by another caller.
pub(super) fn f32_scaled_add() -> Option<F32ScaledAdd> {
    Some(floating_kernels()?.f32_scaled_add)
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod leaves {
    use std::ops::{Add, Mul, Sub};

    #[cfg(target_arch = "aarch64")]
    use fearless_simd::aarch64::Neon as Token;
    #[cfg(target_arch = "x86_64")]
    use fearless_simd::x86::Sse2 as Token;
    use fearless_simd::{
        f32x4, f32x8, f32x16, f64x2, f64x4, f64x8, prelude::*, u8x16, u8x32, u8x64, u16x8, u16x16,
        u16x32, u32x4, u32x8, u32x16, u64x2, u64x4, u64x8,
    };

    use zkcl_bytecode::LaneKind;

    use super::{Arithmetic, PackBackend, SimdPack};

    // Generate disjoint arithmetic bodies so the integer entry can run in any FP mode.
    macro_rules! packed_entries {
        ($baseline:ident, $avx2:ident, $avx512:ident, $float:literal) => {
            #[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
            #[cfg_attr(target_arch = "x86_64", target_feature(enable = "fxsr,sse,sse2"))]
            #[inline(never)]
            pub(super) unsafe fn $baseline(
                backend: PackBackend,
                op: Arithmetic,
                a: &SimdPack,
                b: &SimdPack,
            ) -> [u8; 64] {
                match backend {
                    PackBackend::Baseline => {
                        packed_with::<_, $float>(Token::assume_supported(), op, a, b)
                    }
                    #[cfg(target_arch = "x86_64")]
                    PackBackend::Avx2(token) => $avx2(token, op, a, b),
                    #[cfg(target_arch = "x86_64")]
                    PackBackend::Avx512(token) => $avx512(token, op, a, b),
                }
            }

            fearless_simd::kernel!(
                #[inline]
                fn $avx2(token: Avx2, op: Arithmetic, a: &SimdPack, b: &SimdPack) -> [u8; 64] {
                    packed_with::<_, $float>(token, op, a, b)
                }
            );

            fearless_simd::kernel!(
                #[inline]
                fn $avx512(token: Avx512, op: Arithmetic, a: &SimdPack, b: &SimdPack) -> [u8; 64] {
                    packed_with::<_, $float>(token, op, a, b)
                }
            );
        };
    }
    packed_entries!(packed_float, float_avx2, float_avx512, true);
    packed_entries!(packed_integer, integer_avx2, integer_avx512, false);

    #[inline(always)]
    fn packed_with<S: Simd, const FLOAT: bool>(
        token: S,
        op: Arithmetic,
        a: &SimdPack,
        b: &SimdPack,
    ) -> [u8; 64] {
        let kind = a.shape().kind();
        let a = a.as_bytes();
        let b = b.as_bytes();
        let mut out = [0; 64];

        // The logical width is independent of the register width of S. For wrapping
        // arithmetic, the low lane bits are identical for signed and unsigned inputs.
        macro_rules! width {
            ($n:literal, $u8:ident, $u16:ident, $u32:ident, $u64:ident, $f32:ident, $f64:ident) => {{
                let a = $u8::simd_from(token, *a.as_array::<$n>().expect("pack width"));
                let b = $u8::simd_from(token, *b.as_array::<$n>().expect("matching pack width"));
                let result = if FLOAT {
                    match kind {
                        LaneKind::F32 => apply::<$f32<S>, _>(op, a, b),
                        LaneKind::F64 => apply::<$f64<S>, _>(op, a, b),
                        _ => unreachable!("validated float pack"),
                    }
                } else {
                    match kind {
                        LaneKind::I8 | LaneKind::U8 => apply::<$u8<S>, _>(op, a, b),
                        LaneKind::I16 | LaneKind::U16 => apply::<$u16<S>, _>(op, a, b),
                        LaneKind::I32 | LaneKind::U32 => apply::<$u32<S>, _>(op, a, b),
                        LaneKind::I64 | LaneKind::U64 => apply::<$u64<S>, _>(op, a, b),
                        _ => unreachable!("validated integer pack"),
                    }
                };
                let result: [u8; $n] = result.into();
                out[..$n].copy_from_slice(&result);
            }};
        }

        match a.len() {
            16 => width!(16, u8x16, u16x8, u32x4, u64x2, f32x4, f64x2),
            32 => width!(32, u8x32, u16x16, u32x8, u64x4, f32x8, f64x4),
            64 => width!(64, u8x64, u16x32, u32x16, u64x8, f32x16, f64x8),
            _ => unreachable!("pack width"),
        }
        out
    }

    #[inline(always)]
    fn apply<V, B>(op: Arithmetic, a: B, b: B) -> B
    where
        V: Bytes<Bytes = B> + Add<Output = V> + Sub<Output = V> + Mul<Output = V>,
        B: Bytes<Bytes = B>,
    {
        let a = a.bitcast::<V>();
        let b = b.bitcast::<V>();
        let result = match op {
            Arithmetic::Add => a + b,
            Arithmetic::Sub => a - b,
            Arithmetic::Mul => a * b,
        };
        result.bitcast()
    }

    #[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
    #[inline(never)]
    pub(super) unsafe fn packed_fma(
        backend: PackBackend,
        a: &SimdPack,
        b: &SimdPack,
        c: &SimdPack,
    ) -> [u8; 64] {
        match backend {
            #[cfg(target_arch = "aarch64")]
            PackBackend::Baseline => fma_with(Token::assume_supported(), a, b, c),
            #[cfg(target_arch = "x86_64")]
            PackBackend::Baseline => unreachable!("hardware FMA required"),
            #[cfg(target_arch = "x86_64")]
            PackBackend::Avx2(token) => fma_avx2(token, a, b, c),
            #[cfg(target_arch = "x86_64")]
            PackBackend::Avx512(token) => fma_avx512(token, a, b, c),
        }
    }

    fearless_simd::kernel!(
        #[inline]
        fn fma_avx2(token: Avx2, a: &SimdPack, b: &SimdPack, c: &SimdPack) -> [u8; 64] {
            fma_with(token, a, b, c)
        }
    );

    fearless_simd::kernel!(
        #[inline]
        fn fma_avx512(token: Avx512, a: &SimdPack, b: &SimdPack, c: &SimdPack) -> [u8; 64] {
            fma_with(token, a, b, c)
        }
    );

    #[inline(always)]
    fn fma_with<S: Simd>(token: S, a: &SimdPack, b: &SimdPack, c: &SimdPack) -> [u8; 64] {
        let kind = a.shape().kind();
        let a = a.as_bytes();
        let b = b.as_bytes();
        let c = c.as_bytes();
        let mut out = [0; 64];

        macro_rules! fused {
            ($a:expr, $b:expr, $c:expr, $float:ident, $bytes:ident) => {{
                let a = $a.bitcast::<$float<S>>();
                let b = $b.bitcast::<$float<S>>();
                let c = $c.bitcast::<$float<S>>();
                a.mul_add_precise(b, c).bitcast::<$bytes<S>>()
            }};
        }
        macro_rules! width {
            ($n:literal, $bytes:ident, $f32:ident, $f64:ident) => {{
                let a = $bytes::simd_from(token, *a.as_array::<$n>().expect("pack width"));
                let b = $bytes::simd_from(token, *b.as_array::<$n>().expect("matching pack width"));
                let c = $bytes::simd_from(token, *c.as_array::<$n>().expect("matching pack width"));
                let result = match kind {
                    LaneKind::F32 => fused!(a, b, c, $f32, $bytes),
                    LaneKind::F64 => fused!(a, b, c, $f64, $bytes),
                    _ => unreachable!("validated float pack"),
                };
                let result: [u8; $n] = result.into();
                out[..$n].copy_from_slice(&result);
            }};
        }

        match a.len() {
            16 => width!(16, u8x16, f32x4, f64x2),
            32 => width!(32, u8x32, f32x8, f64x4),
            64 => width!(64, u8x64, f32x16, f64x8),
            _ => unreachable!("pack width"),
        }
        out
    }

    #[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
    #[cfg_attr(target_arch = "x86_64", target_feature(enable = "fxsr,sse,sse2"))]
    #[inline(never)]
    pub(super) unsafe fn u8x16_eq(a: [u8; 16], b: [u8; 16]) -> u16 {
        let token = Token::assume_supported();
        u8x16::simd_from(token, a).simd_eq(b).to_bitmask() as u16
    }

    #[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
    #[cfg_attr(target_arch = "x86_64", target_feature(enable = "fxsr,sse,sse2"))]
    #[inline(never)]
    pub(super) unsafe fn f32_scaled_add(
        out: *mut f32,
        a: *const f32,
        b: *const f32,
        scale: u32,
        len: usize,
    ) -> usize {
        let token = Token::assume_supported();
        let scale = f32x4::splat(token, f32::from_bits(scale));
        let exponent = u32x4::splat(token, 0x7f80_0000);
        let mut i = 0;
        while len - i >= 4 {
            // SAFETY: The checked spans each hold at least four aligned elements here.
            // Read by value so both inputs may exactly alias the output.
            let a = f32x4::simd_from(token, unsafe { a.add(i).cast::<[f32; 4]>().read() });
            let b = f32x4::simd_from(token, unsafe { b.add(i).cast::<[f32; 4]>().read() });
            let product = a * scale;
            let sum = product + b;
            let product_exp = product.bitcast::<u32x4<_>>() & exponent;
            let sum_exp = sum.bitcast::<u32x4<_>>() & exponent;
            let failed = product_exp.simd_eq(exponent) | sum_exp.simd_eq(exponent);
            if failed.any_true() {
                break;
            }
            // SAFETY: The output group is in bounds and both inputs are already loaded.
            unsafe { out.add(i).cast::<[f32; 4]>().write(sum.into()) };
            i += 4;
        }
        i
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    use std::arch::asm;

    pub(super) fn available() -> bool {
        std::arch::is_aarch64_feature_detected!("neon")
    }

    pub(super) fn fp_controls_match() -> bool {
        const ROUNDING: u64 = 0b11 << 22;
        const FLUSH_TO_ZERO: u64 = 1 << 24;
        const TRAP_ENABLES: u64 = (1 << 15) | (0b1_1111 << 8);
        // FEAT_AFP adds alternate handling and input denormal flushing.
        const ALTERNATE_FP: u64 = 0b11;

        let fpcr: u64;
        // SAFETY: NEON selection has established FP access. This reads only FPCR.
        unsafe {
            asm!("mrs {fpcr}, fpcr", fpcr = out(reg) fpcr, options(nomem, nostack, preserves_flags));
        }
        fpcr & (ROUNDING | FLUSH_TO_ZERO | TRAP_ENABLES | ALTERNATE_FP) == 0
    }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use std::arch::asm;
    use std::mem::MaybeUninit;

    pub(super) fn available() -> bool {
        // The Fearless SSE2 token also requires SSE and FXSAVE/FXRSTOR.
        std::arch::is_x86_feature_detected!("sse2")
            && std::arch::is_x86_feature_detected!("sse")
            && std::arch::is_x86_feature_detected!("fxsr")
    }

    pub(super) fn fp_controls_match() -> bool {
        const ROUNDING: u32 = 0b11 << 13;
        const FLUSH_TO_ZERO: u32 = 1 << 15;
        const DENORMALS_ARE_ZERO: u32 = 1 << 6;
        const TRAP_MASKS: u32 = 0b11_1111 << 7;
        const CONTROLS: u32 = ROUNDING | FLUSH_TO_ZERO | DENORMALS_ARE_ZERO | TRAP_MASKS;

        let mut mxcsr = MaybeUninit::<u32>::uninit();
        // SAFETY: SSE2 selection established SSE access and STMXCSR writes one u32.
        let mxcsr = unsafe {
            asm!("stmxcsr [{mxcsr}]", mxcsr = in(reg) mxcsr.as_mut_ptr(), options(nostack, preserves_flags));
            mxcsr.assume_init()
        };
        mxcsr & CONTROLS == TRAP_MASKS
    }
}
