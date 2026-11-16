use std::rc::Rc;
use std::sync::Arc;

use zkcl_pattern::{
    FailedSuffixes, Input, InputDomain, Match, MatchMode, Matcher, Outcome, Program, Rule, Set,
    parse_regex,
};

use crate::condition::{Fault, named_word};
use crate::heap::{ArrayElement, ListSpine, Word};
use crate::symbol::{Name, Package};
use crate::{EvalError, NativeFunction, Root, Runtime};

type Callback = for<'rt> fn(&'rt Runtime, &[Root<'rt>], usize) -> Result<Vec<Root<'rt>>, Fault>;

pub(super) const NATIVES: &[(&str, Callback)] = &[
    ("%LEX-COMPILE", compile),
    ("%LEX-COVERAGE", coverage),
    ("%LEX-SCANNER-P", scanner_p),
    ("%LEX-RUN", run),
    ("%LEX-CURSOR", cursor),
    ("%CURSOR-APPEND", cursor_append),
    ("%CURSOR-PROBE", cursor_probe),
    ("%CURSOR-COMMIT", cursor_commit),
    ("%CURSOR-RESET", cursor_reset),
    ("%UTF8-ENCODE", utf8_encode),
    ("%UTF8-DECODE", utf8_decode),
    ("%TEXT-NORMALIZE", normalize),
    ("%TEXT-CASE-FOLD", case_fold),
    ("%TEXT-SEGMENT", segment),
    ("%UNICODE-PROPERTY-P", property),
];

pub(super) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    for &(name, callback) in NATIVES {
        let symbol = runtime.intern_package(Package::ZK, name, 0)?.0;
        let mut natives = runtime.natives.borrow_mut();
        let function = Word::native(natives.len());
        natives.push(Arc::new(callback) as Arc<NativeFunction>);
        runtime.heap.set_symbol_cell(symbol, true, function);
    }
    Ok(())
}

fn error(at: usize, message: impl Into<String>) -> Fault {
    EvalError::new(at, message).into()
}

#[derive(Debug, thiserror::Error)]
#[error("lexical rule {ordinal}: {}", .source.message())]
struct LexicalRuleError {
    ordinal: usize,
    source: EvalError,
}

fn keyword(runtime: &Runtime, value: Word) -> Option<String> {
    named_word(runtime, value, Package::KEYWORD)
}

fn keyword_value(runtime: &Runtime, name: &str, at: usize) -> Result<Word, Fault> {
    Ok(runtime.intern(&Name::keyword(name), at)?)
}

fn size(runtime: &Runtime, value: Word, at: usize) -> Result<usize, Fault> {
    value
        .as_fixnum()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            Fault::non_negative_integer(
                runtime,
                value,
                EvalError::new(at, "expected a non-negative index"),
            )
        })
}

fn domain(runtime: &Runtime, word: Word, at: usize) -> Result<InputDomain, Fault> {
    match keyword(runtime, word).as_deref() {
        Some("TEXT") => Ok(InputDomain::Text),
        Some("BYTES") => Ok(InputDomain::Bytes),
        _ => Err(error(at, "lexical input domain must be :text or :bytes")),
    }
}

fn mode(runtime: &Runtime, word: Word, at: usize) -> Result<MatchMode, Fault> {
    match keyword(runtime, word).as_deref() {
        Some("FULL") => Ok(MatchMode::Full),
        Some("PREFIX") => Ok(MatchMode::Prefix),
        Some("LONGEST-PREFIX") => Ok(MatchMode::LongestPrefix),
        Some("SEARCH") => Ok(MatchMode::Search),
        _ => Err(error(at, "unknown lexical match mode")),
    }
}

fn list(runtime: &Runtime, value: Word, at: usize) -> Result<Vec<Word>, Fault> {
    let mut result = Vec::new();
    let mut list = ListSpine::new(value);
    while let Some((value, _)) = list
        .next(&runtime.heap)
        .map_err(|_| error(at, "cyclic lexical rule"))?
    {
        result.push(value);
    }
    if list.tail() != Word::NIL {
        Err(error(at, "expected a proper lexical rule list"))
    } else {
        Ok(result)
    }
}

fn string(runtime: &Runtime, value: Word, at: usize) -> Result<String, Fault> {
    runtime
        .heap
        .string_contents(value)
        .ok_or_else(|| Fault::type_error(runtime, value, "STRING", EvalError::new(at, "expected text")))
}

fn property_name(runtime: &Runtime, value: Word, at: usize) -> Result<String, Fault> {
    if let Some(name) = Name::from_word(runtime, value) {
        Ok(name.spelling().to_owned())
    } else {
        string(runtime, value, at)
    }
}

fn unicode_property(
    runtime: &Runtime,
    name: Word,
    value: Option<Word>,
    at: usize,
) -> Result<zkcl_pattern::unicode::Property, Fault> {
    let name = property_name(runtime, name, at)?;
    let value = value
        .map(|value| property_name(runtime, value, at))
        .transpose()?;
    zkcl_pattern::unicode::Property::parse(&name, value.as_deref())
        .map_err(|cause| EvalError::from_error(at, cause).into())
}

fn octet(runtime: &Runtime, value: Word, at: usize) -> Result<u8, Fault> {
    value
        .as_fixnum()
        .and_then(|value| u8::try_from(value).ok())
        .ok_or_else(|| {
            Fault::integer_range(runtime, value, 0, 255, EvalError::new(at, "expected an octet"))
        })
}

fn rule(
    runtime: &Runtime,
    word: Word,
    domain: InputDomain,
    at: usize,
    depth: usize,
    groups: &mut usize,
) -> Result<Rule, Fault> {
    if depth > 128 {
        return Err(error(at, "lexical rule nesting exceeds 128 forms"));
    }
    if let Some(text) = runtime.heap.string_contents(word) {
        return if domain == InputDomain::Text {
            Ok(Rule::Literal(text.chars().map(u32::from).collect()))
        } else {
            Err(error(at, "text literal in byte rule; use (:octets ...)"))
        };
    }
    let parts = list(runtime, word, at)?;
    let Some((&head, tail)) = parts.split_first() else {
        return Err(error(at, "empty lexical rule"));
    };
    let mut child = |value| rule(runtime, value, domain, at, depth + 1, groups);
    let bound = |value: Word| match domain {
        InputDomain::Text => value
            .as_character()
            .map(u32::from)
            .ok_or_else(|| error(at, "text range needs character endpoints")),
        InputDomain::Bytes => octet(runtime, value, at).map(u32::from),
    };
    Ok(match (keyword(runtime, head).as_deref(), tail) {
        (Some("RE"), [value]) => parse_regex(domain, &string(runtime, *value, at)?)
            .map_err(|cause| EvalError::from_error(at, cause))?,
        (Some("OCTETS"), bytes) if domain == InputDomain::Bytes => Rule::Literal(
            bytes
                .iter()
                .map(|&value| octet(runtime, value, at).map(u32::from))
                .collect::<Result<_, _>>()?,
        ),
        (Some("RANGE"), [low, high]) => {
            let (low, high) = (bound(*low)?, bound(*high)?);
            if low > high {
                return Err(error(at, "descending lexical range"));
            }
            Rule::Set(Set::range(low, high))
        }
        (Some("PROPERTY"), [name] | [name, _]) if domain == InputDomain::Text => Rule::Set(
            Set::property(unicode_property(runtime, *name, tail.get(1).copied(), at)?),
        ),
        (Some("SEQ"), parts) => Rule::Sequence(
            parts
                .iter()
                .copied()
                .map(&mut child)
                .collect::<Result<_, _>>()?,
        ),
        (Some("OR"), parts) => Rule::Choice(
            parts
                .iter()
                .copied()
                .map(&mut child)
                .collect::<Result<_, _>>()?,
        ),
        (Some("*"), [body]) => child(*body)?.repeat(0, None, true),
        (Some("+"), [body]) => child(*body)?.repeat(1, None, true),
        (Some("?"), [body]) => child(*body)?.repeat(0, Some(1), true),
        (Some("REPEAT"), [min, max, body]) => {
            let min = size(runtime, *min, at)?;
            let max = if named_word(runtime, *max, Package::COMMON_LISP).as_deref() == Some("*") {
                None
            } else {
                Some(size(runtime, *max, at)?)
            };
            child(*body)?.repeat(min, max, true)
        }
        (Some("CAPTURE"), [ordinal, body, style]) => {
            let group = size(runtime, *ordinal, at)?;
            let body = child(*body)?;
            match keyword(runtime, *style).as_deref() {
                Some("VALUE" | "SPAN") => {}
                Some("UNIT") if body.width() == (1, Some(1)) => {}
                Some("UNIT") => {
                    return Err(error(
                        at,
                        ":as :unit requires a capture that consumes one unit",
                    ));
                }
                _ => return Err(error(at, "unknown lexical capture style")),
            }
            *groups = (*groups).max(group + 1);
            Rule::Capture {
                group,
                body: Box::new(body),
            }
        }
        _ => {
            return Err(error(
                at,
                "unknown, malformed, or mixed-domain lexical rule",
            ));
        }
    })
}

fn compile<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [rules, input, strategy] = args else {
        return Err(error(at, "expected lexical rules, domain and mode"));
    };
    let domain = domain(runtime, input.slot.get(), at)?;
    let mode = mode(runtime, strategy.slot.get(), at)?;
    let rules = list(runtime, rules.slot.get(), at)?
        .into_iter()
        .enumerate()
        .map(|(ordinal, form)| {
            let mut groups = 0;
            let rule = rule(runtime, form, domain, at, 0, &mut groups).map_err(|cause| {
                EvalError::from_error(
                    at,
                    LexicalRuleError {
                        ordinal: ordinal + 1,
                        source: cause.host,
                    },
                )
            })?;
            Ok((rule, groups))
        })
        .collect::<Result<Vec<_>, Fault>>()?;
    let program =
        Program::compile(domain, mode, &rules).map_err(|cause| EvalError::from_error(at, cause))?;
    Ok(vec![
        runtime
            .foreign_owned(Rc::new(program))
            .map_err(|cause| EvalError::out_of_memory(at, cause))?,
    ])
}

fn program(runtime: &Runtime, handle: &Root<'_>, at: usize) -> Result<Rc<Program>, Fault> {
    handle
        .with_owned_foreign_ref::<Rc<Program>, _>(runtime, Rc::clone)
        .map_err(|_| error(at, "expected a compiled lexical program"))
}

fn scanner_p<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [handle] = args else {
        return Err(error(at, "expected a compiled lexical program"));
    };
    Ok(vec![runtime.root(
        if program(runtime, handle, at)?.is_scanner() {
            Word::T
        } else {
            Word::NIL
        },
    )])
}

fn vec_from<'rt>(
    runtime: &'rt Runtime,
    length: usize,
    values: impl Fn(usize) -> Word,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    runtime
        .heap
        .array(&[length], ArrayElement::General, false, None, values)
        .map(|word| runtime.root(word))
        .map_err(|cause| EvalError::out_of_memory(at, cause).into())
}

fn number<'rt>(runtime: &'rt Runtime, value: usize, at: usize) -> Result<Root<'rt>, Fault> {
    runtime
        .integer_from(value)
        .map_err(|cause| EvalError::out_of_memory(at, cause).into())
}

fn number_vector<'rt>(
    runtime: &'rt Runtime,
    values: &[Option<usize>],
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let roots = runtime.root_frame(values.len());
    for (index, value) in values.iter().enumerate() {
        roots.set(
            index,
            match value {
                Some(value) => number(runtime, *value, at)?.slot.get(),
                None => Word::NIL,
            },
        );
    }
    vec_from(runtime, values.len(), |index| roots.get(index), at)
}

fn match_vector<'rt>(runtime: &'rt Runtime, matched: Match, at: usize) -> Result<Root<'rt>, Fault> {
    let mut values = vec![
        Some(matched.rule),
        Some(matched.range.start),
        Some(matched.range.end),
    ];
    for capture in matched.groups {
        if let Some(range) = capture {
            values.extend([Some(range.start), Some(range.end)]);
        } else {
            values.extend([None, None]);
        }
    }
    number_vector(runtime, &values, at)
}

fn coverage<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [handle] = args else {
        return Err(error(at, "expected a compiled lexical program"));
    };
    let program = program(runtime, handle, at)?;
    let proof = program.coverage();
    let status = match proof.exhaustive {
        Some(true) => "TOTAL",
        Some(false) => "MISSING",
        None => "UNKNOWN",
    };
    let status = runtime.root(keyword_value(runtime, status, at)?);
    let witness = match (program.domain(), proof.witness) {
        (_, None) => runtime.nil(),
        (InputDomain::Text, Some(units)) => {
            let text: String = units
                .into_iter()
                .map(|unit| char::from_u32(unit).expect("text witness"))
                .collect();
            runtime
                .make_string(&text)
                .map_err(|cause| EvalError::out_of_memory(at, cause))?
        }
        (InputDomain::Bytes, Some(units)) => {
            let bytes: Vec<_> = units.into_iter().map(|unit| unit as u8).collect();
            runtime.root(
                runtime
                    .heap
                    .octet_vector(&bytes)
                    .map_err(|cause| EvalError::out_of_memory(at, cause))?,
            )
        }
    };
    let selectable = if let Some(selected) = proof.selectable {
        vec_from(
            runtime,
            selected.len(),
            |index| if selected[index] { Word::T } else { Word::NIL },
            at,
        )?
    } else {
        runtime.nil()
    };
    Ok(vec![status, witness, selectable])
}

fn run<'rt>(runtime: &'rt Runtime, args: &[Root<'rt>], at: usize) -> Result<Vec<Root<'rt>>, Fault> {
    let [handle, input, start, end] = args else {
        return Err(error(at, "expected program and lexical input range"));
    };
    let program = program(runtime, handle, at)?;
    let start = size(runtime, start.slot.get(), at)?;
    let end = size(runtime, end.slot.get(), at)?;
    let count = end
        .checked_sub(start)
        .ok_or_else(|| error(at, "inverted lexical input range"))?;
    let matched = match program.domain() {
        InputDomain::Text => runtime
            .heap
            .with_array_character_slice(input.slot.get(), start, count, |text| {
                program.run_str_with(text, || runtime.check_execution_safepoint(at))
            })
            .ok_or_else(|| error(at, "expected a string range"))??,
        InputDomain::Bytes => runtime
            .heap
            .with_array_octet_slice(input.slot.get(), start, count, |bytes| {
                program.run_bytes_with(bytes, || runtime.check_execution_safepoint(at))
            })
            .ok_or_else(|| error(at, "expected a specialized octet vector range"))??,
    };
    Ok(vec![match matched {
        Some(matched) => match_vector(runtime, matched, at)?,
        None => runtime.nil(),
    }])
}

struct Cursor {
    domain: InputDomain,
    base: usize,
    position: usize,
    left: Option<u32>,
    units: Vec<u32>,
    breaks: Vec<bool>,
    graphemes: zkcl_pattern::unicode::Graphemes,
    program: Option<Rc<Program>>,
    active: Option<Matcher>,
    failed_suffixes: FailedSuffixes,
}

impl Cursor {
    fn commit(&mut self, end: usize) -> bool {
        if end < self.position || end - self.base > self.units.len() {
            return false;
        }
        self.position = end;
        self.active = None;
        let skip = end - self.base;
        if skip != 0 && skip >= self.units.len() - skip {
            self.left = Some(self.units[skip - 1]);
            self.units.drain(..skip);
            if self.domain == InputDomain::Text {
                self.breaks.drain(..skip);
            }
            self.base = end;
            self.failed_suffixes.discard_before(end);
        }
        true
    }
}

fn cursor<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [input] = args else {
        return Err(error(at, "expected a lexical input domain"));
    };
    let cursor = Cursor {
        domain: domain(runtime, input.slot.get(), at)?,
        base: 0,
        position: 0,
        left: None,
        units: Vec::new(),
        breaks: Vec::new(),
        graphemes: Default::default(),
        active: None,
        program: None,
        failed_suffixes: FailedSuffixes::default(),
    };
    Ok(vec![
        runtime
            .foreign_owned(cursor)
            .map_err(|cause| EvalError::out_of_memory(at, cause))?,
    ])
}

fn with_cursor<T>(
    runtime: &Runtime,
    value: &Root<'_>,
    at: usize,
    function: impl FnOnce(&mut Cursor) -> Result<T, Fault>,
) -> Result<T, Fault> {
    value
        .with_owned_foreign_mut::<Cursor, _>(runtime, function)
        .map_err(|_| error(at, "expected a lexical cursor"))?
}

fn cursor_append<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [handle, input, start, end] = args else {
        return Err(error(at, "expected a cursor and chunk range"));
    };
    with_cursor(runtime, handle, at, |cursor| {
        let start = size(runtime, start.slot.get(), at)?;
        let count = size(runtime, end.slot.get(), at)?
            .checked_sub(start)
            .ok_or_else(|| error(at, "inverted lexical input range"))?;
        match cursor.domain {
            InputDomain::Text => runtime
                .heap
                .with_array_character_slice(input.slot.get(), start, count, |text| {
                    cursor.units.reserve(count);
                    cursor.breaks.reserve(count);
                    for character in text.chars() {
                        cursor.breaks.push(cursor.graphemes.push(character));
                        cursor.units.push(u32::from(character));
                    }
                })
                .ok_or_else(|| error(at, "expected a string range")),
            InputDomain::Bytes => runtime
                .heap
                .with_array_octet_slice(input.slot.get(), start, count, |bytes| {
                    cursor.units.extend(bytes.iter().copied().map(u32::from));
                })
                .ok_or_else(|| error(at, "expected a specialized octet vector range")),
        }
    })?;
    Ok(Vec::new())
}

fn cursor_probe<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [handle, program_handle, final_input] = args else {
        return Err(error(at, "expected a cursor, program and EOF flag"));
    };
    let program = program(runtime, program_handle, at)?;
    let eof = !final_input.is_nil();
    let (status, matched) = with_cursor(runtime, handle, at, |cursor| {
        if cursor.domain != program.domain() || !program.is_scanner() {
            return Err(error(
                at,
                "scanner rules must be same-domain, advancing prefix rules",
            ));
        }
        if cursor.position == cursor.base + cursor.units.len() {
            return Ok((if eof { "EOF" } else { "NEED-MORE" }, None));
        }
        if cursor
            .program
            .as_ref()
            .is_none_or(|previous| !Rc::ptr_eq(previous, &program))
        {
            cursor.program = Some(program.clone());
            cursor.active = None;
            cursor.failed_suffixes = FailedSuffixes::default();
        }
        let matcher = cursor
            .active
            .get_or_insert_with(|| program.matcher(cursor.position));
        let outcome = matcher.resume_with(
            &program,
            Input {
                base: cursor.base,
                units: &cursor.units,
                left: cursor.left,
                graphemes: &cursor.breaks,
                eof,
            },
            Some(&mut cursor.failed_suffixes),
            || runtime.check_execution_safepoint(at),
        );
        if !matches!(outcome, Ok(Outcome::NeedMore)) {
            cursor.active = None;
        }
        match outcome? {
            Outcome::NeedMore => Ok(("NEED-MORE", None)),
            Outcome::Miss => Ok(("MISS", None)),
            Outcome::Match(matched) => Ok(("MATCH", Some(matched))),
        }
    })?;
    let status = runtime.root(keyword_value(runtime, status, at)?);
    Ok(vec![
        status,
        matched
            .map(|matched| match_vector(runtime, matched, at))
            .transpose()?
            .unwrap_or_else(|| runtime.nil()),
    ])
}

fn cursor_commit<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [handle, end] = args else {
        return Err(error(at, "expected a cursor and commit position"));
    };
    with_cursor(runtime, handle, at, |cursor| {
        cursor
            .commit(size(runtime, end.slot.get(), at)?)
            .then_some(())
            .ok_or_else(|| error(at, "commit is outside buffered input"))
    })?;
    Ok(Vec::new())
}

fn cursor_reset<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [handle] = args else {
        return Err(error(at, "expected a cursor"));
    };
    with_cursor(runtime, handle, at, |cursor| {
        cursor.active = None;
        Ok(())
    })?;
    Ok(Vec::new())
}

fn utf8_encode<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [text, start, end] = args else {
        return Err(error(at, "expected a character input range"));
    };
    let start = size(runtime, start.slot.get(), at)?;
    let count = size(runtime, end.slot.get(), at)?
        .checked_sub(start)
        .ok_or_else(|| error(at, "inverted character range"))?;
    let text = runtime
        .heap
        .with_array_character_slice(text.slot.get(), start, count, |text| {
            text.as_bytes().to_vec()
        })
        .ok_or_else(|| error(at, "expected a string range"))?;
    let output = runtime
        .heap
        .octet_vector(&text)
        .map_err(|cause| EvalError::out_of_memory(at, cause))?;
    Ok(vec![runtime.root(output)])
}

fn utf8_decode<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [input, start, end, eof, replace] = args else {
        return Err(error(
            at,
            "expected an octet range, EOF and replacement flags",
        ));
    };
    let start = size(runtime, start.slot.get(), at)?;
    let count = size(runtime, end.slot.get(), at)?
        .checked_sub(start)
        .ok_or_else(|| error(at, "inverted octet range"))?;
    let decoded = runtime
        .heap
        .with_array_octet_slice(input.slot.get(), start, count, |bytes| {
            zkcl_pattern::unicode::decode(bytes, !eof.is_nil(), !replace.is_nil())
        })
        .ok_or_else(|| error(at, "expected a specialized octet vector range"))?;
    let text = runtime
        .make_string(&decoded.text)
        .map_err(|cause| EvalError::out_of_memory(at, cause))?;
    let byte_boundaries = decoded
        .byte_boundaries
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();
    let byte_boundaries = number_vector(runtime, &byte_boundaries, at)?;
    let consumed = number(runtime, decoded.consumed_bytes, at)?;
    let error_end = decoded
        .invalid_byte_range
        .map(|range| number(runtime, range.end, at))
        .transpose()?
        .unwrap_or_else(|| runtime.nil());
    Ok(vec![text, byte_boundaries, consumed, error_end])
}

fn normalize<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [input, form] = args else {
        return Err(error(at, "expected text and a normalization form"));
    };
    let form = match keyword(runtime, form.slot.get()).as_deref() {
        Some("NFC") => zkcl_pattern::unicode::Normalization::Nfc,
        Some("NFD") => zkcl_pattern::unicode::Normalization::Nfd,
        Some("NFKC") => zkcl_pattern::unicode::Normalization::Nfkc,
        Some("NFKD") => zkcl_pattern::unicode::Normalization::Nfkd,
        _ => return Err(error(at, "unknown normalization form")),
    };
    let text = zkcl_pattern::unicode::normalize(&string(runtime, input.slot.get(), at)?, form);
    Ok(vec![
        runtime
            .make_string(&text)
            .map_err(|cause| EvalError::out_of_memory(at, cause))?,
    ])
}

fn case_fold<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [input] = args else {
        return Err(error(at, "expected text to case fold"));
    };
    let text = zkcl_pattern::unicode::case_fold(&string(runtime, input.slot.get(), at)?);
    Ok(vec![
        runtime
            .make_string(&text)
            .map_err(|cause| EvalError::out_of_memory(at, cause))?,
    ])
}

fn segment<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [input, mode] = args else {
        return Err(error(at, "expected text and a segmentation kind"));
    };
    let words = match keyword(runtime, mode.slot.get()).as_deref() {
        Some("WORD") => true,
        Some("GRAPHEME") => false,
        _ => return Err(error(at, "expected :word or :grapheme")),
    };
    let edges = zkcl_pattern::unicode::segment(&string(runtime, input.slot.get(), at)?, words);
    Ok(vec![number_vector(
        runtime,
        &edges.into_iter().map(Some).collect::<Vec<_>>(),
        at,
    )?])
}

fn property<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [character, name, value @ ..] = args else {
        return Err(error(
            at,
            "expected a character, property and optional value",
        ));
    };
    if value.len() > 1 {
        return Err(error(at, "expected at most one property value"));
    }
    let character = character
        .slot
        .get()
        .as_character()
        .ok_or_else(|| error(at, "expected a Unicode character"))?;
    let property = unicode_property(
        runtime,
        name.slot.get(),
        value.first().map(|value| value.slot.get()),
        at,
    )?;
    Ok(vec![runtime.root(if property.contains(character) {
        Word::T
    } else {
        Word::NIL
    })])
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::Runtime;

    #[test]
    fn lexical_compile_preserves_the_regex_error_in_the_cause_chain() {
        let runtime = Runtime::start().unwrap();
        let Err(error) = runtime.eval(r#"(zk::%lex-compile '((:re "雪]")) :text :full)"#) else {
            panic!("invalid regular expression compiled successfully");
        };
        assert!(error.message().contains("lexical rule 1:"), "{error}");

        let mut cause = error.source();
        while let Some(error) = cause {
            if let Some(regex_error) = error.downcast_ref::<zkcl_pattern::RegexError>() {
                assert_eq!(regex_error.offset, 3);
                return;
            }
            cause = error.source();
        }
        panic!("missing regex parse error in the cause chain: {error:?}");
    }
}
