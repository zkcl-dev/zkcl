//! ZK-SIMD symbols and lambda lists shared by the runtime and source tooling.

pub const PACKAGE: &str = "ZK-SIMD";

/// Metadata for a public function in the ZK-SIMD package.
#[derive(Debug, Clone, Copy)]
pub struct SimdFunction {
    pub name: &'static str,
    /// Lambda list entries, with defaults kept together with their parameter names.
    pub params: &'static [&'static str],
    /// Lambda list source without the enclosing parentheses.
    pub lambda_list: &'static str,
    pub documentation: &'static str,
}

macro_rules! functions {
    ($($name:literal ($first:literal $(, $param:literal)*) $doc:literal;)*) => {
        &[$(SimdFunction {
            name: $name,
            params: &[$first $(, $param)*],
            lambda_list: concat!($first $(, " ", $param)*),
            documentation: $doc,
        }),*]
    };
}

pub const FUNCTIONS: &[SimdFunction] = functions! {
    "MAKE-PACK" ("type", "&rest", "lanes") "Construct a pack of the exact type from its lane values in order.";
    "SPLAT" ("type", "scalar") "Broadcast a scalar to every lane of the exact pack type.";
    "PACK-P" ("object") "Return true when object is a pack.";
    "MASK-P" ("object") "Return true when object is a mask.";
    "LANE-COUNT" ("value") "Return the number of lanes in a pack or mask.";
    "LANE" ("pack", "index") "Return the scalar at a zero-based lane index.";
    "PACK-LANES" ("pack") "Return a new general vector containing the lanes in order.";
    "ADD" ("x", "y") "Add corresponding lanes of two packs of the same shape.";
    "SUB" ("x", "y") "Subtract each lane of y from the corresponding lane of x.";
    "MUL" ("x", "y") "Multiply corresponding lanes of two packs of the same shape.";
    "DIV" ("x", "y") "Divide corresponding floating-point lanes, signaling at the first lane that fails.";
    "SQRT" ("x") "Take the square root of each floating-point lane.";
    "FMA" ("x", "y", "z") "Compute x × y + z in each floating-point lane with one rounding.";
    "MIN" ("x", "y") "Take the minimum in each lane, choosing negative zero when zero signs differ.";
    "MAX" ("x", "y") "Take the maximum in each lane, choosing positive zero when zero signs differ.";
    "ADD-SATURATING" ("x", "y") "Add integer lanes and clamp each sum to the lane type's range.";
    "SUB-SATURATING" ("x", "y") "Subtract integer lanes and clamp each difference to the lane type's range.";
    "ADD-CHECKED" ("x", "y") "Add integer lanes and signal INTEGER-OVERFLOW at the first overflowing lane.";
    "SUB-CHECKED" ("x", "y") "Subtract integer lanes and signal INTEGER-OVERFLOW at the first overflowing lane.";
    "BIT-AND" ("x", "y") "Compute bitwise AND on corresponding integer lanes.";
    "BIT-OR" ("x", "y") "Compute bitwise OR on corresponding integer lanes.";
    "BIT-XOR" ("x", "y") "Compute bitwise exclusive OR on corresponding integer lanes.";
    "BIT-NOT" ("x") "Complement the bits in each integer lane.";
    "SHIFT-LEFT" ("x", "count") "Shift each integer lane left by a nonnegative count, discarding high bits.";
    "SHIFT-RIGHT" ("x", "count") "Shift each integer lane right by a nonnegative count, sign-extending signed lanes.";
    "CMP=" ("x", "y") "Return a mask of lanes where x and y are numerically equal.";
    "CMP/=" ("x", "y") "Return a mask of lanes where x and y are numerically unequal.";
    "CMP<" ("x", "y") "Return a mask of lanes where x is less than y.";
    "CMP<=" ("x", "y") "Return a mask of lanes where x is less than or equal to y.";
    "CMP>" ("x", "y") "Return a mask of lanes where x is greater than y.";
    "CMP>=" ("x", "y") "Return a mask of lanes where x is greater than or equal to y.";
    "SELECT" ("mask", "x", "y") "Take active lanes from x and inactive lanes from y, preserving their bits.";
    "MASK-AND" ("x", "y") "Keep the lanes active in both masks.";
    "MASK-OR" ("x", "y") "Keep the lanes active in either mask.";
    "MASK-XOR" ("x", "y") "Keep the lanes active in exactly one of the two masks.";
    "MASK-NOT" ("mask") "Invert the bits within the mask's lane count.";
    "MASK-BITS" ("mask") "Return the mask as a nonnegative integer with lane i at bit i.";
    "MASK-FROM-BITS" ("lanes", "bits") "Construct a mask from a nonnegative integer whose set bits fit the lane count.";
    "MASK-FIRST" ("lanes", "count") "Activate the first count lanes of a mask with the given lane count.";
    "MASK-ANY-P" ("mask") "Return true when any mask lane is active.";
    "MASK-ALL-P" ("mask") "Return true when every mask lane is active.";
    "MASK-COUNT" ("mask") "Return the number of active mask lanes.";
    "SAME-BITS-P" ("x", "y") "Compare two SIMD values by kind, shape, and bits.";
    "PERMUTE" ("pack", "indices") "Select lanes of a pack in the order given by a vector of zero-based indices.";
    "SHUFFLE" ("x", "y", "indices") "Select lanes by indexing the logical concatenation of x and y.";
    "BITCAST" ("type", "pack") "Reinterpret a pack's bytes as another pack shape of the same total width.";
    "PACK-BITS" ("pack") "Return a new octet vector in lane order with little-endian bytes within each lane.";
    "PACK-FROM-BITS" ("type", "octets") "Construct a pack from exact-length octets in the order produced by PACK-BITS.";
    "CONVERT" ("type", "pack") "Convert lane values to a new pack type with the same lane count.";
    "LOAD-PACK" ("type", "array", "start") "Load a pack from consecutive row-major elements of a specialized array.";
    "STORE-PACK" ("array", "start", "value") "Store a pack into consecutive row-major elements and return value.";
    "LOAD-MASKED" ("type", "array", "start", "mask", "&optional", "passthrough") "Load active lanes and take inactive lanes from passthrough or zero.";
    "STORE-MASKED" ("array", "start", "mask", "value") "Store the active lanes into a specialized array and return value.";
    "GATHER" ("type", "array", "indices", "mask", "&optional", "passthrough") "Load active lanes by row-major index and take inactive lanes from passthrough or zero.";
    "SCATTER" ("array", "indices", "mask", "value") "Store active lanes by row-major index in lane order and return value.";
    "DIV-MASKED" ("mask", "x", "y", "passthrough") "Divide only the active lanes and preserve passthrough bits in the other lanes.";
    "SQRT-MASKED" ("mask", "x", "passthrough") "Take square roots only in the active lanes and preserve passthrough bits in the other lanes.";
    "REDUCE-SUM" ("pack", "&key", "(order :left)") "Sum the lanes in left-to-right order or using a pairwise tree.";
    "MUL-ADD-INTO" ("out", "a", "scale", "b", "&key", "out-start", "a-start", "b-start", "count") "Store a × scale + b with separate rounding for multiplication and addition. Count defaults to the shortest remaining array interval.";
    "OCTET-EQ-MASK" ("array", "start", "octet", "&optional", "(count 16)") "Compare up to 16 octets with octet and return a 16-lane mask with matching lanes active.";
    "CONDITION-LANE" ("condition") "Return the zero-based lane index recorded by a SIMD condition.";
    "CONDITION-LEVEL" ("condition") "Return the reduction level recorded by a SIMD arithmetic condition, when present.";
    "CONDITION-INDEX" ("condition") "Return the logical array index that triggered a SIMD bounds error.";
    "CONDITION-ARRAY" ("condition") "Return the array involved in a SIMD bounds error.";
};

/// Exact pack type names in lane-kind order, then by increasing total width.
pub const PACK_TYPES: &[&str] = &[
    "F32X4", "F32X8", "F32X16", "F64X2", "F64X4", "F64X8", "I8X16", "I8X32", "I8X64", "U8X16",
    "U8X32", "U8X64", "I16X8", "I16X16", "I16X32", "U16X8", "U16X16", "U16X32", "I32X4", "I32X8",
    "I32X16", "U32X4", "U32X8", "U32X16", "I64X2", "I64X4", "I64X8", "U64X2", "U64X4", "U64X8",
];

pub const CONDITIONS: &[&str] = &["INTEGER-OVERFLOW", "BOUNDS-ERROR"];

/// Look up a function by its case-sensitive Lisp symbol name.
#[must_use]
pub fn function(name: &str) -> Option<&'static SimdFunction> {
    FUNCTIONS.iter().find(|function| function.name == name)
}

#[must_use]
pub fn type_documentation(name: &str) -> Option<&'static str> {
    match name {
        "PACK" => Some(
            "An immutable pack of fixed-width scalar lanes. Use (pack lane-kind lanes) to specify a shape.",
        ),
        "MASK" => Some(
            "An immutable mask of active lanes. Use (mask lanes) to specify 2, 4, 8, 16, 32, or 64 lanes.",
        ),
        "INTEGER-OVERFLOW" => Some(
            "An arithmetic error from the first lane whose exact integer result is out of range.",
        ),
        "BOUNDS-ERROR" => {
            Some("An error recording the array, logical index, and first failing active SIMD lane.")
        }
        _ if PACK_TYPES.contains(&name) => {
            Some("An exact pack type. The name specifies its lane kind and lane count.")
        }
        _ => None,
    }
}

pub fn types() -> impl Iterator<Item = &'static str> {
    ["PACK", "MASK"]
        .into_iter()
        .chain(PACK_TYPES.iter().chain(CONDITIONS).copied())
}
