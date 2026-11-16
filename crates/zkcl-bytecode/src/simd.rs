use serde::{Deserialize, Serialize};

/// Element kind of a SIMD pack. Signed and unsigned integers have distinct shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LaneKind {
    F32,
    F64,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
}

impl LaneKind {
    pub const ALL: [Self; 10] = [
        Self::F32,
        Self::F64,
        Self::I8,
        Self::U8,
        Self::I16,
        Self::U16,
        Self::I32,
        Self::U32,
        Self::I64,
        Self::U64,
    ];

    pub const fn byte_len(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }

    pub const fn keyword(self) -> &'static str {
        match self {
            Self::F32 => "F32",
            Self::F64 => "F64",
            Self::I8 => "I8",
            Self::U8 => "U8",
            Self::I16 => "I16",
            Self::U16 => "U16",
            Self::I32 => "I32",
            Self::U32 => "U32",
            Self::I64 => "I64",
            Self::U64 => "U64",
        }
    }

    /// Test the zero-extended representation of one lane.
    pub const fn valid_lane_bits(self, bits: u64) -> bool {
        match self {
            Self::F32 => bits <= u32::MAX as u64 && bits & 0x7f80_0000 != 0x7f80_0000,
            Self::F64 => bits & 0x7ff0_0000_0000_0000 != 0x7ff0_0000_0000_0000,
            _ => bits <= u64::MAX >> (64 - 8 * self.byte_len()),
        }
    }
}

/// The concrete element kind and logical width of a 128-, 256- or 512-bit pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PackShape {
    kind: LaneKind,
    byte_len: u8,
}

impl PackShape {
    const BYTE_LENGTHS: [u8; 3] = [16, 32, 64];

    pub const ALL: [Self; LaneKind::ALL.len() * Self::BYTE_LENGTHS.len()] = {
        let mut shapes = [Self {
            kind: LaneKind::F32,
            byte_len: 16,
        }; LaneKind::ALL.len() * Self::BYTE_LENGTHS.len()];
        let mut index = 0;
        while index < shapes.len() {
            shapes[index] = Self {
                kind: LaneKind::ALL[index / Self::BYTE_LENGTHS.len()],
                byte_len: Self::BYTE_LENGTHS[index % Self::BYTE_LENGTHS.len()],
            };
            index += 1;
        }
        shapes
    };

    pub const fn new(kind: LaneKind, lanes: usize) -> Option<Self> {
        match lanes.checked_mul(kind.byte_len()) {
            Some(byte_len @ (16 | 32 | 64)) => Some(Self {
                kind,
                byte_len: byte_len as u8,
            }),
            _ => None,
        }
    }

    pub const fn kind(self) -> LaneKind {
        self.kind
    }

    pub const fn lanes(self) -> usize {
        self.byte_len() / self.kind.byte_len()
    }

    pub const fn byte_len(self) -> usize {
        self.byte_len as usize
    }

    /// The unqualified name of this concrete type in the `ZK-SIMD` package.
    pub fn name(self) -> String {
        format!("{}X{}", self.kind.keyword(), self.lanes())
    }
}

/// Packed lane bits in ascending lane order, with each lane in little endian.
/// Float lanes are finite, including subnormals and signed zeroes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EncodedPack", into = "EncodedPack")]
pub struct SimdPack {
    shape: PackShape,
    bits: [u8; 64],
}

impl SimdPack {
    pub fn from_bytes(shape: PackShape, bytes: &[u8]) -> Option<Self> {
        if bytes.len() != shape.byte_len() {
            return None;
        }
        let mut pack = Self {
            shape,
            bits: [0; 64],
        };
        pack.bits[..bytes.len()].copy_from_slice(bytes);
        if matches!(shape.kind(), LaneKind::F32 | LaneKind::F64)
            && (0..shape.lanes()).any(|index| !shape.kind().valid_lane_bits(pack.lane(index)))
        {
            return None;
        }
        Some(pack)
    }

    /// Construct a pack from zero-extended lane bit patterns.
    pub fn from_lanes(shape: PackShape, lanes: &[u64]) -> Option<Self> {
        if lanes.len() != shape.lanes() {
            return None;
        }
        let kind = shape.kind();
        let width = kind.byte_len();
        let mut bits = [0; 64];
        for (&lane, bytes) in lanes.iter().zip(bits.chunks_exact_mut(width)) {
            if !kind.valid_lane_bits(lane) {
                return None;
            }
            bytes.copy_from_slice(&lane.to_le_bytes()[..width]);
        }
        Some(Self { shape, bits })
    }

    pub const fn shape(&self) -> PackShape {
        self.shape
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bits[..self.shape.byte_len()]
    }

    /// Return the bits of a lane, zero-extended to 64 bits.
    ///
    /// Panics if `index` is outside the pack.
    pub fn lane(&self, index: usize) -> u64 {
        assert!(index < self.shape.lanes());
        let width = self.shape.kind().byte_len();
        let start = index * width;
        let mut bits = [0; 8];
        bits[..width].copy_from_slice(&self.bits[start..start + width]);
        u64::from_le_bytes(bits)
    }
}

// The width controls how many bytes Serde reads, with no variable-length buffer.
#[derive(Serialize, Deserialize)]
struct EncodedPack {
    kind: LaneKind,
    bits: EncodedPackBits,
}

#[derive(Serialize, Deserialize)]
enum EncodedPackBits {
    Bits128([u8; 16]),
    Bits256([u8; 32]),
    Bits512([[u8; 32]; 2]),
}

impl From<SimdPack> for EncodedPack {
    fn from(pack: SimdPack) -> Self {
        let bytes = pack.as_bytes();
        let bits = match bytes.len() {
            16 => EncodedPackBits::Bits128(bytes.try_into().expect("128-bit pack")),
            32 => EncodedPackBits::Bits256(bytes.try_into().expect("256-bit pack")),
            64 => {
                let (lo, hi) = bytes.split_at(32);
                EncodedPackBits::Bits512([
                    lo.try_into().expect("lower 256 bits"),
                    hi.try_into().expect("upper 256 bits"),
                ])
            }
            _ => unreachable!("pack width"),
        };
        Self {
            kind: pack.shape().kind(),
            bits,
        }
    }
}

impl TryFrom<EncodedPack> for SimdPack {
    type Error = &'static str;

    fn try_from(pack: EncodedPack) -> Result<Self, Self::Error> {
        let bytes: &[u8] = match &pack.bits {
            EncodedPackBits::Bits128(bytes) => bytes,
            EncodedPackBits::Bits256(bytes) => bytes,
            EncodedPackBits::Bits512(bytes) => bytes.as_flattened(),
        };
        let shape = PackShape {
            kind: pack.kind,
            byte_len: bytes.len() as u8,
        };
        Self::from_bytes(shape, bytes).ok_or("SIMD pack float lanes must be finite")
    }
}

/// A boolean for each lane. Bit zero belongs to lane zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EncodedMask", into = "EncodedMask")]
pub struct SimdMask {
    lanes: u8,
    bits: u64,
}

impl SimdMask {
    pub const LANE_COUNTS: [usize; 6] = [2, 4, 8, 16, 32, 64];

    pub fn new(lanes: usize, bits: u64) -> Option<Self> {
        (bits & !Self::allowed_bits(lanes)? == 0).then_some(Self {
            lanes: lanes as u8,
            bits,
        })
    }

    pub const fn lanes(self) -> usize {
        self.lanes as usize
    }

    pub const fn bits(self) -> u64 {
        self.bits
    }

    pub fn allowed_bits(lanes: usize) -> Option<u64> {
        Self::LANE_COUNTS
            .contains(&lanes)
            .then(|| u64::MAX >> (64 - lanes))
    }
}

#[derive(Serialize, Deserialize)]
struct EncodedMask {
    lanes: u8,
    bits: u64,
}

impl From<SimdMask> for EncodedMask {
    fn from(mask: SimdMask) -> Self {
        Self {
            lanes: mask.lanes,
            bits: mask.bits,
        }
    }
}

impl TryFrom<EncodedMask> for SimdMask {
    type Error = &'static str;

    fn try_from(mask: EncodedMask) -> Result<Self, Self::Error> {
        Self::new(mask.lanes as usize, mask.bits).ok_or("invalid SIMD mask lane count or bits")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompiledFile, LiteralNode};

    #[test]
    fn simd_literals_round_trip_all_shapes() {
        let mut nodes = Vec::new();
        for shape in PackShape::ALL {
            let mut bytes = std::array::from_fn::<_, 64, _>(|index| index as u8);
            if matches!(shape.kind(), LaneKind::F32 | LaneKind::F64) {
                let width = shape.kind().byte_len();
                bytes[..width].fill(0);
                bytes[width - 1] = 0x80;
            }
            let bytes = &bytes[..shape.byte_len()];
            let pack = SimdPack::from_bytes(shape, bytes).unwrap();
            let lanes: Vec<_> = (0..shape.lanes()).map(|index| pack.lane(index)).collect();
            assert_eq!(SimdPack::from_lanes(shape, &lanes), Some(pack));
            let encoded = postcard::to_allocvec(&pack).unwrap();
            // One kind tag and one width tag precede the fixed raw byte block.
            assert_eq!(&encoded[2..], bytes);
            nodes.push(LiteralNode::SimdPack(pack));
        }
        for lanes in SimdMask::LANE_COUNTS {
            let mask = SimdMask::new(lanes, SimdMask::allowed_bits(lanes).unwrap()).unwrap();
            nodes.push(LiteralNode::SimdMask(mask));
        }
        let file = CompiledFile {
            target: zkcl_target::CompilationTarget::native(),
            source_files: Vec::new(),
            nodes,
            entries: Vec::new(),
        };
        assert_eq!(CompiledFile::decode(&file.encode().unwrap()).unwrap(), file);
    }

    #[test]
    fn simd_pack_decode_rejects_invalid_width_truncation_and_nonfinite_lanes() {
        let valid = EncodedPack {
            kind: LaneKind::F32,
            bits: EncodedPackBits::Bits128([0; 16]),
        };
        let mut encoded = postcard::to_allocvec(&valid).unwrap();
        assert!(postcard::from_bytes::<SimdPack>(&encoded[..encoded.len() - 1]).is_err());
        encoded[1] = 3;
        assert!(postcard::from_bytes::<SimdPack>(&encoded).is_err());

        let mut infinity = [0; 16];
        infinity[12..].copy_from_slice(&f32::INFINITY.to_le_bytes());
        let mut nan = [0; 32];
        nan[24..].copy_from_slice(&f64::NAN.to_le_bytes());
        for pack in [
            EncodedPack {
                kind: LaneKind::F32,
                bits: EncodedPackBits::Bits128(infinity),
            },
            EncodedPack {
                kind: LaneKind::F64,
                bits: EncodedPackBits::Bits256(nan),
            },
        ] {
            let encoded = postcard::to_allocvec(&pack).unwrap();
            assert!(postcard::from_bytes::<SimdPack>(&encoded).is_err());
        }
    }

    #[test]
    fn simd_mask_decode_rejects_invalid_lane_counts_and_high_bits() {
        for mask in [
            EncodedMask { lanes: 3, bits: 0 },
            EncodedMask { lanes: 4, bits: 16 },
        ] {
            let encoded = postcard::to_allocvec(&mask).unwrap();
            assert!(postcard::from_bytes::<SimdMask>(&encoded).is_err());
        }
    }
}
