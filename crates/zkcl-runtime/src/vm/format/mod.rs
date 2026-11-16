use std::collections::HashSet;
use std::ops::Range;

use super::printer::PrintContext;
use super::{printer, proper_list, streams, symbol_value};
use crate::condition::{Class, Fault};
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::package::Package;
use crate::symbol::Name;
use crate::{EvalError, Runtime};

mod control_string;
mod numeric;

#[path = "../../pprint/mod.rs"]
pub(crate) mod pprint;

use control_string::{
    ControlPart, DirectiveGroup, DirectiveSection, FormatDirective, GroupKind, PrefixParameter,
    parse_control_string,
};

const FORMAT_NESTING_LIMIT: usize = 256;

/// Function names routed through the FORMAT implementation.  The main
/// primitive table holds the long-standing entries; this table is for
/// additional printer entry points such as FORMATTER and PPRINT.
pub(super) const PRIMITIVES: &[(&str, OutputPrimitive)] = &[
    ("FORMATTER", OutputPrimitive::Formatter),
    ("%FORMATTER-CALL", OutputPrimitive::FormatterCall),
    (
        "%PPRINT-IMPL",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::PrintToStream),
    ),
    (
        "%PPRINT-DISPATCH-VALUES",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::DispatchValues),
    ),
    (
        "%PPRINT-BLOCK-START",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::BlockStart),
    ),
    (
        "%PPRINT-BLOCK-END",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::BlockEnd),
    ),
    (
        "%PPRINT-LENGTH-CHECK",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::LengthCheck),
    ),
    (
        "%PRINT-UNREADABLE-OBJECT",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::PrintUnreadable),
    ),
    (
        "PPRINT",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::Pprint),
    ),
    (
        "PPRINT-FILL",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::PprintFill),
    ),
    (
        "PPRINT-LINEAR",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::PprintLinear),
    ),
    (
        "PPRINT-TABULAR",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::PprintTabular),
    ),
    (
        "PPRINT-TAB",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::PprintTab),
    ),
    (
        "PPRINT-INDENT",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::PprintIndent),
    ),
    (
        "PPRINT-NEWLINE",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::PprintNewline),
    ),
    (
        "PPRINT-DISPATCH",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::PprintDispatch),
    ),
    (
        "COPY-PPRINT-DISPATCH",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::CopyPprintDispatch),
    ),
    (
        "SET-PPRINT-DISPATCH",
        OutputPrimitive::Pprint(pprint::PprintPrimitive::SetPprintDispatch),
    ),
    ("WRITE", OutputPrimitive::Write),
];

#[derive(Clone, Copy)]
pub(crate) enum OutputPrimitive {
    Pprint(pprint::PprintPrimitive),
    Write,
    Formatter,
    FormatterCall,
    Format,
    Prin1,
    Princ,
    Print,
    Terpri,
    FreshLine,
}

impl OutputPrimitive {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Pprint(op) => op.accepts(count),
            Self::Write => count >= 1,
            Self::Formatter => count == 1,
            Self::FormatterCall => count >= 2,
            Self::Format => count >= 2,
            Self::Prin1 | Self::Princ | Self::Print => (1..=2).contains(&count),
            Self::Terpri | Self::FreshLine => count <= 1,
        }
    }
}

pub(super) fn run(
    runtime: &PrintContext<'_>,
    frame: &RootSlots,
    op: OutputPrimitive,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    match op {
        OutputPrimitive::Pprint(op) => pprint::run(runtime, frame, op, args, at),
        OutputPrimitive::Write => printer::write(runtime, frame, args, at),
        OutputPrimitive::Formatter => compile_formatter(runtime, frame.get(args.start), at),
        OutputPrimitive::Format | OutputPrimitive::FormatterCall => {
            let return_unused = matches!(op, OutputPrimitive::FormatterCall);
            run_format(runtime, frame, args, return_unused, at)
        }
        OutputPrimitive::Prin1 | OutputPrimitive::Princ | OutputPrimitive::Print => {
            let mode = if matches!(op, OutputPrimitive::Princ) {
                printer::PrinterPrimitive::Princ
            } else {
                printer::PrinterPrimitive::Prin1
            };
            let stream = if args.len() == 2 {
                frame.get(args.start + 1)
            } else {
                Word::NIL
            };
            let stream = runtime.root_slot(pprint::output_stream(runtime, stream, at)?);
            if matches!(op, OutputPrimitive::Print) {
                streams::gray::print_text(runtime, stream.get(), "\n", at)?;
            }
            printer::print_to(runtime, mode, frame.get(args.start), stream.get(), at)?;
            if matches!(op, OutputPrimitive::Print) {
                streams::gray::print_text(runtime, stream.get(), " ", at)?;
            }
            Ok(frame.get(args.start))
        }
        OutputPrimitive::Terpri | OutputPrimitive::FreshLine => {
            let stream = if args.is_empty() {
                Word::NIL
            } else {
                frame.get(args.start)
            };
            if matches!(op, OutputPrimitive::FreshLine)
                && streams::gray::print_column(runtime, stream, at)? == Some(0)
            {
                return Ok(Word::NIL);
            }
            streams::gray::print_text(runtime, stream, "\n", at)?;
            Ok(if matches!(op, OutputPrimitive::FreshLine) {
                Word::T
            } else {
                Word::NIL
            })
        }
    }
}

fn compile_formatter(runtime: &PrintContext<'_>, control: Word, at: usize) -> Result<Word, Fault> {
    let source = runtime
        .heap
        .with_string_slice(control, |control| {
            format!(
                "(lambda (stream &rest arguments) (apply (function :%formatter-call) {} stream arguments))",
                lisp_string(control)
            )
        })
        .ok_or_else(|| format_error(at, "FORMATTER requires a control string"))?;
    bind_formatter_call(runtime, at)?;
    let form = crate::read::read(runtime, &source)
        .map_err(|error| Fault::plain(Class::ProgramError, error))?;
    let Some([_, parameters, body @ ..]) = form.list() else {
        unreachable!("generated formatter lambda");
    };
    let (code, _) = crate::compile::compile_lambda(runtime, parameters, body, at)
        .map_err(|error| Fault::plain(Class::ProgramError, error))?;
    code.instantiate(runtime)
        .map_err(|error| Fault::plain(Class::ProgramError, error))
}

fn run_format(
    runtime: &PrintContext<'_>,
    frame: &RootSlots,
    args: Range<usize>,
    return_unused: bool,
    at: usize,
) -> Result<Word, Fault> {
    // FORMAT takes the destination first. The internal FORMATTER entry point
    // takes the generated lambda's control string first.
    let (destination_slot, control_slot) = if return_unused {
        (args.start + 1, args.start)
    } else {
        (args.start, args.start + 1)
    };
    let control = frame.get(control_slot);
    let values = frame.slice(args.start + 2..args.end);
    let destination = frame.get(destination_slot);
    let wants_string = destination == Word::NIL;
    if let Some(control) = runtime.heap.string_contents(control) {
        // Keep the chosen destination alive across Lisp callbacks, which may
        // also change the binding of *STANDARD-OUTPUT*.
        let stream = runtime.root_slot(format_stream(runtime, destination, at)?);
        let (text, consumed) = execute(runtime, &control, stream.get(), values, at)?;
        if stream.get() != Word::NIL {
            streams::gray::print_text(runtime, stream.get(), &text, at)?;
        }
        if return_unused {
            return remaining_arguments(runtime, values, consumed, at);
        }
        if wants_string {
            return runtime
                .heap
                .string(&text)
                .map_err(|error| EvalError::out_of_memory(at, error).into());
        }
    } else {
        if return_unused {
            return Err(control_string_error(runtime, control, at));
        }
        if !super::is_function(runtime, control) {
            return Err(invalid_format_control(runtime, control, "FORMAT", at));
        }
        if wants_string {
            let target = runtime.root_slot(control);
            let (_, text) = pprint::capture_output(runtime, 0, false, at, |stream| {
                call_with_stream(runtime, target.get(), stream, values)
            })?;
            return runtime
                .heap
                .string(&text)
                .map_err(|error| EvalError::out_of_memory(at, error).into());
        }
        let stream = format_stream(runtime, destination, at)?;
        call_with_stream(runtime, control, stream, values)?;
    }
    Ok(Word::NIL)
}

/// Run the directives on the selected destination and return the buffered
/// text together with the number of arguments consumed.
fn execute(
    runtime: &PrintContext<'_>,
    control: &str,
    destination: Word,
    values: &RootSlots,
    at: usize,
) -> Result<(String, usize), Fault> {
    let control_parts = parse_format_control(control, at)?;
    validate_directives(&control_parts, at)?;
    let _scope = crate::printer::output_scope(destination);
    let interpreter = FormatInterpreter { runtime, at };
    let mut cursor = ArgumentCursor::new(values);
    let mut output = if destination == Word::NIL {
        FormatOutput::new(Some(0))
    } else {
        let destination = runtime.root_slot(destination);
        let mut output =
            FormatOutput::new(Some(pprint::stream_column(runtime, destination.get(), at)?));
        output.preserve_layout_markers =
            pprint::in_block_buffer(destination.get()) && pprint::pretty_enabled(runtime);
        output.destination = Some(destination);
        output
    };
    interpreter.render_parts(&control_parts, &mut cursor, &mut output, 0)?;
    Ok((output.text, cursor.position))
}

fn remaining_arguments(
    runtime: &PrintContext<'_>,
    values: &RootSlots,
    consumed: usize,
    at: usize,
) -> Result<Word, Fault> {
    let tail = runtime.root_frame(1);
    for index in (consumed..values.len()).rev() {
        let pair = runtime
            .heap
            .cons(|| (values.get(index), tail.get(0)))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        tail.set(0, pair);
    }
    Ok(tail.get(0))
}

fn root_words(runtime: &Runtime, words: Vec<Word>) -> RootFrame {
    let roots = runtime.root_frame(words.len());
    for (index, word) in words.into_iter().enumerate() {
        roots.set(index, word);
    }
    roots
}

/// Bind the internal FORMATTER entry point used by generated formatter lambdas.
fn bind_formatter_call(runtime: &PrintContext<'_>, at: usize) -> Result<(), Fault> {
    let symbol = runtime
        .intern(&Name::keyword("%FORMATTER-CALL"), at)
        .map_err(|error| Fault::plain(Class::ProgramError, error))?;
    let primitive = super::Primitive::named("%FORMATTER-CALL").expect("formatter call primitive");
    runtime.heap.set_symbol_cell(symbol, true, primitive);
    Ok(())
}

fn format_stream(runtime: &PrintContext<'_>, destination: Word, at: usize) -> Result<Word, Fault> {
    if destination == Word::T {
        let variable = runtime
            .find_common_lisp("*STANDARD-OUTPUT*")
            .expect("standard stream initialized");
        return symbol_value(runtime, variable, at);
    }
    if runtime.heap.is_string(destination) {
        let destination = runtime.root_slot(destination);
        return streams::make_output_to_string(runtime, || destination.get(), at);
    }
    Ok(destination)
}

fn invalid_format_control(runtime: &Runtime, value: Word, operator: &str, at: usize) -> Fault {
    Fault::format_control(
        runtime,
        value,
        EvalError::new(
            at,
            format!("{operator} expects a control string or function"),
        ),
    )
}

fn control_string_error(runtime: &Runtime, value: Word, at: usize) -> Fault {
    Fault::type_error(
        runtime,
        value,
        "STRING",
        EvalError::new(at, "FORMAT expects a control string"),
    )
}

fn lisp_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        if matches!(character, '"' | '\\') {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

/// Call a formatter function with the output stream followed by ARGUMENTS.
fn call_with_stream<'rt>(
    runtime: &'rt PrintContext<'rt>,
    target: Word,
    stream: Word,
    values: &RootSlots,
) -> Result<Vec<crate::Root<'rt>>, Fault> {
    let target = runtime.root(target);
    let stream = runtime.root(stream);
    let roots: Vec<_> = (0..values.len())
        .map(|index| runtime.root(values.get(index)))
        .collect();
    let mut arguments: Vec<&_> = Vec::with_capacity(roots.len() + 1);
    arguments.push(&stream);
    arguments.extend(roots.iter());
    runtime.invoke(&target, &arguments, 0)
}

fn parse_format_control(text: &str, at: usize) -> Result<Vec<ControlPart<'_>>, Fault> {
    parse_control_string(text)
        .map_err(|error| Fault::plain(Class::ProgramError, EvalError::from_error(at, error)))
}

fn format_error(at: usize, message: impl Into<String>) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

struct FormatOutput {
    destination: Option<RootSlot>,
    text: String,
    column: Option<usize>,
    /// The destination is a pretty-printer buffer and markers are meaningful.
    preserve_layout_markers: bool,
    /// Literal spaces in control strings become fill newlines (close ~@>).
    add_fill_breaks: bool,
}

impl FormatOutput {
    fn new(column: Option<usize>) -> Self {
        Self {
            destination: None,
            text: String::new(),
            column,
            preserve_layout_markers: false,
            add_fill_breaks: false,
        }
    }

    /// Deliver preceding text before handing the destination to Lisp. Fields
    /// that must be measured or transformed use a stream at the field's column.
    fn with_stream<T>(
        &mut self,
        runtime: &PrintContext<'_>,
        at: usize,
        call: impl FnOnce(Word) -> Result<T, Fault>,
    ) -> Result<T, Fault> {
        if let Some(stream) = &self.destination {
            if !self.text.is_empty() {
                streams::gray::print_text(runtime, stream.get(), &self.text, at)?;
                self.text.clear();
            }
            let result = call(stream.get())?;
            self.column = Some(pprint::stream_column(runtime, stream.get(), at)?);
            Ok(result)
        } else {
            let (result, text) = pprint::capture_output(
                runtime,
                self.column.unwrap_or(0),
                self.preserve_layout_markers,
                at,
                call,
            )?;
            self.push(&text);
            Ok(result)
        }
    }

    fn print(
        &mut self,
        runtime: &PrintContext<'_>,
        mode: printer::PrinterPrimitive,
        argument: Word,
        at: usize,
    ) -> Result<(), Fault> {
        if self.destination.is_some() || self.preserve_layout_markers {
            let argument = runtime.root_slot(argument);
            self.with_stream(runtime, at, |stream| {
                printer::print_to(runtime, mode, argument.get(), stream, at)
            })
        } else {
            self.push(&printer::print_at(
                runtime,
                mode,
                argument,
                None,
                self.column.unwrap_or(0),
                at,
            )?);
            Ok(())
        }
    }

    fn push(&mut self, text: &str) {
        self.text.push_str(text);
        self.advance(text);
    }

    fn push_text(&mut self, text: &str) {
        if self.add_fill_breaks && self.preserve_layout_markers {
            self.push_filled(text);
        } else {
            self.push(text);
        }
    }

    fn push_filled(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut start = 0usize;
        let mut index = 0usize;
        while index < bytes.len() {
            if bytes[index] != b' ' {
                index += 1;
                continue;
            }
            let mut end = index;
            while end < bytes.len() && bytes[end] == b' ' {
                end += 1;
            }
            self.push(&text[start..end]);
            self.push(pprint::newline_marker_for(pprint::NewlineKind::Fill));
            start = end;
            index = end;
        }
        if start < bytes.len() {
            self.push(&text[start..]);
        }
    }

    fn advance(&mut self, text: &str) {
        if let Some((_, last)) = text.rsplit_once('\n') {
            self.column = Some(pprint::visible_width(last));
        } else {
            self.column = self
                .column
                .map(|column| column + pprint::visible_width(text));
        }
    }

    fn tab(
        &mut self,
        runtime: &PrintContext<'_>,
        directive: &FormatDirective,
        column: usize,
        increment: usize,
        at: usize,
    ) -> Result<(), Fault> {
        let kind = match (directive.colon, directive.atsign) {
            (false, false) => pprint::TabKind::Line,
            (false, true) => pprint::TabKind::LineRelative,
            (true, false) => pprint::TabKind::Section,
            (true, true) => pprint::TabKind::SectionRelative,
        };
        if self.preserve_layout_markers && pprint::pretty_enabled(runtime) {
            self.push(&pprint::tab_marker_for(kind, column, increment));
        } else if !directive.colon {
            let count = match self.column {
                Some(current) => pprint::tab_padding(kind, column, increment, current, 0),
                None if directive.atsign => column,
                None => 2,
            };
            if !directive.atsign
                && let Some(stream) = &self.destination
                && streams::gray::uses_protocol(runtime, stream.get(), at)?
            {
                let target = self
                    .column
                    .map_or(column, |current| current.saturating_add(count));
                let advanced = self.with_stream(runtime, at, |stream| {
                    streams::gray::advance_to_column(runtime, stream, target, at)
                })?;
                if advanced {
                    return Ok(());
                }
            }
            self.pad(' ', count, at)?;
        }
        Ok(())
    }

    fn pad(&mut self, character: char, count: usize, at: usize) -> Result<(), Fault> {
        let oom = || EvalError::out_of_memory(at, crate::OutOfMemory);
        let bytes = count.checked_mul(character.len_utf8()).ok_or_else(oom)?;
        self.text.try_reserve(bytes).map_err(|_| oom())?;
        self.text.extend(std::iter::repeat_n(character, count));
        if character == '\n' && count > 0 {
            self.column = Some(0);
        } else if let Some(column) = &mut self.column {
            *column += count;
        }
        Ok(())
    }
}

struct ArgumentCursor<'a> {
    values: &'a RootSlots,
    position: usize,
    has_more_sublists: Option<bool>,
}

impl<'a> ArgumentCursor<'a> {
    fn new(values: &'a RootSlots) -> Self {
        Self {
            values,
            position: 0,
            has_more_sublists: None,
        }
    }

    fn remaining(&self) -> usize {
        self.values.len() - self.position
    }

    fn next(&mut self, at: usize) -> Result<Word, Fault> {
        if self.position == self.values.len() {
            return Err(format_error(at, "FORMAT ran out of arguments"));
        }
        let value = self.values.get(self.position);
        self.position += 1;
        Ok(value)
    }

    fn rest(&self) -> &'a RootSlots {
        self.values.slice(self.position..self.values.len())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PrefixParameterValue {
    Omitted,
    Integer(i64),
    Character(char),
}

struct PrefixParameterValues(Vec<PrefixParameterValue>);

impl PrefixParameterValues {
    fn evaluate(
        runtime: &PrintContext<'_>,
        directive: &FormatDirective,
        args: &mut ArgumentCursor<'_>,
        at: usize,
    ) -> Result<Self, Fault> {
        let values = directive
            .prefix_parameters
            .iter()
            .map(|parameter| {
                Ok(match *parameter {
                    PrefixParameter::Omitted => PrefixParameterValue::Omitted,
                    PrefixParameter::Integer(number) => PrefixParameterValue::Integer(number),
                    PrefixParameter::Character(character) => {
                        PrefixParameterValue::Character(character)
                    }
                    PrefixParameter::RemainingArguments => {
                        PrefixParameterValue::Integer(args.remaining() as i64)
                    }
                    PrefixParameter::NextArgument => match args.next(at)? {
                        Word::NIL => PrefixParameterValue::Omitted,
                        word => {
                            if let Some(number) = word.as_fixnum() {
                                PrefixParameterValue::Integer(number as i64)
                            } else if let Some(character) = word.as_character() {
                                PrefixParameterValue::Character(character)
                            } else if let Some(number) = runtime.heap.integer(word) {
                                PrefixParameterValue::Integer(saturating_integer(&number))
                            } else {
                                return Err(format_error(
                                    at,
                                    "FORMAT V parameter expects an integer, character or NIL",
                                ));
                            }
                        }
                    },
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self(values))
    }

    fn escape(
        &self,
        directive: &FormatDirective,
        args: &ArgumentCursor<'_>,
        at: usize,
    ) -> Result<DirectiveFlow, Fault> {
        self.count(3, at)?;
        if directive.atsign {
            return Err(format_error(at, "FORMAT ~@^ is not defined"));
        }
        if directive.colon && args.has_more_sublists.is_none() {
            return Err(format_error(at, "FORMAT ~:^ requires a colon iteration"));
        }
        let value = |index: usize| {
            self.0
                .get(index)
                .copied()
                .unwrap_or(PrefixParameterValue::Omitted)
        };
        let (first, second, third) = (value(0), value(1), value(2));
        let escape = if third != PrefixParameterValue::Omitted {
            if first == PrefixParameterValue::Omitted || second == PrefixParameterValue::Omitted {
                return Err(format_error(
                    at,
                    "FORMAT ~^ cannot compare NIL with an ordering parameter",
                ));
            }
            first <= second && second <= third
        } else if second != PrefixParameterValue::Omitted {
            first == second
        } else if first != PrefixParameterValue::Omitted {
            first == PrefixParameterValue::Integer(0)
        } else if directive.colon {
            args.has_more_sublists == Some(false)
        } else {
            args.remaining() == 0
        };
        Ok(match (escape, directive.colon) {
            (false, _) => DirectiveFlow::Continue,
            (true, true) => DirectiveFlow::EscapeIteration,
            (true, false) => DirectiveFlow::Escape,
        })
    }

    fn count(&self, maximum: usize, at: usize) -> Result<(), Fault> {
        if self.0.len() > maximum {
            return Err(format_error(at, "too many FORMAT directive parameters"));
        }
        Ok(())
    }

    fn integer(&self, index: usize, at: usize) -> Result<Option<i64>, Fault> {
        match self.0.get(index) {
            None | Some(PrefixParameterValue::Omitted) => Ok(None),
            Some(PrefixParameterValue::Integer(value)) => Ok(Some(*value)),
            _ => Err(format_error(at, "expected integer FORMAT parameter")),
        }
    }

    fn width(&self, index: usize, default: usize, at: usize) -> Result<usize, Fault> {
        self.integer(index, at)?.map_or(Ok(default), |number| {
            Ok(usize::try_from(number).unwrap_or(0))
        })
    }

    fn character(&self, index: usize, default: char, at: usize) -> Result<char, Fault> {
        Ok(self.character_opt(index, at)?.unwrap_or(default))
    }

    fn character_opt(&self, index: usize, at: usize) -> Result<Option<char>, Fault> {
        match self.0.get(index) {
            None | Some(PrefixParameterValue::Omitted) => Ok(None),
            Some(PrefixParameterValue::Character(character)) => Ok(Some(*character)),
            _ => Err(format_error(at, "expected character FORMAT parameter")),
        }
    }
}

#[derive(Default)]
struct FormatDirectiveUsage {
    uses_pretty_printing: bool,
    has_justification_prefix: bool,
}

fn validate_directives(parts: &[ControlPart<'_>], at: usize) -> Result<(), Fault> {
    let mut scan = FormatDirectiveUsage::default();
    collect_directive_usage(parts, false, &mut scan, at)?;
    if scan.has_justification_prefix && scan.uses_pretty_printing {
        return Err(format_error(
            at,
            "pretty-printing directives cannot be used with the ~:; construct in the same format string",
        ));
    }
    Ok(())
}

fn is_pretty_print_directive(directive: &FormatDirective) -> bool {
    match directive.operator {
        'W' | 'I' | '_' => true,
        'T' => directive.colon,
        _ => false,
    }
}

fn collect_directive_usage(
    parts: &[ControlPart<'_>],
    inside_justification: bool,
    scan: &mut FormatDirectiveUsage,
    at: usize,
) -> Result<(), Fault> {
    for part in parts {
        match part {
            ControlPart::Text(_) => {}
            ControlPart::Directive(directive) => {
                if !is_pretty_print_directive(directive) {
                    continue;
                }
                if inside_justification {
                    return Err(format_error(
                        at,
                        format!(
                            "FORMAT directive ~{} is not allowed inside a justification block",
                            directive.operator
                        ),
                    ));
                }
                scan.uses_pretty_printing = true;
            }
            ControlPart::Group(group) => {
                let logical = group.kind == GroupKind::LogicalBlock;
                let justification = group.kind == GroupKind::Justification;
                if inside_justification && logical {
                    return Err(format_error(
                        at,
                        "a pretty-printing logical block is not allowed inside a justification block",
                    ));
                }
                if logical {
                    scan.uses_pretty_printing = true;
                }
                if (logical || justification)
                    && group.sections.iter().any(|section| {
                        section.terminator.operator == ';' && section.terminator.colon
                    })
                {
                    scan.has_justification_prefix = true;
                }
                let child_justification = inside_justification || justification;
                for section in &group.sections {
                    collect_directive_usage(&section.parts, child_justification, scan, at)?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirectiveFlow {
    Continue,
    Escape,
    EscapeIteration,
}

enum IterationBody<'a> {
    Parts(&'a [ControlPart<'a>]),
    Function(RootSlot),
}

struct IterationOptions {
    limit: usize,
    run_once_if_empty: bool,
    sublists: bool,
}

struct FormatInterpreter<'rt> {
    runtime: &'rt PrintContext<'rt>,
    at: usize,
}

impl FormatInterpreter<'_> {
    fn render_parts(
        &self,
        parts: &[ControlPart<'_>],
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
        depth: usize,
    ) -> Result<DirectiveFlow, Fault> {
        if depth == FORMAT_NESTING_LIMIT {
            return Err(format_error(self.at, "FORMAT execution nests too deeply"));
        }
        for part in parts {
            let flow = match part {
                ControlPart::Text(text) => {
                    output.push_text(text);
                    DirectiveFlow::Continue
                }
                ControlPart::Directive(directive) => {
                    self.render_directive(directive, args, output, depth)?
                }
                ControlPart::Group(group) => self.render_group(group, args, output, depth + 1)?,
            };
            if flow != DirectiveFlow::Continue {
                return Ok(flow);
            }
        }
        Ok(DirectiveFlow::Continue)
    }

    fn render_directive(
        &self,
        directive: &FormatDirective,
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
        depth: usize,
    ) -> Result<DirectiveFlow, Fault> {
        let at = self.at;
        let params = PrefixParameterValues::evaluate(self.runtime, directive, args, at)?;
        match directive.operator {
            'A' | 'S' => {
                self.write_object(directive, &params, args.next(at)?, output)?;
            }
            'W' => {
                params.count(0, at)?;
                let argument = args.next(at)?;
                let _controls = printer::bind_format_write_controls(
                    self.runtime,
                    directive.colon,
                    directive.atsign,
                );
                output.print(self.runtime, printer::PrinterPrimitive::Write, argument, at)?;
            }
            'D' | 'B' | 'O' | 'X' | 'R' => {
                self.number(directive, params, args.next(at)?, output)?
            }
            'F' => {
                let text = self.fixed(directive, params, args.next(at)?)?;
                output.push(&text);
            }
            'E' => {
                let text = self.exponential(directive, params, args.next(at)?)?;
                output.push(&text);
            }
            'P' => {
                params.count(0, at)?;
                let argument = if directive.colon {
                    args.values.get(args.position.checked_sub(1).ok_or_else(|| {
                        format_error(at, "FORMAT ~:P requires a previous argument")
                    })?)
                } else {
                    args.next(at)?
                };
                let singular = argument.as_fixnum() == Some(1);
                let suffix = match (singular, directive.atsign) {
                    (true, false) => "",
                    (false, false) => "s",
                    (true, true) => "y",
                    (false, true) => "ies",
                };
                output.push(suffix);
            }
            'C' => {
                params.count(0, at)?;
                self.write_character(directive, args.next(at)?, output)?;
            }
            '%' | '&' | '|' | '~' => {
                params.count(1, at)?;
                let count = params.width(0, 1, at)?;
                let character = match directive.operator {
                    '|' => '\x0c',
                    '~' => '~',
                    _ => '\n',
                };
                let count = if directive.operator == '&' && output.column == Some(0) {
                    count.saturating_sub(1)
                } else {
                    count
                };
                output.pad(character, count, at)?;
            }
            '\n' => {
                params.count(0, at)?;
                if directive.atsign {
                    output.push("\n");
                }
            }
            '_' => {
                params.count(0, at)?;
                if output.preserve_layout_markers && pprint::pretty_enabled(self.runtime) {
                    let kind = match (directive.colon, directive.atsign) {
                        (false, false) => pprint::NewlineKind::Linear,
                        (false, true) => pprint::NewlineKind::Miser,
                        (true, false) => pprint::NewlineKind::Fill,
                        (true, true) => pprint::NewlineKind::Mandatory,
                    };
                    output.push(pprint::newline_marker_for(kind));
                }
            }
            'I' => {
                params.count(1, at)?;
                if directive.atsign {
                    return Err(format_error(at, "FORMAT ~@I is not defined"));
                }
                let amount = params.integer(0, at)?.unwrap_or(0);
                if output.preserve_layout_markers && pprint::pretty_enabled(self.runtime) {
                    output.push(&pprint::indent_marker_for(directive.colon, amount));
                }
            }
            'T' => {
                params.count(2, at)?;
                let column = params.width(0, 1, at)?;
                let increment = params.width(1, 1, at)?;
                output.tab(self.runtime, directive, column, increment, at)?;
            }
            '*' => {
                params.count(1, at)?;
                let n = params.width(0, usize::from(!directive.atsign), at)?;
                let position = if directive.atsign {
                    Some(n)
                } else if directive.colon {
                    args.position.checked_sub(n)
                } else {
                    args.position.checked_add(n)
                };
                args.position = position
                    .filter(|&position| position <= args.values.len())
                    .ok_or_else(|| format_error(at, "FORMAT argument index is out of range"))?;
            }
            '?' => {
                params.count(0, at)?;
                let argument = args.next(at)?;
                if let Some(control) = self.runtime.heap.string_contents(argument) {
                    let parts = parse_format_control(&control, at)?;
                    let flow = if directive.atsign {
                        self.render_parts(&parts, args, output, depth + 1)?
                    } else {
                        let values = self.list(args.next(at)?)?;
                        self.render_parts(
                            &parts,
                            &mut ArgumentCursor::new(&values),
                            output,
                            depth + 1,
                        )?
                    };
                    if flow == DirectiveFlow::EscapeIteration {
                        return Ok(flow);
                    }
                } else if super::is_function(self.runtime, argument) {
                    let function = self.runtime.root_slot(argument);
                    if directive.atsign {
                        self.consume_control_arguments(&function, args, output)?;
                    } else {
                        let values = self.list(args.next(at)?)?;
                        self.call_control_function(&function, &values, output)?;
                    }
                } else {
                    return Err(invalid_format_control(self.runtime, argument, "FORMAT ~?", at));
                }
            }
            '^' => return params.escape(directive, args, at),
            '/' => {
                self.call_directive(directive, &params, args, output)?;
            }
            operator => {
                return Err(format_error(
                    at,
                    format!("unsupported FORMAT directive ~{operator}"),
                ));
            }
        }
        Ok(DirectiveFlow::Continue)
    }

    fn write_object(
        &self,
        directive: &FormatDirective,
        params: &PrefixParameterValues,
        argument: Word,
        output: &mut FormatOutput,
    ) -> Result<(), Fault> {
        let at = self.at;
        let mode = if directive.operator == 'A' {
            printer::PrinterPrimitive::Princ
        } else {
            printer::PrinterPrimitive::Prin1
        };
        let padding = FieldPadding::read(params, at)?;
        if !(directive.colon && argument == Word::NIL)
            && padding.minimum_width == 0
            && padding.minimum_padding == 0
        {
            output.print(self.runtime, mode, argument, at)?;
            return Ok(());
        }
        let printed = if directive.colon && argument == Word::NIL {
            "()".into()
        } else {
            let _depth = output
                .destination
                .as_ref()
                .map(|stream| crate::printer::write_depth(self.runtime, stream.get()));
            printer::print_at(
                self.runtime,
                mode,
                argument,
                None,
                output.column.unwrap_or(0),
                at,
            )?
        };
        let n = padding.object_padding(printed.chars().count(), at)?;
        if directive.atsign {
            output.pad(padding.pad_character, n, at)?;
        }
        output.push(&printed);
        if !directive.atsign {
            output.pad(padding.pad_character, n, at)?;
        }
        Ok(())
    }

    fn write_character(
        &self,
        directive: &FormatDirective,
        word: Word,
        output: &mut FormatOutput,
    ) -> Result<(), Fault> {
        let at = self.at;
        let character = word.as_character().ok_or_else(|| {
            Fault::type_error(self.runtime, 
                word,
                "CHARACTER",
                EvalError::new(at, "FORMAT ~C expects a character"),
            )
        })?;
        let mut buffer = [0; 4];
        if directive.colon {
            if character == ' ' || character.is_control() {
                output.push(&crate::character::name(character));
            } else {
                output.push(character.encode_utf8(&mut buffer));
            }
        } else if directive.atsign {
            output.push(&printer::print_text(
                self.runtime,
                printer::PrinterPrimitive::Prin1,
                word,
                None,
                at,
            )?);
        } else {
            output.push(character.encode_utf8(&mut buffer));
        }
        Ok(())
    }

    fn call_directive(
        &self,
        directive: &FormatDirective,
        params: &PrefixParameterValues,
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
    ) -> Result<(), Fault> {
        let at = self.at;
        let name = directive
            .name
            .as_deref()
            .ok_or_else(|| format_error(at, "FORMAT ~/ requires a function name"))?;
        let call_args = self.runtime.root_frame(params.0.len() + 3);
        self.write_parameter_arguments(params, call_args.slice(3..call_args.len()));
        call_args.set(0, args.next(at)?);
        call_args.set(1, if directive.colon { Word::T } else { Word::NIL });
        call_args.set(2, if directive.atsign { Word::T } else { Word::NIL });
        let function = self.runtime.root_slot(self.format_function(name, at)?);
        output.with_stream(self.runtime, at, |stream| {
            call_with_stream(self.runtime, function.get(), stream, &call_args)
        })?;
        Ok(())
    }

    fn render_group(
        &self,
        group: &DirectiveGroup<'_>,
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
        depth: usize,
    ) -> Result<DirectiveFlow, Fault> {
        let at = self.at;
        let directive = &group.opener;
        let sections = &group.sections;
        let params = PrefixParameterValues::evaluate(self.runtime, directive, args, at)?;
        match group.kind {
            GroupKind::CaseConversion => {
                params.count(0, at)?;
                let mut case_output = FormatOutput::new(output.column);
                let flow = self.render_parts(&sections[0].parts, args, &mut case_output, depth)?;
                let text = match (directive.colon, directive.atsign) {
                    (false, false) => case_output.text.to_lowercase(),
                    (true, true) => case_output.text.to_uppercase(),
                    (colon, _) => capitalize(&case_output.text, colon),
                };
                output.push(&text);
                Ok(flow)
            }
            GroupKind::Conditional => {
                let default = sections.iter().position(|section| {
                    section.terminator.operator == ';' && section.terminator.colon
                });
                let selected = match (directive.colon, directive.atsign) {
                    (true, false) if sections.len() == 2 && default.is_none() => {
                        params.count(0, at)?;
                        Some(usize::from(args.next(at)? != Word::NIL))
                    }
                    (false, true) if sections.len() == 1 => {
                        params.count(0, at)?;
                        if args.next(at)? == Word::NIL {
                            None
                        } else {
                            args.position -= 1;
                            Some(0)
                        }
                    }
                    (false, false) => {
                        params.count(1, at)?;
                        if default.is_some_and(|index| index + 2 != sections.len()) {
                            return Err(format_error(at, "FORMAT default clause must come last"));
                        }
                        let number = if let Some(number) = params.integer(0, at)? {
                            number
                        } else {
                            integer_selector(self.runtime, args.next(at)?, at)?
                        };
                        let count = default.map_or(sections.len(), |index| index + 1);
                        usize::try_from(number)
                            .ok()
                            .filter(|&index| index < count)
                            .or(default.map(|index| index + 1))
                    }
                    _ => return Err(format_error(at, "invalid FORMAT conditional clauses")),
                };
                if let Some(selected) = selected {
                    self.render_parts(&sections[selected].parts, args, output, depth)
                } else {
                    Ok(DirectiveFlow::Continue)
                }
            }
            GroupKind::Iteration => {
                self.render_iteration(directive, &sections[0], &params, args, output, depth)
            }
            GroupKind::LogicalBlock => self.logical_block(directive, sections, args, output, depth),
            GroupKind::Justification => {
                self.justify(directive, sections, params, args, output, depth)
            }
        }
    }

    fn render_iteration(
        &self,
        directive: &FormatDirective,
        section: &DirectiveSection<'_>,
        params: &PrefixParameterValues,
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
        depth: usize,
    ) -> Result<DirectiveFlow, Fault> {
        let at = self.at;
        params.count(1, at)?;
        let repeat = IterationOptions {
            limit: params.width(0, usize::MAX, at)?,
            run_once_if_empty: section.terminator.colon,
            sublists: directive.colon,
        };
        let control;
        let parts;
        let body = if section.parts.is_empty() {
            let argument = args.next(at)?;
            if let Some(text) = self.runtime.heap.string_contents(argument) {
                control = text;
                parts = parse_format_control(&control, at)?;
                IterationBody::Parts(&parts)
            } else if super::is_function(self.runtime, argument) {
                IterationBody::Function(self.runtime.root_slot(argument))
            } else {
                return Err(invalid_format_control(self.runtime, argument, "FORMAT ~{", at));
            }
        } else {
            IterationBody::Parts(&section.parts)
        };

        let owned;
        let source = if directive.atsign {
            args.rest()
        } else {
            owned = self.list(args.next(at)?)?;
            &owned
        };
        let mut local = ArgumentCursor::new(source);
        self.iterate(&body, &mut local, output, repeat, depth)?;
        if directive.atsign {
            args.position += local.position;
        }
        Ok(DirectiveFlow::Continue)
    }

    fn iterate(
        &self,
        body: &IterationBody<'_>,
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
        repeat: IterationOptions,
        depth: usize,
    ) -> Result<(), Fault> {
        let at = self.at;
        let mut visited = HashSet::new();
        for index in 0..repeat.limit {
            if args.remaining() == 0 && !(index == 0 && repeat.run_once_if_empty) {
                break;
            }
            if repeat.sublists {
                let values = if args.remaining() == 0 {
                    RootFrame::empty()
                } else {
                    self.list(args.next(at)?)?
                };
                let mut sublist = ArgumentCursor::new(&values);
                sublist.has_more_sublists = Some(args.remaining() > 0);
                if self.render_iteration_body(body, &mut sublist, output, depth)?
                    == DirectiveFlow::EscapeIteration
                {
                    break;
                }
            } else {
                if matches!(body, IterationBody::Parts(_))
                    && repeat.limit == usize::MAX
                    && !visited.insert(args.position)
                {
                    return Err(format_error(
                        at,
                        "unbounded FORMAT iteration repeats the same arguments",
                    ));
                }
                if self.render_iteration_body(body, args, output, depth)? != DirectiveFlow::Continue
                {
                    break;
                }
            }
        }
        Ok(())
    }

    fn render_iteration_body(
        &self,
        body: &IterationBody<'_>,
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
        depth: usize,
    ) -> Result<DirectiveFlow, Fault> {
        match body {
            IterationBody::Parts(parts) => self.render_parts(parts, args, output, depth),
            IterationBody::Function(function) => {
                if args.has_more_sublists.is_some() {
                    self.call_control_function(function, args.rest(), output)?;
                    return Ok(DirectiveFlow::Continue);
                }
                let consumed = self.consume_control_arguments(function, args, output)?;
                Ok(if consumed == 0 {
                    DirectiveFlow::Escape
                } else {
                    DirectiveFlow::Continue
                })
            }
        }
    }

    fn call_control_function(
        &self,
        function: &RootSlot,
        values: &RootSlots,
        output: &mut FormatOutput,
    ) -> Result<Vec<crate::Root<'_>>, Fault> {
        output.with_stream(self.runtime, self.at, |stream| {
            call_with_stream(self.runtime, function.get(), stream, values)
        })
    }

    /// A formatter function returns the unused tail. Advance the shared cursor
    /// by the number of arguments it consumed, not by the number it received.
    fn consume_control_arguments(
        &self,
        function: &RootSlot,
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
    ) -> Result<usize, Fault> {
        let remaining = args.rest();
        let returned = self.call_control_function(function, remaining, output)?;
        let tail = returned.first().map_or(Word::NIL, |value| value.slot.get());
        let unused = self.list(tail)?.len();
        let consumed = remaining.len().checked_sub(unused).ok_or_else(|| {
            format_error(
                self.at,
                "a format function returned a tail longer than its arguments",
            )
        })?;
        args.position += consumed;
        Ok(consumed)
    }

    fn justify(
        &self,
        directive: &FormatDirective,
        sections: &[DirectiveSection<'_>],
        params: PrefixParameterValues,
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
        depth: usize,
    ) -> Result<DirectiveFlow, Fault> {
        let at = self.at;
        if sections
            .last()
            .is_some_and(|section| section.terminator.colon)
        {
            return Err(format_error(
                at,
                "FORMAT pretty-printing logical blocks are unsupported",
            ));
        }
        let padding = FieldPadding::read(&params, at)?;
        let mut prefix = None;
        let mut fragments = Vec::new();
        for (index, section) in sections.iter().enumerate() {
            let mut fragment = FormatOutput::new(Some(0));
            let flow = self.render_parts(&section.parts, args, &mut fragment, depth)?;
            if flow == DirectiveFlow::EscapeIteration {
                return Ok(flow);
            }
            if flow == DirectiveFlow::Escape {
                break;
            }
            if index == 0 && section.terminator.operator == ';' && section.terminator.colon {
                let params =
                    PrefixParameterValues::evaluate(self.runtime, &section.terminator, args, at)?;
                params.count(2, at)?;
                prefix = Some((
                    fragment.text,
                    params.width(0, 0, at)?,
                    params.width(1, 72, at)?,
                ));
            } else {
                fragments.push(fragment.text);
            }
        }
        if fragments.is_empty() {
            return Ok(DirectiveFlow::Continue);
        }
        let gaps =
            fragments.len() - 1 + usize::from(directive.colon) + usize::from(directive.atsign);
        let contents: usize = fragments.iter().map(|text| text.chars().count()).sum();
        let chars = contents.saturating_add(gaps.saturating_mul(padding.minimum_padding));
        let total = if chars > padding.minimum_width {
            let extra = (chars - padding.minimum_width)
                .div_ceil(padding.column_increment)
                .saturating_mul(padding.column_increment);
            padding.minimum_width.saturating_add(extra)
        } else {
            padding.minimum_width
        };
        if let Some((new_line, spare, line_width)) = prefix
            && output
                .column
                .unwrap_or(0)
                .saturating_add(total)
                .saturating_add(spare)
                > line_width
        {
            output.push(&new_line);
        }
        let mut remaining = total.saturating_sub(contents);
        let mut remaining_gaps = gaps;
        let single = fragments.len() == 1;
        if directive.colon || (!directive.atsign && single) {
            justify_padding(output, &mut remaining, &mut remaining_gaps, &padding, at)?;
        }
        for (index, text) in fragments.iter().enumerate() {
            if index > 0 {
                justify_padding(output, &mut remaining, &mut remaining_gaps, &padding, at)?;
            }
            output.push(text);
        }
        if directive.atsign {
            justify_padding(output, &mut remaining, &mut remaining_gaps, &padding, at)?;
        }
        Ok(DirectiveFlow::Continue)
    }

    fn literal_section(
        &self,
        section: &DirectiveSection<'_>,
        which: &str,
        at: usize,
    ) -> Result<String, Fault> {
        let mut text = String::new();
        for part in &section.parts {
            match part {
                ControlPart::Text(piece) => text.push_str(piece),
                _ => {
                    return Err(format_error(
                        at,
                        format!(
                            "Cannot include format directives inside the {which} segment of ~<...~:>"
                        ),
                    ));
                }
            }
        }
        Ok(text)
    }

    fn write_parameter_arguments(&self, params: &PrefixParameterValues, slots: &RootSlots) {
        for (index, value) in params.0.iter().enumerate() {
            let word = match value {
                PrefixParameterValue::Omitted => Word::NIL,
                PrefixParameterValue::Character(character) => Word::character(*character),
                PrefixParameterValue::Integer(number) => Word::fixnum(*number as isize)
                    .unwrap_or_else(|| {
                        let big = num_bigint::BigInt::from(*number);
                        self.runtime
                            .heap
                            .allocate_number(&crate::number::Number::Real(
                                crate::number::Real::Integer(big),
                            ))
                            .unwrap_or(Word::NIL)
                    }),
            };
            slots.set(index, word);
        }
    }

    fn format_function(&self, name: &str, at: usize) -> Result<Word, Fault> {
        let name = name.to_ascii_uppercase();
        let (package, symbol_name) = match name.find(':') {
            None => (Package::USER, name.clone()),
            Some(first) => {
                let package_name = &name[..first];
                let rest = &name[first + 1..];
                let package = self
                    .runtime
                    .find_package_named(package_name)
                    .ok_or_else(|| format_error(at, format!("no package named {package_name}")))?;
                let symbol_name = rest.trim_prefix(':');
                (package, symbol_name.to_string())
            }
        };
        if let Some((word, _)) = self.runtime.packages.borrow().find(package, &symbol_name) {
            return Ok(word);
        }
        self.runtime
            .intern_package(package, &symbol_name, at)
            .map(|(word, _)| word)
            .map_err(Into::into)
    }

    fn logical_block(
        &self,
        directive: &FormatDirective,
        sections: &[DirectiveSection<'_>],
        args: &mut ArgumentCursor<'_>,
        output: &mut FormatOutput,
        depth: usize,
    ) -> Result<DirectiveFlow, Fault> {
        let at = self.at;
        params_count(&directive.prefix_parameters, at)?;
        let (default_prefix, default_suffix) = if directive.colon {
            ("(".to_string(), ")".to_string())
        } else {
            (String::new(), String::new())
        };
        let (prefix, per_line, body, suffix, close) = match sections {
            [body] => (
                default_prefix,
                false,
                &body.parts,
                default_suffix,
                &body.terminator,
            ),
            [prefix, body] => (
                self.literal_section(prefix, "prefix", at)?,
                prefix.terminator.atsign,
                &body.parts,
                default_suffix,
                &body.terminator,
            ),
            [prefix, body, suffix] => (
                self.literal_section(prefix, "prefix", at)?,
                prefix.terminator.atsign,
                &body.parts,
                self.literal_section(suffix, "suffix", at)?,
                &suffix.terminator,
            ),
            _ => return Err(format_error(at, "Too many segments for ~<...~:>")),
        };
        let remaining = args.rest();
        let object = if directive.atsign {
            // The block owns all remaining arguments, even when its body is skipped.
            args.position = args.values.len();
            None
        } else {
            let object = args.next(at)?;
            if object != Word::NIL && self.runtime.heap.cons_fields(object).is_none() {
                output.print(self.runtime, printer::PrinterPrimitive::Write, object, at)?;
                return Ok(DirectiveFlow::Continue);
            }
            Some(self.runtime.root_slot(object))
        };
        let print_depth = pprint::logical_block_depth(self.runtime);
        if pprint::print_level_reached(self.runtime, print_depth, at)? {
            output.push("#");
            return Ok(DirectiveFlow::Continue);
        }
        let pretty = pprint::pretty_enabled(self.runtime);
        let mut inner =
            FormatOutput::new(output.column.map(|column| column + prefix.chars().count()));
        inner.preserve_layout_markers = pretty;
        inner.add_fill_breaks = close.atsign;
        let mut label_prefix = String::new();
        let mut tail = None;
        let mut sharing_scope = None;
        let block_values;
        let values = if let Some(object) = object {
            let object = object.get();
            let options = pretty
                .then(|| pprint::printer_options(self.runtime, at))
                .transpose()?;
            block_values = if let Some(options) = options
                .as_ref()
                .filter(|options| options.circle && object != Word::NIL)
            {
                let mut values = Vec::new();
                sharing_scope = Some(crate::printer::sharing_scope(self.runtime, object, options));
                if let Some(crate::printer::SharingMarker::Definition(id)) =
                    crate::printer::next_sharing_marker(self.runtime, options, object)
                {
                    label_prefix = format!("#{id}=");
                }
                let mut spine = object;
                loop {
                    let Some((car, cdr)) = self.runtime.heap.cons_fields(spine) else {
                        tail = Some(self.runtime.root_slot(spine));
                        break;
                    };
                    values.push(car);
                    if cdr == Word::NIL {
                        break;
                    }
                    if self.runtime.heap.cons_fields(cdr).is_none()
                        || crate::printer::list_tail_needs_label(self.runtime, options, cdr)
                    {
                        tail = Some(self.runtime.root_slot(cdr));
                        break;
                    }
                    spine = cdr;
                }
                root_words(self.runtime, values)
            } else {
                self.list(object)?
            };
            &*block_values
        } else {
            remaining
        };
        let block_depth = pprint::bind_logical_block_depth(self.runtime, print_depth + 1);
        let flow = self.render_parts(body, &mut ArgumentCursor::new(values), &mut inner, depth)?;
        let tail_text = match tail {
            Some(tail) => {
                let column = output.column.unwrap_or(0);
                let text = super::printer::render_current(self.runtime, tail.get(), column, at)?;
                format!(" . {text}")
            }
            None => String::new(),
        };
        drop(block_depth);
        drop(sharing_scope);
        if pretty {
            let mut text = label_prefix;
            text.push_str(&prefix);
            let line_prefix = per_line.then_some(prefix.as_str());
            text.push_str(&pprint::logical_block_marker(line_prefix));
            text.push_str(&inner.text);
            text.push_str(&tail_text);
            text.push_str(&suffix);
            text.push_str(&pprint::block_end_marker());
            if output.preserve_layout_markers {
                output.push(&text);
            } else {
                let column = output.column.unwrap_or(0);
                let (margin, miser) = pprint::limits(self.runtime, at)?;
                output.push(&pprint::layout_text(&text, column, margin, miser));
            }
        } else {
            output.push(&label_prefix);
            output.push(&prefix);
            output.push(&inner.text);
            output.push(&tail_text);
            output.push(&suffix);
        }
        if flow == DirectiveFlow::EscapeIteration {
            return Ok(flow);
        }
        Ok(DirectiveFlow::Continue)
    }

    fn list(&self, word: Word) -> Result<RootFrame, Fault> {
        proper_list(self.runtime, word, self.at)
            .map(|words| root_words(self.runtime, words))
            .map_err(|error| {
                Fault::type_error(self.runtime, 
                    crate::vm::list::list_tail(self.runtime, word),
                    "LIST",
                    error,
                )
            })
    }
}

fn params_count(params: &[PrefixParameter], at: usize) -> Result<(), Fault> {
    if params.is_empty() {
        return Ok(());
    }
    Err(format_error(
        at,
        "No parameters can be supplied with ~<...~:>",
    ))
}

fn justify_padding(
    output: &mut FormatOutput,
    remaining: &mut usize,
    remaining_gaps: &mut usize,
    padding: &FieldPadding,
    at: usize,
) -> Result<(), Fault> {
    let count = remaining.checked_div(*remaining_gaps).unwrap_or(*remaining);
    *remaining = remaining.saturating_sub(count);
    *remaining_gaps = remaining_gaps.saturating_sub(1);
    output.pad(padding.pad_character, count, at)
}

fn integer_selector(runtime: &PrintContext<'_>, value: Word, at: usize) -> Result<i64, Fault> {
    if let Some(number) = value.as_fixnum() {
        return Ok(number as i64);
    }
    runtime
        .heap
        .integer(value)
        .map(|number| saturating_integer(&number))
        .ok_or_else(|| format_error(at, "FORMAT ~[ expects an integer selector"))
}

fn saturating_integer(number: &num_bigint::BigInt) -> i64 {
    use num_traits::ToPrimitive;
    number
        .to_i64()
        .unwrap_or(if number.sign() == num_bigint::Sign::Minus {
            i64::MIN
        } else {
            i64::MAX
        })
}

struct FieldPadding {
    minimum_width: usize,
    column_increment: usize,
    minimum_padding: usize,
    pad_character: char,
}

impl FieldPadding {
    fn read(params: &PrefixParameterValues, at: usize) -> Result<Self, Fault> {
        params.count(4, at)?;
        let increment = params.width(1, 1, at)?;
        if increment == 0 {
            return Err(format_error(at, "FORMAT column increment must be positive"));
        }
        Ok(Self {
            minimum_width: params.width(0, 0, at)?,
            column_increment: increment,
            minimum_padding: params.width(2, 0, at)?,
            pad_character: params.character(3, ' ', at)?,
        })
    }

    fn object_padding(&self, contents: usize, at: usize) -> Result<usize, Fault> {
        let oom = || EvalError::out_of_memory(at, crate::OutOfMemory);
        let minimum_width = contents.checked_add(self.minimum_padding).ok_or_else(oom)?;
        let extra = self
            .minimum_width
            .saturating_sub(minimum_width)
            .div_ceil(self.column_increment);
        extra
            .checked_mul(self.column_increment)
            .and_then(|extra| self.minimum_padding.checked_add(extra))
            .ok_or_else(|| oom().into())
    }
}

fn capitalize(text: &str, all_words: bool) -> String {
    let mut output = String::new();
    let mut start = true;
    for character in text.chars() {
        if start && character.is_alphanumeric() {
            output.extend(character.to_uppercase());
            start = false;
        } else {
            output.extend(character.to_lowercase());
            if all_words && !character.is_alphanumeric() {
                start = true;
            }
        }
    }
    output
}
