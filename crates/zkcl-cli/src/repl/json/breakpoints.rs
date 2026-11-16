use serde::Deserialize;
use serde_json::{Value, json};
use zkcl::{Breakpoint, BreakpointFollow, BreakpointOptions, BreakpointStatus, Runtime};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum BreakpointRequest {
    Add {
        function: String,
        pc: Option<usize>,
        line: Option<usize>,
        #[serde(default)]
        options: Options,
    },
    Source {
        path: std::path::PathBuf,
        line: usize,
        #[serde(default)]
        options: Options,
    },
    SourceSet {
        path: std::path::PathBuf,
        breakpoints: Vec<SourceBreakpoint>,
    },
    Update {
        number: u64,
        options: Options,
    },
    Enable {
        number: u64,
        enabled: bool,
    },
    Delete {
        number: u64,
    },
    Locations {
        function: String,
    },
    List,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceBreakpoint {
    line: usize,
    #[serde(default)]
    options: Options,
}
#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Options {
    enabled: bool,
    condition: Option<String>,
    hit_count: Option<u64>,
    hit_every: Option<u64>,
    log: Option<String>,
    log_only: bool,
    follow: Follow,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            enabled: true,
            condition: None,
            hit_count: None,
            hit_every: None,
            log: None,
            log_only: false,
            follow: Follow::Object,
        }
    }
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum Follow {
    #[default]
    Object,
    Name,
    Source,
}
impl From<Options> for BreakpointOptions {
    fn from(options: Options) -> Self {
        Self {
            enabled: options.enabled,
            condition: options.condition,
            hit_count: options.hit_count,
            hit_every: options.hit_every,
            log: options.log,
            log_only: options.log_only,
            follow: match options.follow {
                Follow::Object => BreakpointFollow::Object,
                Follow::Name => BreakpointFollow::Name,
                Follow::Source => BreakpointFollow::Source,
            },
        }
    }
}
fn encode(entry: Breakpoint) -> Value {
    let path = entry
        .requested_path
        .as_deref()
        .or_else(|| entry.source.as_ref().and_then(zkcl::SourceDocument::path));
    let line = entry.requested_line.or_else(|| {
        entry
            .source
            .as_ref()
            .and_then(zkcl::SourceDocument::text)
            .zip(entry.offset)
            .map(|(text, offset)| {
                text[..offset.min(text.len())]
                    .bytes()
                    .filter(|&byte| byte == b'\n')
                    .count()
                    + 1
            })
    });
    let (verified, message) = match entry.status {
        BreakpointStatus::Verified => (true, None),
        BreakpointStatus::Pending(reason) => (false, Some(reason)),
    };
    json!({"number":entry.id,"function":entry.function,"bytecodePc":entry.bytecode_pc,"offset":entry.offset,"sourceId":entry.source.as_ref().map(zkcl::SourceDocument::id),"sourcePath":path.map(|path|path.to_string_lossy()),"origin":entry.source.as_ref().map(crate::diagnostics::source_metadata_json),"line":line,"verified":verified,"message":message,"hits":entry.hits,
    "options":{"enabled":entry.options.enabled,"condition":entry.options.condition,"hitCount":entry.options.hit_count,"hitEvery":entry.options.hit_every,"log":entry.options.log,"logOnly":entry.options.log_only,"follow":match entry.options.follow{BreakpointFollow::Object=>"object",BreakpointFollow::Name=>"name",BreakpointFollow::Source=>"source"}}})
}
impl BreakpointRequest {
    pub(super) fn execute(self, runtime: &Runtime) -> Result<Value, String> {
        let error = |error: zkcl::EvalError| error.message().to_owned();
        match self {
            Self::Add {
                function,
                pc,
                line,
                options,
            } => {
                if pc.is_some() && line.is_some() {
                    return Err("choose either a source line or a bytecode instruction".into());
                }
                let function = crate::repl::debugger::read_function_name(runtime, &function)?;
                let entry = match line {
                    Some(line) => {
                        runtime.add_source_breakpoint_with_options(&function, line, options.into())
                    }
                    None => runtime.add_breakpoint_with_options(&function, pc, options.into()),
                }
                .map_err(error)?;
                Ok(encode(entry))
            }
            Self::Source {
                path,
                line,
                options,
            } => Ok(encode(
                runtime
                    .add_file_breakpoint(path, line, options.into())
                    .map_err(error)?,
            )),
            Self::SourceSet { path, breakpoints } => {
                let path = path.canonicalize().unwrap_or_else(|_| {
                    if path.is_absolute() {
                        path.clone()
                    } else {
                        std::env::current_dir().unwrap_or_default().join(&path)
                    }
                });
                for existing in runtime.breakpoints() {
                    if existing.requested_path.as_ref() == Some(&path) {
                        runtime.remove_breakpoint(existing.id);
                    }
                }
                Ok(Value::Array(breakpoints.into_iter().map(|entry|match runtime.add_file_breakpoint(&path,entry.line,entry.options.into()){
                    Ok(breakpoint)=>encode(breakpoint),Err(error)=>json!({"verified":false,"line":entry.line,"message":error.message()})
                }).collect()))
            }
            Self::Update { number, options } => Ok(encode(
                runtime
                    .update_breakpoint(number, options.into())
                    .map_err(error)?,
            )),
            Self::Enable { number, enabled } => Ok(encode(
                runtime.enable_breakpoint(number, enabled).map_err(error)?,
            )),
            Self::Delete { number } if runtime.remove_breakpoint(number) => Ok(Value::Null),
            Self::Delete { .. } => Err("no such breakpoint".into()),
            Self::List => Ok(Value::Array(
                runtime.breakpoints().into_iter().map(encode).collect(),
            )),
            Self::Locations { function } => {
                let function = crate::repl::debugger::read_function_name(runtime, &function)?;
                Ok(json!(runtime.executable_locations(&function).map_err(error)?.into_iter().map(|entry|json!({"bytecodePc":entry.bytecode_pc,"offset":entry.offset,"line":entry.line,"column":entry.column,"origin":entry.source.as_ref().map(crate::diagnostics::source_metadata_json)})).collect::<Vec<_>>()))
            }
        }
    }
}
