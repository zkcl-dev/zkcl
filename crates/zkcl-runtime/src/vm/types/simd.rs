use rustc_hash::FxHashMap;
use zkcl_bytecode::{LaneKind, PackShape, SimdMask};

use crate::condition::Fault;
use crate::heap::{Heap, RootFrame, Word};
use crate::symbol::Package;
use crate::{EvalError, Runtime};

const PACK: usize = 0;
const MASK: usize = 1;
const FIRST_SHAPE: usize = 2;
const PACK_SHAPES: usize = PackShape::ALL.len();

/// Keep the original symbols rooted independently of the package's mappings.
pub(crate) struct SimdTypeSymbols {
    symbols: RootFrame,
    shape_by_id: FxHashMap<usize, usize>,
}

impl SimdTypeSymbols {
    pub(in crate::vm) fn pack(&self) -> Word {
        self.symbols.get(PACK)
    }

    pub(in crate::vm) fn mask(&self) -> Word {
        self.symbols.get(MASK)
    }
}

pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    if runtime.simd_type_symbols.get().is_some() {
        return Ok(());
    }
    let package = Package::SIMD;
    let export = |name: &str| -> Result<Word, EvalError> {
        let symbol = runtime.intern_package(package, name, 0)?.0;
        runtime
            .packages
            .borrow_mut()
            .insert(package, name.to_owned(), symbol, true);
        Ok(symbol)
    };
    let symbols = runtime.root_frame(FIRST_SHAPE + PACK_SHAPES);
    symbols.set(PACK, export("PACK")?);
    symbols.set(MASK, export("MASK")?);
    let mut shape_by_id = FxHashMap::default();
    for (index, shape) in PackShape::ALL.into_iter().enumerate() {
        let symbol = export(&shape.name())?;
        symbols.set(FIRST_SHAPE + index, symbol);
        let id = runtime
            .heap
            .symbol_attributes(symbol)
            .expect("SIMD type symbol")
            .id;
        shape_by_id.insert(id, index);
    }
    let initialized = runtime.simd_type_symbols.set(SimdTypeSymbols {
        symbols,
        shape_by_id,
    });
    debug_assert!(initialized.is_ok());
    Ok(())
}

pub(in crate::vm) fn pack_class_symbol(runtime: &Runtime) -> Word {
    runtime
        .simd_type_symbols
        .get()
        .expect("SIMD types initialized")
        .pack()
}

pub(in crate::vm) fn mask_class_symbol(runtime: &Runtime) -> Word {
    runtime
        .simd_type_symbols
        .get()
        .expect("SIMD types initialized")
        .mask()
}

pub(in crate::vm) fn pack_symbol(runtime: &Runtime, shape: PackShape) -> Word {
    let index = shape_index(shape);
    runtime
        .simd_type_symbols
        .get()
        .expect("SIMD types initialized")
        .symbols
        .get(FIRST_SHAPE + index)
}

pub(in crate::vm) fn mask_type(runtime: &Runtime, lanes: usize, at: usize) -> Result<Word, Fault> {
    let lanes = Word::fixnum(lanes as isize).expect("mask lane count fits a fixnum");
    let heap = &runtime.heap;
    let tail = runtime.root_slot(
        heap.cons(|| (lanes, Word::NIL))
            .map_err(|err| EvalError::out_of_memory(at, err))?,
    );
    heap.cons(|| (mask_class_symbol(runtime), tail.get()))
        .map_err(|err| EvalError::out_of_memory(at, err).into())
}

pub(in crate::vm) fn exact_pack_shape(runtime: &Runtime, spec: Word) -> Option<PackShape> {
    let SimdTypes(bits) = parse(runtime, spec)?.ok()?;
    if bits.count_ones() != 1 {
        return None;
    }
    PackShape::ALL.get(bits.trailing_zeros() as usize).copied()
}

fn shape_index(shape: PackShape) -> usize {
    PackShape::ALL
        .iter()
        .position(|&candidate| candidate == shape)
        .expect("valid pack shape")
}

fn mask_index(lanes: usize) -> usize {
    PACK_SHAPES
        + SimdMask::LANE_COUNTS
            .iter()
            .position(|&count| count == lanes)
            .expect("valid mask lane count")
}

/// A set of pack shapes and mask lane counts, indexed by the bytecode tables.
#[derive(Clone, Copy)]
pub(super) struct SimdTypes(u64);

impl SimdTypes {
    const PACK: Self = Self((1 << PACK_SHAPES) - 1);
    const MASK: Self = Self(((1 << SimdMask::LANE_COUNTS.len()) - 1) << PACK_SHAPES);

    pub(super) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(super) fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub(super) fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub(super) fn matches(self, heap: &Heap, value: Word) -> bool {
        let index = if let Some(shape) = heap.simd_pack_shape(value) {
            shape_index(shape)
        } else if let Some(mask) = heap.simd_mask(value) {
            mask_index(mask.lanes())
        } else {
            return false;
        };
        self.0 & (1 << index) != 0
    }
}

pub(super) fn parse(runtime: &Runtime, spec: Word) -> Option<Result<SimdTypes, &'static str>> {
    let symbols = runtime.simd_type_symbols.get()?;
    let heap = &runtime.heap;
    let compound = heap.cons_fields(spec);
    let (head, operands) = compound.unwrap_or((spec, Word::NIL));
    Some(if head == symbols.pack() {
        pack_type(runtime, operands)
    } else if head == symbols.mask() {
        mask_type_set(runtime, operands)
    } else if compound.is_none() {
        let index = symbols.shape_by_id.get(&heap.symbol_attributes(head)?.id)?;
        Ok(SimdTypes(1 << index))
    } else {
        return None;
    })
}

fn pack_type(runtime: &Runtime, operands: Word) -> Result<SimdTypes, &'static str> {
    const INVALID: &str = "expected a ZK-SIMD:PACK lane kind and count, or *";
    let operands = super::arguments(&runtime.heap, operands, 2).ok_or(INVALID)?;
    if operands.is_empty() {
        return Ok(SimdTypes::PACK);
    }
    let kind = match operands
        .first()
        .copied()
        .filter(|&word| !super::wildcard(runtime, word))
    {
        None => None,
        Some(word) => Some(lane_kind(runtime, word).ok_or(INVALID)?),
    };
    let lanes = lane_count(runtime, operands.get(1).copied()).ok_or(INVALID)?;
    let mut bits = 0;
    for (index, shape) in PackShape::ALL.into_iter().enumerate() {
        if kind.is_none_or(|kind| shape.kind() == kind)
            && lanes.is_none_or(|lanes| shape.lanes() == lanes)
        {
            bits |= 1 << index;
        }
    }
    if bits == 0 {
        return Err("unsupported ZK-SIMD:PACK shape");
    }
    Ok(SimdTypes(bits))
}

fn mask_type_set(runtime: &Runtime, operands: Word) -> Result<SimdTypes, &'static str> {
    const INVALID: &str = "expected a ZK-SIMD:MASK lane count or *";
    let lanes = super::optional_argument(&runtime.heap, operands).ok_or(INVALID)?;
    match lane_count(runtime, lanes).ok_or(INVALID)? {
        None => Ok(SimdTypes::MASK),
        Some(lanes) => Ok(SimdTypes(1 << mask_index(lanes))),
    }
}

fn lane_count(runtime: &Runtime, word: Option<Word>) -> Option<Option<usize>> {
    match word.filter(|&word| !super::wildcard(runtime, word)) {
        None => Some(None),
        Some(word) => {
            let lanes = usize::try_from(word.as_fixnum()?).ok()?;
            SimdMask::LANE_COUNTS
                .contains(&lanes)
                .then_some(Some(lanes))
        }
    }
}

fn lane_kind(runtime: &Runtime, word: Word) -> Option<LaneKind> {
    if runtime.heap.symbol_home(word) != Some(Package::KEYWORD) {
        return None;
    }
    let packages = runtime.packages.borrow();
    LaneKind::ALL.into_iter().find(|kind| {
        packages
            .present(Package::KEYWORD, kind.keyword())
            .is_some_and(|(keyword, _)| keyword == word)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd_subtypep_combines_shape_sets() {
        let runtime = Runtime::start().unwrap();
        runtime
            .load_str("(deftype simd-f32s () '(zk-simd:pack :f32 *))")
            .unwrap();
        for (source, target, expected) in [
            (
                "simd-f32s",
                "(or zk-simd:f32x4 zk-simd:f32x8 zk-simd:f32x16)",
                true,
            ),
            ("simd-f32s", "(or zk-simd:f32x4 zk-simd:f32x8)", false),
            ("(and simd-f32s (zk-simd:pack * 4))", "zk-simd:f32x4", true),
            (
                "(and (zk-simd:mask *) (not (zk-simd:mask 4)))",
                "(or (zk-simd:mask 2) (zk-simd:mask 8) (zk-simd:mask 16)
                     (zk-simd:mask 32) (zk-simd:mask 64))",
                true,
            ),
        ] {
            let form = format!("(subtypep '{source} '{target})");
            let values = runtime.eval(&form).unwrap();
            assert!(values[1].is_t(), "undecided: {form}");
            assert_eq!(values[0].is_t(), expected, "{form}");
        }
    }

    #[test]
    fn simd_type_and_class_names_keep_their_original_symbols() {
        let runtime = Runtime::start().unwrap();
        let values = runtime
            .eval(
                r#"(let* ((shape 'zk-simd:f32x4)
                          (name 'zk-simd:pack)
                          (pack-class (find-class name))
                          (simd-package (symbol-package name)))
                     (rename-package simd-package "ZK-SIMD-RENAMED")
                     (assert (unintern shape simd-package))
                     (assert (unintern name simd-package))
                     (let ((new-shape (intern "F32X4" simd-package))
                           (new-name (intern "PACK" simd-package)))
                       (zk:gc)
                       (assert (eq (find-class name) pack-class))
                       (assert (null (find-class new-name nil)))
                       (assert (equal '(t t) (multiple-value-list (subtypep shape pack-class))))
                       (assert (equal '(nil nil) (multiple-value-list (subtypep new-shape pack-class))))
                       (assert (equal '(nil nil) (multiple-value-list (subtypep new-name pack-class)))))
                     t)"#,
            )
            .unwrap();
        assert!(values[0].is_t());
    }
}
