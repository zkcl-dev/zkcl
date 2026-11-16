use std::collections::HashSet;
use std::ops::Range;

use crate::character::{self, simple_lowercase, simple_uppercase};
use crate::condition::{Class, Fault};
use crate::heap::{RootSlots, Word};
use crate::{EvalError, Runtime};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Character,
    Characterp,
    Index { simple: bool },
    SetIndex { simple: bool },
    Code,
    FromCode,
    Name,
    FromName,
    Downcase,
    Upcase,
    Predicate(Predicate),
    Compare(Relation, bool),
    Digit,
    DigitValue,
}

#[derive(Clone, Copy)]
pub(crate) enum Predicate {
    Standard,
    Graphic,
    Alpha,
    Alphanumeric,
    Upper,
    Lower,
    BothCase,
}

#[derive(Clone, Copy)]
pub(crate) enum Relation {
    Equal,
    Distinct,
    Less,
    Greater,
    LessOrEqual,
    GreaterOrEqual,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("CHARACTER", Op::Character),
    ("CHARACTERP", Op::Characterp),
    ("CHAR", Op::Index { simple: false }),
    ("SCHAR", Op::Index { simple: true }),
    ("(SETF CHAR)", Op::SetIndex { simple: false }),
    ("(SETF SCHAR)", Op::SetIndex { simple: true }),
    ("CHAR-CODE", Op::Code),
    ("CHAR-INT", Op::Code),
    ("CODE-CHAR", Op::FromCode),
    ("CHAR-NAME", Op::Name),
    ("NAME-CHAR", Op::FromName),
    ("CHAR-DOWNCASE", Op::Downcase),
    ("CHAR-UPCASE", Op::Upcase),
    ("STANDARD-CHAR-P", Op::Predicate(Predicate::Standard)),
    ("GRAPHIC-CHAR-P", Op::Predicate(Predicate::Graphic)),
    ("ALPHA-CHAR-P", Op::Predicate(Predicate::Alpha)),
    ("ALPHANUMERICP", Op::Predicate(Predicate::Alphanumeric)),
    ("UPPER-CASE-P", Op::Predicate(Predicate::Upper)),
    ("LOWER-CASE-P", Op::Predicate(Predicate::Lower)),
    ("BOTH-CASE-P", Op::Predicate(Predicate::BothCase)),
    ("CHAR=", Op::Compare(Relation::Equal, false)),
    ("CHAR/=", Op::Compare(Relation::Distinct, false)),
    ("CHAR<", Op::Compare(Relation::Less, false)),
    ("CHAR>", Op::Compare(Relation::Greater, false)),
    ("CHAR<=", Op::Compare(Relation::LessOrEqual, false)),
    ("CHAR>=", Op::Compare(Relation::GreaterOrEqual, false)),
    ("CHAR-EQUAL", Op::Compare(Relation::Equal, true)),
    ("CHAR-NOT-EQUAL", Op::Compare(Relation::Distinct, true)),
    ("CHAR-LESSP", Op::Compare(Relation::Less, true)),
    ("CHAR-GREATERP", Op::Compare(Relation::Greater, true)),
    (
        "CHAR-NOT-GREATERP",
        Op::Compare(Relation::LessOrEqual, true),
    ),
    (
        "CHAR-NOT-LESSP",
        Op::Compare(Relation::GreaterOrEqual, true),
    ),
    ("DIGIT-CHAR", Op::Digit),
    ("DIGIT-CHAR-P", Op::DigitValue),
];

pub(super) fn setf_primitive(accessor: &str) -> Option<Word> {
    let setter = match accessor {
        "CHAR" => "(SETF CHAR)",
        "SCHAR" => "(SETF SCHAR)",
        _ => return None,
    };
    super::Primitive::named(setter)
}

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Index { .. } => count == 2,
            Self::SetIndex { .. } => count == 3,
            Self::Compare(..) => count >= 1,
            Self::Digit | Self::DigitValue => (1..=2).contains(&count),
            _ => count == 1,
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
    let first = frame.get(args.start);
    Ok(match op {
        Op::Character => {
            if first.as_character().is_some() {
                return Ok(first);
            }
            let name = runtime.string_designator_text(first).ok_or_else(|| {
                Fault::character_designator(runtime, 
                    first,
                    EvalError::new(at, "expected a character designator"),
                )
            })?;
            let mut characters = name.chars();
            let character = characters
                .next()
                .filter(|_| characters.as_str().is_empty())
                .ok_or_else(|| {
                    Fault::character_designator(runtime, 
                        first,
                        EvalError::new(at, "a character designator must denote one character"),
                    )
                })?;
            Word::character(character)
        }
        Op::Characterp => boolean(first.as_character().is_some()),
        Op::Index { simple } => {
            require_string(runtime, first, simple, at)?;
            let index = non_negative_integer(runtime, frame.get(args.start + 1), at)?;
            runtime
                .heap
                .array_slot(first, index)
                .map_err(|error| EvalError::out_of_memory(at, error))?
                .ok_or_else(|| outside_string(at))?
        }
        Op::SetIndex { simple } => {
            let string = frame.get(args.start + 1);
            require_string(runtime, string, simple, at)?;
            let index = non_negative_integer(runtime, frame.get(args.start + 2), at)?;
            require_character(runtime, first, at)?;
            if !runtime
                .heap
                .set_array_slot(string, index, first)
                .map_err(|error| EvalError::out_of_memory(at, error))?
            {
                return Err(outside_string(at));
            }
            first
        }
        Op::Code => Word::fixnum(require_character(runtime, first, at)? as isize).unwrap(),
        Op::FromCode => {
            let code = first
                .as_fixnum()
                .and_then(|code| u32::try_from(code).ok())
                .filter(|&code| code < character::CODE_LIMIT)
                .ok_or_else(|| {
                    Fault::character_code(runtime, 
                        first,
                        EvalError::new(at, "expected a code below CHAR-CODE-LIMIT"),
                    )
                })?;
            char::from_u32(code).map_or(Word::NIL, Word::character)
        }
        Op::Name => {
            let name = character::name(require_character(runtime, first, at)?);
            return runtime
                .heap
                .string(&name)
                .map_err(|error| EvalError::out_of_memory(at, error).into());
        }
        Op::FromName => {
            let name = runtime.string_designator_text(first).ok_or_else(|| {
                Fault::string_designator(runtime, first, EvalError::new(at, "expected a string designator"))
            })?;
            character::named(&name).map_or(Word::NIL, Word::character)
        }
        Op::Downcase => Word::character(simple_lowercase(require_character(runtime, first, at)?)),
        Op::Upcase => Word::character(simple_uppercase(require_character(runtime, first, at)?)),
        Op::Predicate(predicate) => {
            let character = require_character(runtime, first, at)?;
            boolean(match predicate {
                Predicate::Standard => character::is_standard(character),
                Predicate::Graphic => !character.is_control(),
                Predicate::Alpha => character.is_alphabetic(),
                Predicate::Alphanumeric => character::is_alphanumeric(character),
                Predicate::Upper => simple_lowercase(character) != character,
                Predicate::Lower => simple_uppercase(character) != character,
                Predicate::BothCase => {
                    simple_lowercase(character) != character
                        || simple_uppercase(character) != character
                }
            })
        }
        Op::Compare(relation, caseless) => {
            compare(runtime, args.map(|index| frame.get(index)), relation, caseless, at)?
        }
        Op::Digit => {
            let weight = non_negative_integer(runtime, first, at)?;
            let radix = radix(runtime, frame, args, at)?;
            u32::try_from(weight)
                .ok()
                .and_then(|weight| char::from_digit(weight, radix))
                .map_or(Word::NIL, |digit| {
                    Word::character(digit.to_ascii_uppercase())
                })
        }
        Op::DigitValue => {
            let character = require_character(runtime, first, at)?;
            let radix = radix(runtime, frame, args, at)?;
            character
                .to_digit(radix)
                .map_or(Word::NIL, |digit| Word::fixnum(digit as isize).unwrap())
        }
    })
}

fn require_character(runtime: &Runtime, word: Word, at: usize) -> Result<char, Fault> {
    word.as_character().ok_or_else(|| {
        Fault::type_error(
            runtime,
            word,
            "CHARACTER",
            EvalError::new(at, "expected a character"),
        )
    })
}

fn non_negative_integer(runtime: &Runtime, word: Word, at: usize) -> Result<usize, Fault> {
    word.as_fixnum()
        .and_then(|integer| usize::try_from(integer).ok())
        .ok_or_else(|| {
            Fault::non_negative_integer(
                runtime,
                word,
                EvalError::new(at, "expected a non-negative integer"),
            )
        })
}

fn require_string(runtime: &Runtime, string: Word, simple: bool, at: usize) -> Result<(), Fault> {
    if runtime.heap.array_info(string).is_some_and(|info| {
        info.rank == 1
            && info.element == crate::heap::ArrayElement::Character
            && (!simple || info.simple())
    }) {
        return Ok(());
    }
    let expected = if simple { "SIMPLE-STRING" } else { "STRING" };
    Err(Fault::type_error(runtime, 
        string,
        expected,
        EvalError::new(at, format!("expected {expected}")),
    ))
}

fn outside_string(at: usize) -> Fault {
    Fault::plain(
        Class::Error,
        EvalError::new(at, "character index is outside the string"),
    )
}

fn radix(runtime: &Runtime, frame: &RootSlots, args: Range<usize>, at: usize) -> Result<u32, Fault> {
    if args.len() == 1 {
        return Ok(10);
    }
    let position = frame.get(args.start + 1);
    position
        .as_fixnum()
        .filter(|radix| (2..=36).contains(radix))
        .map(|radix| radix as u32)
        .ok_or_else(|| {
            Fault::integer_range(
                runtime,
                position,
                2,
                36,
                EvalError::new(at, "expected a radix from 2 to 36"),
            )
        })
}

fn compare(
    runtime: &Runtime,
    words: impl Iterator<Item = Word>,
    relation: Relation,
    caseless: bool,
    at: usize,
) -> Result<Word, Fault> {
    let characters = words.map(|word| {
        require_character(runtime, word, at).map(|ch| if caseless { simple_lowercase(ch) } else { ch })
    });
    if matches!(relation, Relation::Distinct) {
        let mut seen = HashSet::new();
        let mut included = true;
        for character in characters {
            included &= seen.insert(character?);
        }
        return Ok(boolean(included));
    }
    let mut previous = None;
    let mut matched = true;
    for character in characters {
        let current = character?;
        if let Some(left) = previous {
            matched &= match relation {
                Relation::Equal => left == current,
                Relation::Less => left < current,
                Relation::Greater => left > current,
                Relation::LessOrEqual => left <= current,
                Relation::GreaterOrEqual => left >= current,
                Relation::Distinct => unreachable!("distinct compares every pair"),
            };
        }
        previous = Some(current);
    }
    Ok(boolean(matched))
}

fn boolean(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}
