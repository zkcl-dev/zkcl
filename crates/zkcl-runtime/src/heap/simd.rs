use zkcl_bytecode::{LaneKind, PackShape, SimdMask, SimdPack};

use super::{Address, Header, Heap, HeapObject, ObjectKind, OutOfMemory, Word, object_address};

const I32X4: PackShape = PackShape::new(LaneKind::I32, 4).unwrap();

#[repr(C)]
struct PackHeader {
    header: Header,
    shape: PackShape,
}

// SAFETY: `PackHeader` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::SimdPack`.
unsafe impl HeapObject for PackHeader {
    const KIND: ObjectKind = ObjectKind::SimdPack;
}

#[repr(C, align(8))]
struct MaskObject {
    header: Header,
    // Store the bits as bytes so its alignment is independent of target u64 alignment.
    bits: [u8; 8],
    lanes: u8,
}

// SAFETY: `MaskObject` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::SimdMask`.
unsafe impl HeapObject for MaskObject {
    const KIND: ObjectKind = ObjectKind::SimdMask;
}

impl Heap {
    pub(crate) fn allocate_simd_pack(&self, pack: SimdPack) -> Result<Word, OutOfMemory> {
        let shape = pack.shape();
        let length = shape.byte_len();
        let size = size_of::<PackHeader>() + length;
        let base = self.allocate(size)?;
        // SAFETY: The allocation holds the aligned header and exactly `length` payload bytes.
        // Both parts contain only inline data; moving the allocation preserves the pack.
        unsafe {
            base.to_mut_ptr::<PackHeader>().write(PackHeader {
                header: Header::new(ObjectKind::SimdPack, size),
                shape,
            });
            pack.as_bytes()
                .as_ptr()
                .copy_to_nonoverlapping((base + size_of::<PackHeader>()).to_mut_ptr(), length);
        }
        Ok(Word::from_object_address(base))
    }

    pub(crate) fn simd_pack_shape(&self, word: Word) -> Option<PackShape> {
        pack_payload(word).map(|(shape, _)| shape)
    }

    pub(crate) fn simd_pack(&self, word: Word) -> Option<SimdPack> {
        let (shape, payload) = pack_payload(word)?;
        // SAFETY: `shape` describes the entire trailing allocation. The borrow ends before GC.
        let bytes = unsafe { std::slice::from_raw_parts(payload.to_ptr(), shape.byte_len()) };
        Some(SimdPack::from_bytes(shape, bytes).expect("valid immutable pack payload"))
    }

    pub(crate) fn simd_i32x4_lanes(&self, word: Word) -> Option<[u32; 4]> {
        let (shape, payload) = pack_payload(word)?;
        if shape != I32X4 {
            return None;
        }
        // SAFETY: This shape guarantees an initialized sixteen-byte allocation and every
        // bit pattern represents an integer. Reading it cannot collect.
        let lanes = unsafe { payload.to_ptr::<[u32; 4]>().read_unaligned() };
        Some(lanes.map(u32::from_le))
    }

    pub(crate) fn allocate_simd_mask(&self, mask: SimdMask) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| MaskObject {
            header,
            bits: mask.bits().to_le_bytes(),
            lanes: mask.lanes() as u8,
        })
    }

    pub(crate) fn simd_mask(&self, word: Word) -> Option<SimdMask> {
        let object = self.object_ref::<MaskObject>(word)?;
        Some(
            SimdMask::new(usize::from(object.lanes), u64::from_le_bytes(object.bits))
                .expect("valid immutable mask payload"),
        )
    }
}

fn pack_payload(word: Word) -> Option<(PackShape, Address)> {
    let base = object_address(word, ObjectKind::SimdPack)?;
    // SAFETY: Kind checked. The header is immutable, and the access cannot collect.
    let shape = unsafe { (*base.to_ptr::<PackHeader>()).shape };
    Some((shape, base + size_of::<PackHeader>()))
}
