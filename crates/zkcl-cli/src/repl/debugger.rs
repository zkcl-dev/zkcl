use super::snapshot::lisp_string_literal;
use std::fmt::Write;

use capstone::prelude::*;
use zkcl::{Breakpoint, BreakpointFollow, BreakpointOptions, Root, Runtime};
pub(super) const DEBUGGER: &str = include_str!("debugger.lisp");

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
    let request = parse_breakpoint(args)?;
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
    let mut request = parse_breakpoint(args)?;
    let line = request
        .line
        .or(request.instruction)
        .filter(|line| *line > 0)
        .ok_or("Usage: :break-file <path> <line> [breakpoint options]")?;
    request.options.follow = BreakpointFollow::Source;
    let path = super::commands::expand_tilde(super::commands::unquote_path(&request.name));
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
    let mut output = String::new();
    for location in locations {
        let path = location
            .source
            .as_ref()
            .and_then(|source| source.path())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<submission>".into());
        let _ = writeln!(
            output,
            "  pc {}  {path}{}{}",
            location.bytecode_pc,
            location
                .line
                .map(|line| format!(":{line}"))
                .unwrap_or_default(),
            location
                .column
                .map(|column| format!(":{column}"))
                .unwrap_or_default()
        );
    }
    Ok(output)
}

struct BreakpointRequest {
    name: String,
    instruction: Option<usize>,
    line: Option<usize>,
    options: BreakpointOptions,
}

fn parse_breakpoint(args: &str) -> Result<BreakpointRequest, String> {
    const USAGE: &str = "Usage: :break <function> [instruction | --line N] [--condition FORM] [--hit N] [--every N] [--log FORM] [--log-only] [--disabled] [--follow object|name|source]";
    let tokens = command_forms(args)?;
    let Some(name) = tokens.first() else {
        return Err(USAGE.into());
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
                    .ok_or_else(|| format!("{option} needs a value.\n{USAGE}"))?;
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
            token if request.instruction.is_none() && request.line.is_none() => {
                request.instruction = Some(token.parse().map_err(|_| USAGE.to_owned())?);
            }
            _ => return Err(USAGE.into()),
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
        args.trim(),
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
    let name = breakpoint
        .function
        .as_deref()
        .unwrap_or("<anonymous function>");
    let requested = breakpoint
        .requested_path
        .as_ref()
        .map(|path| {
            format!(
                " {}:{}",
                path.display(),
                breakpoint.requested_line.unwrap_or(1)
            )
        })
        .unwrap_or_default();
    format!(
        "Breakpoint {}  {name}{requested} [pc {}] {} hits={} follow={:?} status={:?}{}{}{}{}",
        breakpoint.id,
        breakpoint.bytecode_pc,
        if breakpoint.options.enabled {
            "enabled"
        } else {
            "disabled"
        },
        breakpoint.hits,
        breakpoint.options.follow,
        breakpoint.status,
        breakpoint
            .options
            .condition
            .as_ref()
            .map(|form| format!(" condition={form}"))
            .unwrap_or_default(),
        breakpoint
            .options
            .hit_count
            .map(|count| format!(" hit={count}"))
            .unwrap_or_default(),
        breakpoint
            .options
            .hit_every
            .map(|count| format!(" every={count}"))
            .unwrap_or_default(),
        breakpoint
            .options
            .log
            .as_ref()
            .map(|form| format!(
                " log={form}{}",
                if breakpoint.options.log_only {
                    " (no stop)"
                } else {
                    ""
                }
            ))
            .unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoint_options_keep_lisp_forms_and_strings_together() {
        let parsed = parse_breakpoint(r#"sample --line 3 --condition (> n 4) --hit 2 --every 3 --log (format nil "n = ~D" n) --log-only --follow source --disabled"#).unwrap();
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
        assert!(parse_breakpoint("sample --condition (>").is_err());
        assert!(parse_breakpoint("sample --every 0").is_err());
        assert!(parse_breakpoint("sample 0 --line 3").is_err());
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
