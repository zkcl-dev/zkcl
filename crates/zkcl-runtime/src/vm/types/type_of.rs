use crate::condition::{Class, Fault};
use crate::heap::{ArrayElement, StreamKind, Word};
use crate::number::NumberKind;
use crate::symbol::{Name, Package};
use crate::vm::{arrays, classes, is_function};
use crate::{EvalError, Root, Runtime};

fn specialized_array_type(
    runtime: &Runtime,
    value: Word,
    element: Option<ArrayElement>,
    simple: bool,
    at: usize,
) -> Result<Word, Fault> {
    let dimensions = runtime
        .heap
        .array_dimensions(value)
        .expect("live array has dimensions");
    let mut list = runtime.root_slot(Word::NIL);
    for dimension in dimensions.into_iter().rev() {
        let number = Word::fixnum(dimension as isize).ok_or_else(|| {
            Fault::plain(
                Class::Error,
                EvalError::new(at, "array dimension exceeds the fixnum range"),
            )
        })?;
        list = runtime.root_slot(
            runtime
                .heap
                .cons(|| (number, list.get()))
                .map_err(|error| EvalError::out_of_memory(at, error))?,
        );
    }
    let dimensions = runtime.root_slot(list.get());
    let element = runtime.root_slot(arrays::element_type_specifier(runtime, element, at)?);
    let dimension_tail = runtime.root_slot(
        runtime
            .heap
            .cons(|| (dimensions.get(), Word::NIL))
            .map_err(|error| EvalError::out_of_memory(at, error))?,
    );
    let tail = runtime.root_slot(
        runtime
            .heap
            .cons(|| (element.get(), dimension_tail.get()))
            .map_err(|error| EvalError::out_of_memory(at, error))?,
    );
    let name = if simple { "SIMPLE-ARRAY" } else { "ARRAY" };
    let head = runtime.root_slot(runtime.intern(&Name::common_lisp(name), at)?);
    runtime
        .heap
        .cons(|| (head.get(), tail.get()))
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

pub(in crate::vm) fn type_of(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    let heap = &runtime.heap;
    if let Some(class) = heap.instance_class(value) {
        return proper_class_type(runtime, class, at);
    }
    if heap.structure_layout(value).is_some()
        || heap.condition_class(value).is_some()
        || heap.class_info(value).is_some()
        || heap.generic_info(value).is_some()
        || heap.method_info(value).is_some()
        || heap.eql_specializer_object(value).is_some()
        || heap.slot_definition_info(value).is_some()
    {
        let class = classes::class_of(runtime, value, at)?;
        return proper_class_type(runtime, class, at);
    }
    if let Some(kind) = super::target_number_kind(runtime, value) {
        return numeric_type_of(runtime, value, kind, at);
    }
    if let Some(shape) = heap.simd_pack_shape(value) {
        return Ok(super::simd::pack_symbol(runtime, shape));
    }
    if let Some(mask) = heap.simd_mask(value) {
        return super::simd::mask_type(runtime, mask.lanes(), at);
    }
    let name = if value == Word::NIL {
        "NULL"
    } else if value == Word::T {
        "BOOLEAN"
    } else if let Some(character) = value.as_character() {
        if crate::character::is_standard(character) {
            "STANDARD-CHAR"
        } else {
            "BASE-CHAR"
        }
    } else if heap.symbol_home(value) == Some(Package::KEYWORD) {
        "KEYWORD"
    } else if heap.is_symbol(value) {
        "SYMBOL"
    } else if heap.cons_fields(value).is_some() {
        "CONS"
    } else if let Some(info) = heap.array_info(value) {
        if info.nil_element
            || matches!(
                info.element,
                ArrayElement::Octet | ArrayElement::SingleFloat | ArrayElement::DoubleFloat
            )
        {
            return specialized_array_type(
                runtime,
                value,
                (!info.nil_element).then_some(info.element),
                info.simple(),
                at,
            );
        }
        match (info.rank, info.simple(), info.element) {
            (1, true, ArrayElement::General) => "SIMPLE-VECTOR",
            (1, true, ArrayElement::Bit) => "SIMPLE-BIT-VECTOR",
            (1, false, ArrayElement::Bit) => "BIT-VECTOR",
            (1, false, ArrayElement::General) => "VECTOR",
            (1, true, ArrayElement::Character) => "SIMPLE-BASE-STRING",
            (1, false, ArrayElement::Character) => "BASE-STRING",
            (_, true, _) => "SIMPLE-ARRAY",
            _ => "ARRAY",
        }
    } else if let Some(fields) = heap.pathname_fields(value) {
        if heap.is_string(fields[0]) {
            "LOGICAL-PATHNAME"
        } else {
            "PATHNAME"
        }
    } else if value.package_index().is_some() {
        "PACKAGE"
    } else if heap.restart_name(value).is_some() {
        "RESTART"
    } else if heap.hash_table_info(value).is_some() {
        "HASH-TABLE"
    } else if heap.is_random_state(value) {
        "RANDOM-STATE"
    } else if heap.readtable_case(value).is_some() {
        "READTABLE"
    } else if let Some(stream) = heap.stream_info(value) {
        match stream.kind {
            StreamKind::StringInput | StreamKind::StringOutput | StreamKind::StringOutputTo => {
                "STRING-STREAM"
            }
            StreamKind::FileInput
            | StreamKind::FileOutput
            | StreamKind::FileIo
            | StreamKind::FileProbe => "FILE-STREAM",
            StreamKind::Broadcast => "BROADCAST-STREAM",
            StreamKind::Concatenated => "CONCATENATED-STREAM",
            StreamKind::Echo => "ECHO-STREAM",
            StreamKind::Synonym => "SYNONYM-STREAM",
            StreamKind::TwoWay => "TWO-WAY-STREAM",
            StreamKind::StandardInput
            | StreamKind::StandardOutput
            | StreamKind::ErrorOutput
            | StreamKind::Terminal => "STREAM",
        }
    } else if heap.is_treelist(value) {
        return Ok(runtime.intern_package(Package::TREELIST, "TREELIST", at)?.0);
    } else if heap.is_mutable_treelist(value) {
        return Ok(runtime
            .intern_package(Package::TREELIST, "MUTABLE-TREELIST", at)?
            .0);
    } else if is_function(runtime, value) {
        "COMPILED-FUNCTION"
    } else {
        "T"
    };
    Ok(runtime.intern(&Name::common_lisp(name), at)?)
}

fn proper_class_type(runtime: &Runtime, class: Word, at: usize) -> Result<Word, Fault> {
    let class = runtime.root_slot(class);
    let name = runtime.root_slot(classes::class_name(runtime, class.get(), at)?);
    if name.get() != Word::NIL
        && classes::find_named_class(runtime, name.get(), at)? == Some(class.get())
    {
        Ok(name.get())
    } else {
        Ok(class.get())
    }
}

fn numeric_type_of(
    runtime: &Runtime,
    value: Word,
    kind: NumberKind,
    at: usize,
) -> Result<Word, Fault> {
    let fixnum_max = runtime.compilation_target().fixnum_bounds().1;
    let upper_bound = || {
        runtime
            .integer_from(fixnum_max)
            .map_err(|error| EvalError::out_of_memory(at, error))
    };
    let name = match kind {
        NumberKind::Fixnum => match runtime.target_fixnum_value(value).expect("target fixnum") {
            0 | 1 => "BIT",
            n if n > 0 => {
                let lower = runtime.fixnum(0).expect("zero fixnum");
                let upper = upper_bound()?;
                return numeric_specifier(runtime, "INTEGER", [lower, upper], at);
            }
            _ => "FIXNUM",
        },
        NumberKind::Bignum => {
            let positive = value.as_fixnum().map_or_else(
                || {
                    !runtime
                        .heap
                        .bignum_info(value)
                        .expect("bignum kind")
                        .negative
                },
                |number| number > 0,
            );
            if positive {
                let limit = upper_bound()?;
                let bound = runtime
                    .cons(&limit, &runtime.nil())
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
                return numeric_specifier(runtime, "INTEGER", [bound], at);
            }
            "BIGNUM"
        }
        NumberKind::Ratio => "RATIO",
        NumberKind::Single => "SINGLE-FLOAT",
        NumberKind::Double => "DOUBLE-FLOAT",
        NumberKind::Complex => {
            let (real, _) = runtime.heap.complex_fields(value).expect("complex kind");
            let part_name = match runtime.heap.number_kind(real) {
                Some(NumberKind::Single) => "SINGLE-FLOAT",
                Some(NumberKind::Double) => "DOUBLE-FLOAT",
                _ => "RATIONAL",
            };
            let part = runtime.root(runtime.intern(&Name::common_lisp(part_name), at)?);
            return numeric_specifier(runtime, "COMPLEX", [part], at);
        }
    };
    Ok(runtime.intern(&Name::common_lisp(name), at)?)
}

fn numeric_specifier<'rt, const N: usize>(
    runtime: &'rt Runtime,
    name: &str,
    arguments: [Root<'rt>; N],
    at: usize,
) -> Result<Word, Fault> {
    let operator = runtime.root(runtime.intern(&Name::common_lisp(name), at)?);
    let mut spec = runtime.nil();
    for argument in arguments.iter().rev().chain(std::iter::once(&operator)) {
        spec = runtime
            .cons(argument, &spec)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
    }
    Ok(spec.slot.get())
}
