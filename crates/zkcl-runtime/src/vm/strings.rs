use std::cmp::Ordering;
use std::ops::Range;

use crate::character::{is_alphanumeric, simple_lowercase, simple_uppercase};
use crate::condition::{Class, Fault};
use crate::heap::{ListSpine, RootSlots, Word};
use crate::{EvalError, Runtime};

use super::named_keyword_arguments as keys;

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Compare(Relation, bool),
    ChangeCase { case: Case, destructive: bool },
    Trim { left: bool, right: bool },
    MakeString,
    String,
    Stringp,
    SimpleStringP,
    Length,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Relation {
    Equal,
    Different,
    Less,
    Greater,
    LessOrEqual,
    GreaterOrEqual,
}

#[derive(Clone, Copy)]
pub(crate) enum Case {
    Upper,
    Lower,
    Capitalize,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("STRING=", Op::Compare(Relation::Equal, false)),
    ("STRING/=", Op::Compare(Relation::Different, false)),
    ("STRING<", Op::Compare(Relation::Less, false)),
    ("STRING>", Op::Compare(Relation::Greater, false)),
    ("STRING<=", Op::Compare(Relation::LessOrEqual, false)),
    ("STRING>=", Op::Compare(Relation::GreaterOrEqual, false)),
    ("STRING-EQUAL", Op::Compare(Relation::Equal, true)),
    ("STRING-NOT-EQUAL", Op::Compare(Relation::Different, true)),
    ("STRING-LESSP", Op::Compare(Relation::Less, true)),
    ("STRING-GREATERP", Op::Compare(Relation::Greater, true)),
    (
        "STRING-NOT-GREATERP",
        Op::Compare(Relation::LessOrEqual, true),
    ),
    (
        "STRING-NOT-LESSP",
        Op::Compare(Relation::GreaterOrEqual, true),
    ),
    (
        "STRING-UPCASE",
        Op::ChangeCase {
            case: Case::Upper,
            destructive: false,
        },
    ),
    (
        "STRING-DOWNCASE",
        Op::ChangeCase {
            case: Case::Lower,
            destructive: false,
        },
    ),
    (
        "STRING-CAPITALIZE",
        Op::ChangeCase {
            case: Case::Capitalize,
            destructive: false,
        },
    ),
    (
        "NSTRING-UPCASE",
        Op::ChangeCase {
            case: Case::Upper,
            destructive: true,
        },
    ),
    (
        "NSTRING-DOWNCASE",
        Op::ChangeCase {
            case: Case::Lower,
            destructive: true,
        },
    ),
    (
        "NSTRING-CAPITALIZE",
        Op::ChangeCase {
            case: Case::Capitalize,
            destructive: true,
        },
    ),
    (
        "STRING-TRIM",
        Op::Trim {
            left: true,
            right: true,
        },
    ),
    (
        "STRING-LEFT-TRIM",
        Op::Trim {
            left: true,
            right: false,
        },
    ),
    (
        "STRING-RIGHT-TRIM",
        Op::Trim {
            left: false,
            right: true,
        },
    ),
    ("MAKE-STRING", Op::MakeString),
    ("STRING", Op::String),
    ("STRINGP", Op::Stringp),
    ("SIMPLE-STRING-P", Op::SimpleStringP),
    ("LENGTH", Op::Length),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Compare(..) => count >= 2,
            Self::Trim { .. } => count == 2,
            Self::ChangeCase { .. } | Self::MakeString => count >= 1,
            Self::String | Self::Stringp | Self::SimpleStringP | Self::Length => count == 1,
        }
    }
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    match op {
        Op::Compare(relation, caseless) => compare(runtime, frame, args, relation, caseless, at),
        Op::ChangeCase { case, destructive } => {
            change_case(runtime, frame, args, case, destructive, at)
        }
        Op::Trim { left, right } => trim(runtime, frame, args, left, right, at),
        Op::MakeString => make_string(runtime, frame, args, at),
        Op::String => string_designator(runtime, frame.get(args.start), at),
        Op::Stringp => Ok(boolean(runtime.heap.is_string(frame.get(args.start)))),
        Op::SimpleStringP => {
            let word = frame.get(args.start);
            Ok(boolean(
                runtime.heap.is_string(word)
                    && runtime
                        .heap
                        .array_info(word)
                        .is_some_and(|info| info.simple()),
            ))
        }
        Op::Length => length(runtime, frame.get(args.start), at),
    }
}

fn make_string(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let [initial, element_type] = keys(
        runtime,
        frame,
        args.start + 1..args.end,
        ["INITIAL-ELEMENT", "ELEMENT-TYPE"],
        at,
    )?;
    let size = index(runtime, frame.get(args.start), at)?;
    let initial_supplied = initial.is_some();
    let initial = initial.map_or(Word::character(' '), |reg| frame.get(reg));
    let character = initial.as_character().ok_or_else(|| {
        Fault::type_error(runtime, 
            initial,
            "CHARACTER",
            EvalError::new(at, "a string's initial element must be a character"),
        )
    })?;
    if let Some(reg) = element_type {
        let spec = runtime.root_slot(super::type_check::expand_derived_type(
            runtime,
            frame.get(reg),
            at,
        )?);
        let element = super::arrays::upgraded_expanded_type(runtime, spec.get(), at, 0)?;
        if !matches!(element, Some(crate::heap::ArrayElement::Character) | None) {
            return Err(Fault::plain(
                Class::Error,
                EvalError::new(at, "unsupported string element type"),
            ));
        }
        if initial_supplied
            && (size > 0 || element.is_some())
            && !super::types::typep(runtime, initial, spec.get(), at)?
        {
            return Err(Fault::type_error_specifier(runtime, 
                initial,
                frame.get(reg),
                EvalError::new(
                    at,
                    "the initial element is outside the string's element type",
                ),
            ));
        }
        if element.is_none() {
            return runtime
                .heap
                .nil_element_array(&[size], false, None, size, |_| '\0')
                .map_err(|error| EvalError::out_of_memory(at, error).into());
        }
    }
    runtime
        .heap
        .repeated_string(character, size)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn compare(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    relation: Relation,
    caseless: bool,
    at: usize,
) -> Result<Word, Fault> {
    let [start1, end1, start2, end2] = keys(
        runtime,
        frame,
        args.start + 2..args.end,
        ["START1", "END1", "START2", "END2"],
        at,
    )?;
    let left_root = runtime.root_slot(string_designator(runtime, frame.get(args.start), at)?);
    let right_root = runtime.root_slot(string_designator(runtime, frame.get(args.start + 1), at)?);
    runtime
        .heap
        .with_two_string_slices(left_root.get(), right_root.get(), |left, right| {
            let slice_bounds = |text, start: Option<usize>, end: Option<usize>| {
                bounds(
                    runtime,
                    text,
                    start.map(|reg| frame.get(reg)),
                    end.map(|reg| frame.get(reg)),
                    "bounding indices are outside the string",
                    at,
                )
            };
            let (left_bytes, first_index) = slice_bounds(left, start1, end1)?;
            let (right_bytes, _) = slice_bounds(right, start2, end2)?;
            let left = &left[left_bytes];
            let right = &right[right_bytes];
            if relation == Relation::Equal && !caseless {
                return Ok(boolean(left == right));
            }
            let canonical = |ch| if caseless { simple_lowercase(ch) } else { ch };
            let mut left = left.chars().map(canonical);
            let mut right = right.chars().map(canonical);
            let mut index = first_index;
            let ordering = loop {
                let first = left.next();
                let ordering = first.cmp(&right.next());
                if first.is_none() || ordering != Ordering::Equal {
                    break ordering;
                }
                index += 1;
            };
            let matched = match relation {
                Relation::Equal => return Ok(boolean(ordering == Ordering::Equal)),
                Relation::Different => ordering != Ordering::Equal,
                Relation::Less => ordering == Ordering::Less,
                Relation::Greater => ordering == Ordering::Greater,
                Relation::LessOrEqual => ordering != Ordering::Greater,
                Relation::GreaterOrEqual => ordering != Ordering::Less,
            };
            if matched {
                index_word(index, at)
            } else {
                Ok(Word::NIL)
            }
        })
        .unwrap()
}

fn change_case(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    case: Case,
    destructive: bool,
    at: usize,
) -> Result<Word, Fault> {
    let [start, end] = keys(
        runtime,
        frame,
        args.start + 1..args.end,
        ["START", "END"],
        at,
    )?;
    let input = frame.get(args.start);
    let word = if destructive {
        if !runtime.heap.is_string(input) {
            return Err(Fault::type_error(runtime, 
                input,
                "STRING",
                EvalError::new(at, "expected a string"),
            ));
        }
        input
    } else {
        string_designator(runtime, input, at)?
    };
    let word_root = runtime.root_slot(word);
    let (prefix, mapped_chars, suffix, start_char) = runtime
        .heap
        .with_string_slice(word_root.get(), |text| {
            let (bytes, start_char) = bounds(
                runtime,
                text,
                start.map(|reg| frame.get(reg)),
                end.map(|reg| frame.get(reg)),
                "bounding indices are outside the string",
                at,
            )?;
            let mut at_word_start = true;
            let mapped: Vec<char> = text[bytes.clone()]
                .chars()
                .map(|ch| match case {
                    Case::Upper => simple_uppercase(ch),
                    Case::Lower => simple_lowercase(ch),
                    Case::Capitalize => {
                        let mapped = if at_word_start {
                            simple_uppercase(ch)
                        } else {
                            simple_lowercase(ch)
                        };
                        at_word_start = !is_alphanumeric(ch);
                        mapped
                    }
                })
                .collect();
            Ok::<_, Fault>((
                (!destructive).then(|| text[..bytes.start].to_owned()),
                mapped,
                (!destructive).then(|| text[bytes.end..].to_owned()),
                start_char,
            ))
        })
        .unwrap()?;
    if destructive {
        for (offset, ch) in mapped_chars.into_iter().enumerate() {
            runtime
                .heap
                .set_array_slot(word_root.get(), start_char + offset, Word::character(ch))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
        }
        return Ok(word_root.get());
    }
    let mut result = prefix.unwrap();
    result.extend(mapped_chars);
    result.push_str(&suffix.unwrap());
    runtime
        .heap
        .string(&result)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn trim(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    left: bool,
    right: bool,
    at: usize,
) -> Result<Word, Fault> {
    let bag_elements = super::arrays::sequence_elements(runtime, frame.get(args.start), at)?;
    let mut bag = Vec::with_capacity(bag_elements.len());
    for index in 0..bag_elements.len() {
        let element = bag_elements.get(index);
        let ch = element.as_character().ok_or_else(|| {
            Fault::type_error(runtime, 
                element,
                "CHARACTER",
                EvalError::new(at, "character bag must contain characters"),
            )
        })?;
        bag.push(ch);
    }
    let string = runtime.root_slot(string_designator(runtime, frame.get(args.start + 1), at)?);
    let trimmed = runtime
        .heap
        .with_string_slice(string.get(), |text| {
            let slice = match (left, right) {
                (true, true) => text.trim_matches(|ch| bag.contains(&ch)),
                (true, false) => text.trim_start_matches(|ch| bag.contains(&ch)),
                (false, true) => text.trim_end_matches(|ch| bag.contains(&ch)),
                (false, false) => text,
            };
            slice.to_owned()
        })
        .unwrap();
    runtime
        .heap
        .string(&trimmed)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn string_designator(runtime: &Runtime, word: Word, at: usize) -> Result<Word, Fault> {
    if runtime.heap.is_string(word) {
        return Ok(word);
    }
    if let Some(character) = word.as_character() {
        let mut utf8 = [0; 4];
        return runtime
            .heap
            .string(character.encode_utf8(&mut utf8))
            .map_err(|error| EvalError::out_of_memory(at, error).into());
    }
    runtime.symbol_name_word(word, at)?.ok_or_else(|| {
        Fault::string_designator(runtime, word, EvalError::new(at, "expected a string designator"))
    })
}

/// Return the UTF-8 byte range and the initial scalar index for comparison
/// results that have to refer back to the complete first string.
pub(super) fn bounds(
    runtime: &Runtime,
    text: &str,
    start: Option<Word>,
    end: Option<Word>,
    outside: &'static str,
    at: usize,
) -> Result<(Range<usize>, usize), Fault> {
    let start = start
        .map(|word| index(runtime, word, at))
        .transpose()?
        .unwrap_or(0);
    let end = end
        .filter(|&word| word != Word::NIL)
        .map(|word| index(runtime, word, at))
        .transpose()?;
    let outside = || Fault::plain(Class::Error, EvalError::new(at, outside));
    let range = crate::character::utf8_byte_range(text, start, end).ok_or_else(outside)?;
    Ok((range, start))
}

pub(super) fn index(runtime: &Runtime, word: Word, at: usize) -> Result<usize, Fault> {
    word.as_fixnum()
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| {
            Fault::non_negative_integer(
                runtime,
                word,
                EvalError::new(at, "expected a non-negative index"),
            )
        })
}

fn index_word(index: usize, at: usize) -> Result<Word, Fault> {
    isize::try_from(index)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| {
            EvalError::new(at, "sequence index exceeds the supported fixnum range").into()
        })
}

fn boolean(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}

fn length(runtime: &Runtime, word: Word, at: usize) -> Result<Word, Fault> {
    if let Some(length) =
        super::arrays::vector_length(runtime, word).or_else(|| runtime.heap.treelist_len(word))
    {
        return index_word(length, at);
    }
    let invalid = || {
        Fault::type_error(runtime, 
            word,
            "SEQUENCE",
            EvalError::new(at, "expected a proper sequence"),
        )
    };
    let mut spine = ListSpine::new(word);
    let mut len = 0;
    while spine.next(&runtime.heap).map_err(|_| invalid())?.is_some() {
        len += 1;
    }
    if spine.tail() != Word::NIL {
        return Err(invalid());
    }
    index_word(len, at)
}
