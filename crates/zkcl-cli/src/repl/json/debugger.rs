use serde::Deserialize;
use serde_json::{Value, json};
use zkcl::{DebugLocalsAvailability, DebugMotion, EvalError, NativeResult, Root, Runtime};

use super::interaction::{Answer, SessionIo};
use super::objects::{ObjectError, ObjectRequest, ValueInput};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Action {
    Frame {
        frame: usize,
    },
    Locals {
        frame: Option<usize>,
    },
    Eval {
        frame: Option<usize>,
        source: String,
        #[serde(default = "render_by_default")]
        render: bool,
    },
    Local {
        frame: Option<usize>,
        expression: String,
    },
    Set {
        frame: Option<usize>,
        name: String,
        source: Option<String>,
        handle: Option<u64>,
    },
    Restart {
        number: Option<usize>,
        name: Option<String>,
        #[serde(default)]
        arguments: Vec<ValueInput>,
    },
    Motion {
        motion: Motion,
        #[serde(default)]
        release: Vec<u64>,
    },
    FrameReturn {
        frame: Option<usize>,
        #[serde(default)]
        values: Vec<ValueInput>,
    },
    FrameRestart {
        frame: Option<usize>,
        #[serde(default)]
        stop: bool,
        #[serde(default)]
        release: Vec<u64>,
    },
    Object {
        object: ObjectRequest,
    },
    Breakpoint {
        breakpoint: super::BreakpointRequest,
    },
    Profile {
        profile: super::profile::ProfileRequest,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Motion {
    Continue,
    Into,
    Next,
    Out,
    Abort,
}

fn render_by_default() -> bool {
    true
}

fn call<'rt>(
    runtime: &'rt Runtime,
    package: &str,
    name: &str,
    args: &[&Root<'_>],
) -> Result<Vec<Root<'rt>>, ObjectError> {
    let symbol = runtime
        .lookup_symbol(package, name)?
        .ok_or_else(|| format!("{package}:{name} is unavailable"))?;
    Ok(runtime.call(&symbol, args)?)
}

fn variables(
    io: &SessionIo,
    runtime: &Runtime,
    frame: usize,
) -> Result<(Value, Value), ObjectError> {
    let variables = runtime.debug_variables(frame)?;
    let names = variables
        .iter()
        .map(|variable| runtime.repr(&variable.name))
        .collect::<Result<Vec<_>, _>>()?;
    let mut values = io
        .objects
        .retain_all(
            runtime,
            variables
                .iter()
                .filter_map(|variable| variable.value.as_ref()),
        )?
        .into_iter();
    let mut legacy = Vec::new();
    let mut typed = Vec::new();
    for (variable, name) in variables.into_iter().zip(names) {
        let value = variable
            .value
            .map(|_| values.next().expect("retained variable"));
        if let Some(value) = &value {
            legacy.push(json!({"name":name,"value":value["preview"]}));
        }
        typed.push(json!({"name":name,"value":value,"writable":variable.writable,"unavailable":variable.unavailable}));
    }
    Ok((json!(legacy), json!(typed)))
}

fn evaluation_result(
    io: &SessionIo,
    runtime: &Runtime,
    values: &[Root<'_>],
    render: bool,
) -> Result<Value, ObjectError> {
    let print = |value: &Root<'_>| runtime.print_result_handling_errors(value, None);
    let values = io.present_values(runtime, values, render.then_some(print))?;
    let mut result = json!({"values":values.objects});
    if render {
        result["renderedValues"] = json!(values.text);
    }
    if !values.print_errors.is_empty() {
        result["printErrors"] = json!(values.print_errors);
    }
    Ok(result)
}

fn restart_name(runtime: &Runtime, restart: &Root<'_>) -> Result<Option<String>, ObjectError> {
    let result = call(runtime, "COMMON-LISP", "RESTART-NAME", &[restart])?;
    Ok(result
        .first()
        .filter(|root| !root.is_nil())
        .and_then(|root| runtime.symbol_name(root)))
}

fn choose_restart<'rt>(
    runtime: &'rt Runtime,
    number: Option<usize>,
    name: Option<&str>,
) -> Result<Root<'rt>, ObjectError> {
    if number.is_some() == name.is_some() {
        return Err("choose a restart number or name".into());
    }
    let restarts = runtime.debug_restarts()?;
    if let Some(number) = number {
        return restarts
            .into_iter()
            .nth(
                number
                    .checked_sub(1)
                    .ok_or("restart numbers start at one")?,
            )
            .ok_or_else(|| "no such restart".into());
    }
    for restart in restarts {
        if restart_name(runtime, &restart)?
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name.unwrap()))
        {
            return Ok(restart);
        }
    }
    Err("no such restart is active".into())
}

fn frame_value<'rt>(
    io: &SessionIo,
    runtime: &'rt Runtime,
    frame: usize,
    input: &ValueInput,
) -> Result<Root<'rt>, ObjectError> {
    match input {
        ValueInput::Handle { handle } => io.objects.get(runtime, *handle),
        ValueInput::Source { source } => Ok(runtime
            .debug_eval(frame, source)?
            .into_iter()
            .next()
            .unwrap_or_else(|| runtime.nil())),
    }
}

fn execute(
    io: &SessionIo,
    runtime: &Runtime,
    selected: &mut usize,
    action: Action,
) -> Result<(Value, bool), ObjectError> {
    match action {
        Action::Frame { frame } | Action::Locals { frame: Some(frame) } => {
            if !runtime
                .debug_frames()?
                .iter()
                .any(|entry| entry.index == frame)
            {
                return Err("no such frame".into());
            }
            *selected = frame;
            let (_, variables) = variables(io, runtime, frame)?;
            Ok((json!({"selected":frame,"variables":variables}), false))
        }
        Action::Locals { frame: None } => {
            let (_, variables) = variables(io, runtime, *selected)?;
            Ok((json!({"selected":*selected,"variables":variables}), false))
        }
        Action::Eval {
            frame,
            source,
            render,
        } => {
            let values = runtime.debug_eval(frame.unwrap_or(*selected), &source)?;
            Ok((evaluation_result(io, runtime, &values, render)?, false))
        }
        Action::Local { frame, expression } => {
            let value = runtime.debug_find_local(frame.unwrap_or(*selected), &expression)?;
            let preview = value
                .as_ref()
                .map(|value| runtime.preview(value))
                .transpose()?;
            Ok((
                json!(preview.map(|preview| json!({"preview":preview}))),
                false,
            ))
        }
        Action::Set {
            frame,
            name,
            source,
            handle,
        } => {
            let value = match (source, handle) {
                (Some(source), None) => ValueInput::Source { source },
                (None, Some(handle)) => ValueInput::Handle { handle },
                _ => return Err("set requires either source or handle".into()),
            };
            let frame = frame.unwrap_or(*selected);
            let variable = runtime
                .debug_variables(frame)?
                .into_iter()
                .find(|variable| {
                    runtime
                        .repr(&variable.name)
                        .is_ok_and(|candidate| candidate == name)
                        || runtime
                            .symbol_name(&variable.name)
                            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(&name))
                })
                .ok_or("no such local variable")?;
            let value = frame_value(io, runtime, frame, &value)?;
            runtime.debug_set_local(frame, &variable.name, &value)?;
            Ok((io.objects.retain(runtime, &value)?, false))
        }
        Action::Restart {
            number,
            name,
            arguments,
        } => {
            let restart = choose_restart(runtime, number, name.as_deref())?;
            let arguments = arguments
                .iter()
                .map(|value| frame_value(io, runtime, *selected, value))
                .collect::<Result<Vec<_>, _>>()?;
            let arguments = arguments.iter().collect::<Vec<_>>();
            let values = runtime.debug_invoke_restart(&restart, &arguments)?;
            Ok((evaluation_result(io, runtime, &values, true)?, false))
        }
        Action::Motion { motion, release } => {
            let motion = match motion {
                Motion::Continue => DebugMotion::Continue,
                Motion::Into => DebugMotion::Into,
                Motion::Next => DebugMotion::Next,
                Motion::Out => DebugMotion::Out,
                Motion::Abort => DebugMotion::Abort,
            };
            if runtime.debug_motion(motion)? || motion == DebugMotion::Abort {
                io.objects.release(release);
                return Ok((Value::Null, true));
            }
            let restart = choose_restart(runtime, None, Some("CONTINUE"))?;
            let result = runtime.debug_invoke_restart(&restart, &[]);
            if result
                .as_ref()
                .is_err_and(|error| error.is_control_transfer())
            {
                io.objects.release(release);
            }
            result?;
            Ok((Value::Null, false))
        }
        Action::FrameReturn { frame, values } => {
            let values = values
                .iter()
                .map(|value| frame_value(io, runtime, frame.unwrap_or(*selected), value))
                .collect::<Result<Vec<_>, _>>()?;
            let values = values.iter().collect::<Vec<_>>();
            runtime.debug_return_frame(frame.unwrap_or(*selected), &values)?;
            Ok((Value::Null, true))
        }
        Action::FrameRestart {
            frame,
            stop,
            release,
        } => {
            let frame = frame.unwrap_or(*selected);
            if !runtime
                .debug_frames()?
                .iter()
                .any(|entry| entry.index == frame && entry.can_restart)
            {
                return Err("this frame cannot be restarted".into());
            }
            if stop {
                runtime.debug_motion(DebugMotion::Into)?;
            }
            let result = runtime.debug_restart_frame(frame);
            if result.is_ok()
                || result
                    .as_ref()
                    .is_err_and(|error| error.is_control_transfer())
            {
                io.objects.release(release);
            }
            result?;
            Ok((Value::Null, true))
        }
        Action::Object {
            object:
                ObjectRequest::Modify {
                    handle,
                    index,
                    value,
                },
        } => {
            let object = io.objects.get(runtime, handle)?;
            let value = frame_value(io, runtime, *selected, &value)?;
            runtime.replace_inspected_part(&object, index, &value)?;
            Ok((io.objects.retain(runtime, &value)?, false))
        }
        Action::Object { object } => Ok((io.objects.execute(runtime, object)?, false)),
        Action::Profile { profile } => Ok((profile.execute(runtime)?, false)),
        Action::Breakpoint { breakpoint } => Ok((
            breakpoint
                .execute(runtime)
                .map_err(|error| -> ObjectError { error.into() })?,
            false,
        )),
    }
}

fn text_action(text: &str, selected: usize) -> Result<Action, ObjectError> {
    let text = text.trim();
    let (command, args) = text
        .split_once(char::is_whitespace)
        .map(|(a, b)| (a, b.trim()))
        .unwrap_or((text, ""));
    match command.to_ascii_lowercase().as_str() {
        "" | "q" | "quit" | "a" | "abort" | ":abort" => Ok(Action::Motion {
            release: vec![],
            motion: Motion::Abort,
        }),
        "c" | "cont" | "continue" | ":continue" => Ok(Action::Motion {
            release: vec![],
            motion: Motion::Continue,
        }),
        "step" => Ok(Action::Motion {
            release: vec![],
            motion: Motion::Into,
        }),
        "next" => Ok(Action::Motion {
            release: vec![],
            motion: Motion::Next,
        }),
        "out" => Ok(Action::Motion {
            release: vec![],
            motion: Motion::Out,
        }),
        "frame" => Ok(Action::Frame {
            frame: args.parse()?,
        }),
        "up" => Ok(Action::Frame {
            frame: selected + 1,
        }),
        "down" => Ok(Action::Frame {
            frame: selected.checked_sub(1).ok_or("no such frame")?,
        }),
        "locals" | "list-locals" | "bt" | "backtrace" | "source" => {
            Ok(Action::Locals { frame: None })
        }
        "p" => Ok(Action::Eval {
            frame: None,
            source: args.to_owned(),
            render: true,
        }),
        "return" => Ok(Action::FrameReturn {
            frame: None,
            values: vec![ValueInput::Source {
                source: args.to_owned(),
            }],
        }),
        "restart-frame" => Ok(Action::FrameRestart {
            release: vec![],
            stop: false,
            frame: if args.is_empty() {
                None
            } else {
                Some(args.parse()?)
            },
        }),
        _ if text.starts_with('(') => Ok(Action::Eval {
            frame: None,
            source: text.to_owned(),
            render: true,
        }),
        _ => Ok(if let Ok(number) = text.parse() {
            Action::Restart {
                number: Some(number),
                name: None,
                arguments: vec![],
            }
        } else {
            Action::Restart {
                number: None,
                name: Some(text.to_owned()),
                arguments: vec![],
            }
        }),
    }
}

fn write_text(runtime: &Runtime, text: &str) -> Result<(), ObjectError> {
    let stream_symbol = runtime
        .lookup_symbol("COMMON-LISP", "*DEBUG-IO*")?
        .ok_or("*DEBUG-IO* unavailable")?;
    let stream = call(runtime, "COMMON-LISP", "SYMBOL-VALUE", &[&stream_symbol])?;
    let text = runtime.make_string(text)?;
    call(runtime, "COMMON-LISP", "WRITE-STRING", &[&text, &stream[0]])?;
    Ok(())
}

pub(super) fn run<'rt>(
    io: &SessionIo,
    runtime: &'rt Runtime,
    condition: &Root<'_>,
) -> NativeResult<'rt> {
    if io.is_printing_values() {
        return Ok(vec![]);
    }
    let condition_object = io.objects.retain(runtime, condition)?;
    io.record_condition(condition_object.clone());
    if !io.is_interactive() {
        return Ok(vec![]);
    }
    let condition_type = condition_object["type"].as_str().expect("condition type");
    let fault = call(runtime, "ZK", "%DEBUG-FAULT", &[])?;
    let message = fault
        .first()
        .and_then(|fault| runtime.cons_fields(fault))
        .and_then(|(message, _)| runtime.string(&message));
    let message = match message {
        Some(message) => message,
        None => {
            let result = call(runtime, "COMMON-LISP", "PRINC-TO-STRING", &[condition])?;
            runtime
                .string(&result[0])
                .unwrap_or_else(|| condition_type.to_owned())
        }
    };
    let mut selected = 0;
    let mut result = Value::Null;
    let mut payload = Value::Null;
    loop {
        if payload.is_null() {
            let frames = runtime.debug_frames()?;
            if !frames.is_empty() && selected >= frames.len() {
                selected = 0;
            }
            let frames=frames.iter().map(|frame|json!({
            "index":frame.index,"function":frame.function,"source":frame.source_path,"sourcePath":frame.source_path,
            "line":frame.line,"column":frame.column,"text":frame.source_text,"bytecodePc":frame.bytecode_pc,
            "native":frame.native,"localsAvailability":match &frame.locals{DebugLocalsAvailability::Available=>Value::Null,DebugLocalsAvailability::Unavailable(reason)=>json!(reason)},
            "canReturn":frame.can_return,"canRestart":frame.can_restart,
            "origin":frame.source.as_ref().map(crate::diagnostics::source_metadata_json),
            "snapshotId":frame.source.as_ref().map(zkcl::SourceDocument::id),
            "sourceId":frame.source.as_ref().map(zkcl::SourceDocument::id),
            "sourceText":frame.source.as_ref().and_then(zkcl::SourceDocument::text),
            "range":frame.source_range.as_ref().map(|range|json!({"start":range.start,"end":range.end})),
        })).collect::<Vec<_>>();
            let (locals, variables) = if frames.is_empty() {
                (json!([]), json!([]))
            } else {
                variables(io, runtime, selected)?
            };
            let restarts = runtime.debug_restarts()?;
            let reports = restarts
                .iter()
                .map(|restart| {
                    let values = call(runtime, "COMMON-LISP", "PRINC-TO-STRING", &[restart])?;
                    Ok(runtime.string(&values[0]))
                })
                .collect::<Result<Vec<_>, ObjectError>>()?;
            let names = restarts
                .iter()
                .map(|restart| restart_name(runtime, restart))
                .collect::<Result<Vec<_>, _>>()?;
            payload = json!({"condition":message,"conditionType":condition_type,"conditionObject":condition_object,
            "frames":frames,"restarts":reports,"restartNames":names,"selected":selected,"locals":locals,"variables":variables});
        }
        payload["result"] = result;
        let Some(answer) = io.prompt_action("debugger", &payload)? else {
            runtime.debug_motion(DebugMotion::Abort)?;
            return Ok(vec![]);
        };
        let text_reply = matches!(&answer, Answer::Text(_));
        let (id, action) = match answer {
            Answer::Text(text) => (None, text_action(&text, selected)),
            Answer::Action { id, action } => (
                Some(id),
                serde_json::from_value(action).map_err(|error| Box::new(error) as ObjectError),
            ),
        };
        // A hover lookup leaves the snapshot intact. Refreshing restart reports
        // here would invoke their Lisp report functions on every hover.
        if !matches!(&action, Ok(Action::Local { .. })) {
            payload = Value::Null;
        }
        let action_result = action.and_then(|action| execute(io, runtime, &mut selected, action));
        let (value, error, resumed, transfer) = match action_result {
            Ok((value, resumed)) => (value, Value::Null, resumed, None),
            Err(error)
                if error
                    .downcast_ref::<EvalError>()
                    .is_some_and(EvalError::is_control_transfer) =>
            {
                (Value::Null, Value::Null, true, Some(error))
            }
            Err(error) => (Value::Null, super::boxed_error(&*error), false, None),
        };
        if text_reply {
            if let Some(message) = error["message"].as_str() {
                write_text(runtime, &format!("error: {message}\n"))?;
            }
            if let Some(errors) = value["printErrors"].as_array() {
                for error in errors {
                    if let Some(message) = error["error"]["message"].as_str() {
                        write_text(runtime, &format!("error: printing result: {message}\n"))?;
                    }
                }
            }
            if let Some(values) = value["renderedValues"].as_array() {
                if values.is_empty() {
                    write_text(runtime, "No values.\n")?;
                }
                for (index, rendered) in values.iter().filter_map(Value::as_str).enumerate() {
                    write_text(
                        runtime,
                        &format!("{} {rendered}\n", if index == 0 { "=>" } else { "  " }),
                    )?;
                }
            }
        }
        result = json!({"id":id,"value":value,"error":error});
        if let Some(id) = id {
            io.protocol.send(
                &json!({"id":id,"event":"debugger-action","result":result,"resumed":resumed}),
            )?;
        }
        if let Some(transfer) = transfer {
            return Err(transfer);
        }
        if resumed {
            return Ok(vec![]);
        }
    }
}
