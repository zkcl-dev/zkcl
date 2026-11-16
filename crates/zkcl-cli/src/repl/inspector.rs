use std::io::Write;

use zkcl::{EvalError, NativeResult, Root, Runtime};

const PAGE_SIZE: usize = 24;

pub(super) fn install_terminal(runtime: &Runtime) -> Result<(), EvalError> {
    runtime.register_function("ZK::%REPL-INSPECT", inspect_terminal)?;
    let wrapper = runtime.eval("(CL:LAMBDA (OBJECT) (ZK::%REPL-INSPECT OBJECT CL:*DEBUG-IO*))")?;
    runtime.set_inspector(Some(&wrapper[0]))
}

fn inspect_terminal<'rt>(runtime: &'rt Runtime, args: &[Root<'rt>]) -> NativeResult<'rt> {
    let [object, stream] = args else {
        return Err("the inspector expects one object and a stream".into());
    };
    let write = runtime
        .lookup_symbol("CL", "WRITE-STRING")?
        .expect("WRITE-STRING");
    let read = runtime
        .lookup_symbol("CL", "READ-LINE")?
        .expect("READ-LINE");
    let flush = runtime
        .lookup_symbol("CL", "FINISH-OUTPUT")?
        .expect("FINISH-OUTPUT");
    inspect_session(runtime, object, |page, offset, _, notice| {
        let output = {
            let mut output = Vec::new();
            if let Some(notice) = notice {
                writeln!(output, "{notice}")?;
            }
            writeln!(output, "\n{}", super::single_line(&page.summary))?;
            for part in &page.parts {
                let value = part
                    .value
                    .as_ref()
                    .map(|value| runtime.preview(value))
                    .transpose()?;
                let label = super::single_line(&part.label);
                let value = value.as_deref().unwrap_or("<unbound>");
                writeln!(
                    output,
                    "  {}. {label} = {}",
                    part.index,
                    super::single_line(value)
                )?;
            }
            if page.total > PAGE_SIZE {
                writeln!(
                    output,
                    "  Showing {}–{} of {}",
                    offset + 1,
                    offset + page.parts.len(),
                    page.total
                )?;
            }
            write!(output, "inspect> ")?;
            runtime.make_string(&String::from_utf8(output)?)?
        };
        runtime.call(&write, &[&output, stream])?;
        runtime.call(&flush, &[stream])?;
        let values = runtime.call(&read, &[stream, &runtime.nil(), &runtime.nil()])?;
        Ok(values.first().and_then(|line| runtime.string(line)))
    })
}

pub(super) fn inspect_session<'rt>(
    runtime: &'rt Runtime,
    object: &Root<'rt>,
    mut read_command: impl FnMut(
        &zkcl::Inspection<'rt>,
        usize,
        usize,
        Option<&str>,
    )
        -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>>,
) -> NativeResult<'rt> {
    let mut path = vec![(object.clone(), 0usize)];
    let mut notice = None;
    loop {
        let (object, offset) = path.last().expect("inspector root");
        let page = runtime.inspect_object(object, *offset, PAGE_SIZE)?;
        let Some(input) = read_command(&page, *offset, path.len() - 1, notice.take().as_deref())?
        else {
            return Ok(Vec::new());
        };
        let input = input.trim();
        match input {
            "" | "q" | "quit" => return Ok(Vec::new()),
            "h" | "?" | "help" => {
                notice = Some("N enters a component. u goes back. n and p change pages. set N FORM replaces a writable component. q exits.".into());
            }
            "u" | "up" => {
                if path.len() > 1 {
                    path.pop();
                }
            }
            "n" | "next" => {
                if *offset + PAGE_SIZE < page.total {
                    path.last_mut().unwrap().1 += PAGE_SIZE;
                }
            }
            "p" | "prev" => {
                path.last_mut().unwrap().1 = offset.saturating_sub(PAGE_SIZE);
            }
            _ => {
                if let Some(args) = input.strip_prefix("set ") {
                    if let Err(error) = set_part(runtime, object, &page, args.trim_start()) {
                        notice = Some(format!("error: {error}"));
                    }
                } else if let Some(value) = input.parse::<usize>().ok().and_then(|index| {
                    page.parts
                        .into_iter()
                        .find(|part| part.index == index)
                        .and_then(|part| part.value)
                }) {
                    path.push((value, 0));
                } else {
                    notice = Some("Choose a component or enter h for help.".into());
                }
            }
        }
    }
}

fn set_part(
    runtime: &Runtime,
    object: &Root<'_>,
    page: &zkcl::Inspection<'_>,
    request: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (index, source) = request
        .split_once(char::is_whitespace)
        .ok_or("usage: set N FORM")?;
    let index = index.parse::<usize>()?;
    if !page
        .parts
        .iter()
        .any(|part| part.index == index && part.writable)
    {
        return Err("choose a writable component on this page".into());
    }
    let values = runtime.eval(source.trim_start())?;
    let value = values
        .first()
        .ok_or("the replacement form returned no value")?;
    runtime.replace_inspected_part(object, index, value)?;
    Ok(())
}
