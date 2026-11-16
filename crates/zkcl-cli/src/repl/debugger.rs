use super::snapshot::lisp_string_literal;
use std::fmt::Write;

use capstone::prelude::*;
use zkcl::{Breakpoint, BreakpointFollow, BreakpointOptions, BreakpointStatus, Root, Runtime};
use zkcl_terminal::{display_path, single_line};

pub(super) const DEBUGGER: &str = include_str!("debugger.lisp");
const BREAK_USAGE: &str = "Usage: :break <function> [pc | --line N] [breakpoint options]";
const BREAK_FILE_USAGE: &str = "Usage: :break-file <path> <line> [breakpoint options]";

pub(super) fn restart_menu_source(color: bool) -> String {
    let prompt = if color {
        "\x1b[31;1mdebug\x1b[0m\x1b[90m>\x1b[0m "
    } else {
        "debug> "
    };
    format!(
        "(CL:FUNCALL {DEBUGGER}
           (CL:LAMBDA (MESSAGE FRAMES RESTARTS SELECTED LOCALS)
             (CL:DECLARE (CL:IGNORE MESSAGE FRAMES RESTARTS SELECTED LOCALS))
             (CL:WRITE-STRING {} CL:*DEBUG-IO*)
             (CL:FINISH-OUTPUT CL:*DEBUG-IO*)
             (CL:READ-LINE CL:*DEBUG-IO* CL:NIL CL:NIL)) CL:T)",
        lisp_string_literal(prompt)
    )
}

pub(super) fn add_breakpoint(runtime: &Runtime, args: &str) -> Result<String, String> {
    let request = parse_breakpoint(args, BREAK_USAGE)?;
    let function = read_function_name(runtime, &request.name)?;
    let breakpoint = if let Some(line) = request.line {
        runtime.add_source_breakpoint_with_options(&function, line, request.options)
    } else {
        runtime.add_breakpoint_with_options(&function, request.instruction, request.options)
    };
    breakpoint
        .map(|breakpoint| format_breakpoint(&breakpoint))
        .map_err(|error| error.message().to_owned())
}

pub(super) fn add_file_breakpoint(runtime: &Runtime, args: &str) -> Result<String, String> {
    let mut request = parse_breakpoint(args, BREAK_FILE_USAGE)?;
    let line = request
        .line
        .or(request.instruction)
        .filter(|line| *line > 0)
        .ok_or(BREAK_FILE_USAGE)?;
    request.options.follow = BreakpointFollow::Source;
    let path = super::commands::command_path(&request.name)?;
    runtime
        .add_file_breakpoint(&path, line, request.options)
        .map(|breakpoint| format_breakpoint(&breakpoint))
        .map_err(|error| error.to_string())
}

pub(super) fn executable_locations(runtime: &Runtime, args: &str) -> Result<String, String> {
    if args.is_empty() {
        return Err("Usage: :break-locations <function>".into());
    }
    let function = read_function_name(runtime, args)?;
    let locations = runtime
        .executable_locations(&function)
        .map_err(|error| error.to_string())?;
    if locations.is_empty() {
        return Ok("No executable locations for this function.".into());
    }
    let mut output = String::new();
    for location in locations {
        write!(output, "  pc {}", location.bytecode_pc).unwrap();
        if let Some(source) = location.source {
            let label = source.path().map_or_else(
                || {
                    source
                        .uri()
                        .map_or_else(|| "<submission>".into(), single_line)
                },
                display_path,
            );
            write!(output, "  {label}").unwrap();
            if let Some(line) = location.line {
                write!(output, ":{line}").unwrap();
                if let Some(column) = location.column {
                    write!(output, ":{column}").unwrap();
                }
            }
        }
        output.push('\n');
    }
    Ok(output)
}

struct BreakpointRequest {
    name: String,
    instruction: Option<usize>,
    line: Option<usize>,
    options: BreakpointOptions,
}

fn parse_breakpoint(args: &str, usage: &str) -> Result<BreakpointRequest, String> {
    let tokens = command_forms(args)?;
    let Some(name) = tokens.first() else {
        return Err(usage.into());
    };
    let mut request = BreakpointRequest {
        name: name.to_string(),
        instruction: None,
        line: None,
        options: BreakpointOptions::default(),
    };
    let mut index = 1;
    while let Some(token) = tokens.get(index) {
        index += 1;
        match *token {
            "--disabled" => request.options.enabled = false,
            "--log-only" => request.options.log_only = true,
            option @ ("--line" | "--condition" | "--hit" | "--every" | "--log" | "--follow") => {
                let value = tokens
                    .get(index)
                    .ok_or_else(|| format!("{option} needs a value.\n{usage}"))?;
                index += 1;
                let number = || {
                    value
                        .parse::<u64>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("{option} needs a positive integer."))
                };
                match option {
                    "--line" => {
                        request.line =
                            Some(usize::try_from(number()?).map_err(|_| "line is too large")?)
                    }
                    "--hit" => request.options.hit_count = Some(number()?),
                    "--every" => request.options.hit_every = Some(number()?),
                    "--condition" => request.options.condition = Some(value.to_string()),
                    "--log" => request.options.log = Some(value.to_string()),
                    "--follow" => {
                        request.options.follow = match *value {
                            "object" => BreakpointFollow::Object,
                            "name" => BreakpointFollow::Name,
                            "source" => BreakpointFollow::Source,
                            _ => return Err("--follow expects object, name, or source.".into()),
                        }
                    }
                    _ => unreachable!(),
                }
            }
            token if token.starts_with("--") => {
                return Err(format!(
                    "Unknown breakpoint option {}.\n{usage}",
                    single_line(token)
                ));
            }
            token if request.instruction.is_none() => {
                request.instruction = Some(token.parse().map_err(|_| {
                    format!(
                        "Invalid breakpoint position {}.\n{usage}",
                        single_line(token)
                    )
                })?);
            }
            token => {
                return Err(format!(
                    "Unexpected argument {}.\n{usage}",
                    single_line(token)
                ));
            }
        }
    }
    if request.line.is_some() && request.instruction.is_some() {
        return Err("Choose either an instruction or a source line.".into());
    }
    Ok(request)
}

// Preserve Lisp forms as single option values, including strings containing spaces.
fn command_forms(text: &str) -> Result<Vec<&str>, String> {
    let mut forms = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut barred = false;
    let mut escaped = false;
    for (offset, character) in text.char_indices() {
        if start.is_none() && character.is_whitespace() {
            continue;
        }
        start.get_or_insert(offset);
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if !barred && character == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        if character == '|' {
            barred = !barred;
            continue;
        }
        if barred {
            continue;
        }
        match character {
            '(' => depth += 1,
            ')' => depth = depth.checked_sub(1).ok_or("Unmatched ')' in command.")?,
            c if c.is_whitespace() && depth == 0 => {
                forms.push(&text[start.take().unwrap()..offset]);
            }
            _ => {}
        }
    }
    if quoted || barred || depth != 0 || escaped {
        return Err("Incomplete Lisp form in command.".into());
    }
    if let Some(start) = start {
        forms.push(&text[start..]);
    }
    Ok(forms)
}

pub(super) fn read_function_name<'rt>(
    runtime: &'rt Runtime,
    text: &str,
) -> Result<Root<'rt>, String> {
    let name = text
        .trim()
        .trim_start_matches("#'")
        .trim_start_matches('\'');
    runtime.read_value(name).map_err(|error| error.to_string())
}

pub(super) fn disassemble_native(runtime: &Runtime, args: &str) -> Result<String, String> {
    let function = read_function_name(runtime, args)?;
    let code = runtime.machine_code(&function).map_err(|error| error.to_string())?
        .ok_or("No installed machine code for this function. Use a JIT session and compile or call the function first.")?;
    let mut decoder = match std::env::consts::ARCH {
        "aarch64" => Capstone::new()
            .arm64()
            .mode(arch::arm64::ArchMode::Arm)
            .build(),
        "x86_64" => Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode64)
            .build(),
        architecture => {
            return Err(format!(
                "Machine-code disassembly is unavailable for {architecture}."
            ));
        }
    }
    .map_err(|error| error.to_string())?;
    decoder
        .set_skipdata(true)
        .map_err(|error| error.to_string())?;
    let instructions = decoder
        .disasm_all(&code.bytes, code.address)
        .map_err(|error| error.to_string())?;
    let mut result = format!(
        "; {} — installed machine code, {} bytes\n",
        single_line(args.trim()),
        code.bytes.len()
    );
    for instruction in instructions.iter() {
        let offset = instruction.address() - code.address;
        let bytes = instruction
            .bytes()
            .iter()
            .fold(String::new(), |mut displayed, byte| {
                write!(displayed, "{byte:02x}").unwrap();
                displayed
            });
        let mnemonic = instruction.mnemonic().unwrap_or_default();
        let operands = instruction.op_str().unwrap_or_default();
        writeln!(
            result,
            "  +{offset:04x}  {bytes:<30}  {mnemonic:<8} {operands}"
        )
        .unwrap();
    }
    Ok(result)
}

pub(super) fn list_breakpoints(runtime: &Runtime) -> String {
    let breakpoints = runtime.breakpoints();
    if breakpoints.is_empty() {
        return "No breakpoints.".into();
    }
    breakpoints
        .iter()
        .map(format_breakpoint)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn format_breakpoint(breakpoint: &Breakpoint) -> String {
    let mut output = format!("Breakpoint {}", breakpoint.id);
    if let Some(name) = &breakpoint.function {
        write!(output, "  {}", single_line(name)).unwrap();
    }
    if let Some(path) = &breakpoint.requested_path {
        write!(output, "  {}", display_path(path)).unwrap();
        if let Some(line) = breakpoint.requested_line {
            write!(output, ":{line}").unwrap();
        }
    } else if let Some(line) = breakpoint.requested_line {
        write!(output, "  line {line}").unwrap();
    }
    match &breakpoint.status {
        BreakpointStatus::Verified => write!(output, "  [pc {}]", breakpoint.bytecode_pc).unwrap(),
        BreakpointStatus::Pending(reason) => {
            write!(output, "  pending ({})", single_line(reason)).unwrap();
        }
    }

    let options = &breakpoint.options;
    let enabled = if options.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let follow = match options.follow {
        BreakpointFollow::Object => "object",
        BreakpointFollow::Name => "name",
        BreakpointFollow::Source => "source",
    };
    write!(
        output,
        "  {enabled}  hits={}  follow={follow}",
        breakpoint.hits
    )
    .unwrap();
    if let Some(form) = &options.condition {
        write!(output, "  condition={}", single_line(form)).unwrap();
    }
    if let Some(count) = options.hit_count {
        write!(output, "  first hit={count}").unwrap();
    }
    if let Some(count) = options.hit_every {
        write!(output, "  every={count}").unwrap();
    }
    if let Some(form) = &options.log {
        write!(output, "  log={}", single_line(form)).unwrap();
    }
    if options.log_only {
        output.push_str("  (no stop)");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoint_options_keep_lisp_forms_and_strings_together() {
        let parsed = parse_breakpoint(r#"sample --line 3 --condition (> n 4) --hit 2 --every 3 --log (format nil "n = ~D" n) --log-only --follow source --disabled"#, BREAK_USAGE).unwrap();
        assert_eq!(parsed.name, "sample");
        assert_eq!(parsed.line, Some(3));
        assert_eq!(parsed.options.condition.as_deref(), Some("(> n 4)"));
        assert_eq!(
            parsed.options.log.as_deref(),
            Some(r#"(format nil "n = ~D" n)"#)
        );
        assert_eq!(parsed.options.hit_count, Some(2));
        assert_eq!(parsed.options.hit_every, Some(3));
        assert!(parsed.options.log_only);
        assert!(!parsed.options.enabled);
        assert_eq!(parsed.options.follow, BreakpointFollow::Source);
        assert!(parse_breakpoint("sample --condition (>", BREAK_USAGE).is_err());
        assert!(parse_breakpoint("sample --every 0", BREAK_USAGE).is_err());
        assert!(parse_breakpoint("sample 0 --line 3", BREAK_USAGE).is_err());
    }

    #[test]
    fn terminal_breakpoint_controls_preserve_hit_options() {
        let runtime = Runtime::start().unwrap();
        runtime
            .eval("(defun terminal-breakpoint (n) (declare (optimize (debug 3))) (+ n 1))")
            .unwrap();
        let result = add_breakpoint(
            &runtime,
            "terminal-breakpoint --condition (> n 4) --hit 2 --disabled",
        )
        .unwrap();
        assert!(result.contains("disabled"), "{result}");
        let breakpoint = runtime.breakpoints().pop().unwrap();
        assert_eq!(breakpoint.options.condition.as_deref(), Some("(> n 4)"));
        let enabled = runtime.enable_breakpoint(breakpoint.id, true).unwrap();
        assert!(enabled.options.enabled);
        assert_eq!(enabled.options.hit_count, Some(2));
    }
}
