//! Owned snapshots for host tooling. Every exposed Lisp object is a persistent GC root.
use crate::abi::{
    Error, SourceByteRange, Utf8View, boundary, free_owned_handle, require_mut_pointer,
    require_pointer, write_owned_handle,
};
use crate::{Callback, CallbackReturns, Values, bind_call_arguments, bind_value_to_runtime};
use std::{ffi::c_void, path::PathBuf, ptr, time::Duration};
use zkcl::{
    DebugFrame, DebugLocalsAvailability, DebugMotion, DefinitionInfo, DefinitionKind, PauseHandle,
    PersistentRoot, ProfileReport, ProfilerConfig, Runtime, SourceDocument,
};

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_source_new(
    text: Utf8View,
    path: Utf8View,
    uri: Utf8View,
    version: Option<&i64>,
    base_offset: usize,
    out: Option<&mut *mut SourceDocument>,
) -> *mut Error {
    write_owned_handle(out, || {
        let path = unsafe { path.read_utf8()? };
        let uri = unsafe { uri.read_utf8()? };
        let mut source = SourceDocument::new(
            (!path.is_empty()).then(|| PathBuf::from(path)),
            Some(unsafe { text.read_utf8()? }.to_owned()),
        )
        .with_base_offset(base_offset);
        if !uri.is_empty() {
            source = source.with_uri(uri.to_owned());
        }
        if let Some(version) = version {
            source = source.with_version(*version);
        }
        Ok(source)
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_source_retain(
    source: Option<&SourceDocument>,
    out: Option<&mut *mut SourceDocument>,
) -> *mut Error {
    write_owned_handle(out, || Ok(require_pointer(source)?.clone()))
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_source_free(source: *mut SourceDocument) {
    unsafe { free_owned_handle(source) }
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_source_id(source: Option<&SourceDocument>) -> u64 {
    source.map_or(0, SourceDocument::id)
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_source_text(source: Option<&SourceDocument>) -> Utf8View {
    Utf8View::from_str(source.and_then(SourceDocument::text).unwrap_or(""))
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_source_uri(source: Option<&SourceDocument>) -> Utf8View {
    Utf8View::from_str(source.and_then(SourceDocument::uri).unwrap_or(""))
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_source_path(
    source: Option<&SourceDocument>,
    out: Option<&mut *mut String>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(require_pointer(source)?
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default())
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_source_version(source: Option<&SourceDocument>, out: Option<&mut i64>) -> bool {
    if let (Some(version), Some(out)) = (source.and_then(SourceDocument::version), out) {
        *out = version;
        true
    } else {
        false
    }
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_source_base_offset(source: Option<&SourceDocument>) -> usize {
    source.map_or(0, SourceDocument::base_offset)
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_source_absolute_range(
    source: Option<&SourceDocument>,
    range: SourceByteRange,
    out: Option<&mut std::mem::MaybeUninit<SourceByteRange>>,
) -> *mut Error {
    boundary(|| {
        let range = require_pointer(source)?
            .absolute_range(range.start..range.end)
            .ok_or_else(|| {
                Error::invalid("source range is invalid or overflows its document offset")
            })?;
        require_pointer(out)?.write(SourceByteRange {
            start: range.start,
            end: range.end,
        });
        Ok(())
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_eval_document(
    rt: Option<&Runtime>,
    source: Option<&SourceDocument>,
    out: Option<&mut *mut Values>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(Values::from_roots(
            require_pointer(rt)?.eval_document(require_pointer(source)?)?,
        ))
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_load_document(
    rt: Option<&Runtime>,
    source: Option<&SourceDocument>,
) -> *mut Error {
    boundary(|| Ok(require_pointer(rt)?.load_document(require_pointer(source)?)?))
}

struct Definitions(Vec<DefinitionInfo>);
#[repr(C)]
struct DefinitionView {
    name: Utf8View,
    kind: u32,
    source: *const SourceDocument,
    range: SourceByteRange,
    lambda_list: Utf8View,
    declared_type: Utf8View,
    documentation: Utf8View,
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_find_definitions(
    rt: Option<&Runtime>,
    name: Utf8View,
    out: Option<&mut *mut Definitions>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(Definitions(
            require_pointer(rt)?.find_definitions(unsafe { name.read_utf8()? })?,
        ))
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_definition_info(
    rt: Option<&Runtime>,
    value: Option<&PersistentRoot>,
    out: Option<&mut *mut Definitions>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        Ok(Definitions(
            rt.definition_info(&bind_value_to_runtime(rt, value)?)?,
        ))
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_definitions_len(items: Option<&Definitions>) -> usize {
    items.map_or(0, |v| v.0.len())
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_definitions_get(
    items: Option<&Definitions>,
    index: usize,
    out: Option<&mut std::mem::MaybeUninit<DefinitionView>>,
) -> *mut Error {
    boundary(|| {
        let v = require_pointer(items)?
            .0
            .get(index)
            .ok_or_else(|| Error::invalid("definition index is out of range"))?;
        require_pointer(out)?.write(DefinitionView {
            name: Utf8View::from_str(&v.name),
            kind: match v.kind {
                DefinitionKind::Function => 0,
                DefinitionKind::Macro => 1,
                DefinitionKind::Method => 2,
                DefinitionKind::Class => 3,
                DefinitionKind::Variable => 4,
                DefinitionKind::Constant => 5,
                DefinitionKind::GenericFunction => 6,
            },
            source: v.source.as_ref().map_or(ptr::null(), ptr::from_ref),
            range: SourceByteRange {
                start: v.byte_range.start,
                end: v.byte_range.end,
            },
            lambda_list: Utf8View::from_str(v.lambda_list.as_deref().unwrap_or("")),
            declared_type: Utf8View::from_str(v.declared_type.as_deref().unwrap_or("")),
            documentation: Utf8View::from_str(v.documentation.as_deref().unwrap_or("")),
        });
        Ok(())
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_definitions_free(items: *mut Definitions) {
    unsafe { free_owned_handle(items) }
}

struct Inspection {
    summary: String,
    total: usize,
    parts: Vec<InspectionPart>,
}
struct InspectionPart {
    index: usize,
    label: String,
    value: Option<PersistentRoot>,
    writable: bool,
}
#[repr(C)]
struct InspectionPartView {
    index: usize,
    label: Utf8View,
    value: *const PersistentRoot,
    writable: bool,
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_inspect(
    rt: Option<&Runtime>,
    value: Option<&PersistentRoot>,
    start: usize,
    count: usize,
    out: Option<&mut *mut Inspection>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        let inspection = rt.inspect_object(&bind_value_to_runtime(rt, value)?, start, count)?;
        Ok(Inspection {
            summary: inspection.summary,
            total: inspection.total,
            parts: inspection
                .parts
                .into_iter()
                .map(|p| InspectionPart {
                    index: p.index,
                    label: p.label,
                    value: p.value.map(|v| v.into_persistent()),
                    writable: p.writable,
                })
                .collect(),
        })
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_inspection_summary(v: Option<&Inspection>) -> Utf8View {
    Utf8View::from_str(v.map_or("", |v| v.summary.as_str()))
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_inspection_total(v: Option<&Inspection>) -> usize {
    v.map_or(0, |v| v.total)
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_inspection_len(v: Option<&Inspection>) -> usize {
    v.map_or(0, |v| v.parts.len())
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_inspection_get(
    v: Option<&Inspection>,
    index: usize,
    out: Option<&mut std::mem::MaybeUninit<InspectionPartView>>,
) -> *mut Error {
    boundary(|| {
        let p = require_pointer(v)?
            .parts
            .get(index)
            .ok_or_else(|| Error::invalid("inspection index is out of range"))?;
        require_pointer(out)?.write(InspectionPartView {
            index: p.index,
            label: Utf8View::from_str(&p.label),
            value: p.value.as_ref().map_or(ptr::null(), ptr::from_ref),
            writable: p.writable,
        });
        Ok(())
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_inspection_free(v: *mut Inspection) {
    unsafe { free_owned_handle(v) }
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_inspection_replace(
    rt: Option<&Runtime>,
    object: Option<&PersistentRoot>,
    index: usize,
    value: Option<&PersistentRoot>,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        Ok(rt.replace_inspected_part(
            &bind_value_to_runtime(rt, object)?,
            index,
            &bind_value_to_runtime(rt, value)?,
        )?)
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_pause_new(
    rt: Option<&Runtime>,
    out: Option<&mut *mut PauseHandle>,
) -> *mut Error {
    write_owned_handle(out, || Ok(require_pointer(rt)?.pause_handle()))
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_pause(handle: Option<&PauseHandle>) {
    if let Some(handle) = handle {
        handle.pause();
    }
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_pause_reset(handle: Option<&PauseHandle>) {
    if let Some(handle) = handle {
        handle.reset();
    }
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_pause_pending(handle: Option<&PauseHandle>) -> bool {
    handle.is_some_and(PauseHandle::is_pending)
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_pause_free(handle: *mut PauseHandle) {
    unsafe { free_owned_handle(handle) }
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_set_debugger(
    rt: Option<&Runtime>,
    callback: Option<Callback>,
    userdata: *mut c_void,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        let Some(callback) = callback else {
            rt.set_debugger(None)?;
            return Ok(());
        };
        let userdata = crate::CUserdata::new(userdata);
        rt.register_function("ZK::%C-HOST-DEBUGGER", move |rt, args| {
            let args = Values::from_roots(args.iter().cloned());
            let mut returns = CallbackReturns {
                runtime: rt,
                roots: Vec::new(),
            };
            let error = unsafe { callback(rt, &args, &raw mut returns, userdata.as_ptr()) };
            unsafe { crate::abi::check_foreign_error(error)? };
            Ok(returns.roots)
        })?;
        let name = rt
            .lookup_symbol("ZK", "%C-HOST-DEBUGGER")?
            .expect("registered host debugger");
        let function = rt.resolve_function(&name)?;
        rt.set_debugger(Some(&function))?;
        Ok(())
    })
}
pub(super) struct DebugFrames(pub(super) Vec<DebugFrame>);
#[repr(C)]
struct DebugFrameView {
    index: usize,
    function: Utf8View,
    path: Utf8View,
    line: usize,
    column: usize,
    source_text: Utf8View,
    bytecode_pc: usize,
    has_bytecode_pc: bool,
    native: bool,
    locals_available: bool,
    locals_reason: Utf8View,
    can_return: bool,
    can_restart: bool,
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_frames(
    rt: Option<&Runtime>,
    out: Option<&mut *mut DebugFrames>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(DebugFrames(require_pointer(rt)?.debug_frames()?))
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_frames_len(v: Option<&DebugFrames>) -> usize {
    v.map_or(0, |v| v.0.len())
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_frames_get(
    v: Option<&DebugFrames>,
    index: usize,
    out: Option<&mut std::mem::MaybeUninit<DebugFrameView>>,
) -> *mut Error {
    boundary(|| {
        let f = require_pointer(v)?
            .0
            .get(index)
            .ok_or_else(|| Error::invalid("frame index is out of range"))?;
        let reason = match &f.locals {
            DebugLocalsAvailability::Available => "",
            DebugLocalsAvailability::Unavailable(reason) => reason,
        };
        require_pointer(out)?.write(DebugFrameView {
            index: f.index,
            function: Utf8View::from_str(&f.function),
            path: Utf8View::from_str(f.source_path.as_deref().unwrap_or("")),
            line: f.line.unwrap_or(0),
            column: f.column.unwrap_or(0),
            source_text: Utf8View::from_str(f.source_text.as_deref().unwrap_or("")),
            bytecode_pc: f.bytecode_pc.unwrap_or(0),
            has_bytecode_pc: f.bytecode_pc.is_some(),
            native: f.native,
            locals_available: matches!(f.locals, DebugLocalsAvailability::Available),
            locals_reason: Utf8View::from_str(reason),
            can_return: f.can_return,
            can_restart: f.can_restart,
        });
        Ok(())
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_debug_frames_free(v: *mut DebugFrames) {
    unsafe { free_owned_handle(v) }
}
struct DebugVariables(Vec<DebugVariable>);
struct DebugVariable {
    name: PersistentRoot,
    value: Option<PersistentRoot>,
    unavailable: Option<String>,
    writable: bool,
}
#[repr(C)]
struct DebugVariableView {
    name: *const PersistentRoot,
    value: *const PersistentRoot,
    unavailable: Utf8View,
    writable: bool,
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_variables(
    rt: Option<&Runtime>,
    frame: usize,
    out: Option<&mut *mut DebugVariables>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(DebugVariables(
            require_pointer(rt)?
                .debug_variables(frame)?
                .into_iter()
                .map(|v| DebugVariable {
                    name: v.name.into_persistent(),
                    value: v.value.map(|v| v.into_persistent()),
                    unavailable: v.unavailable,
                    writable: v.writable,
                })
                .collect(),
        ))
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_variables_len(v: Option<&DebugVariables>) -> usize {
    v.map_or(0, |v| v.0.len())
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_variables_get(
    v: Option<&DebugVariables>,
    index: usize,
    out: Option<&mut std::mem::MaybeUninit<DebugVariableView>>,
) -> *mut Error {
    boundary(|| {
        let v = require_pointer(v)?
            .0
            .get(index)
            .ok_or_else(|| Error::invalid("variable index is out of range"))?;
        require_pointer(out)?.write(DebugVariableView {
            name: ptr::from_ref(&v.name),
            value: v.value.as_ref().map_or(ptr::null(), ptr::from_ref),
            unavailable: Utf8View::from_str(v.unavailable.as_deref().unwrap_or("")),
            writable: v.writable,
        });
        Ok(())
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_debug_variables_free(v: *mut DebugVariables) {
    unsafe { free_owned_handle(v) }
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_local(
    rt: Option<&Runtime>,
    frame: usize,
    name: Option<&PersistentRoot>,
    out: Option<&mut *mut PersistentRoot>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        Ok(rt
            .debug_local(frame, &bind_value_to_runtime(rt, name)?)?
            .into_persistent())
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_set_local(
    rt: Option<&Runtime>,
    frame: usize,
    name: Option<&PersistentRoot>,
    value: Option<&PersistentRoot>,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        Ok(rt.debug_set_local(
            frame,
            &bind_value_to_runtime(rt, name)?,
            &bind_value_to_runtime(rt, value)?,
        )?)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_debug_eval(
    rt: Option<&Runtime>,
    frame: usize,
    source: Utf8View,
    out: Option<&mut *mut Values>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(Values::from_roots(
            require_pointer(rt)?.debug_eval(frame, unsafe { source.read_utf8()? })?,
        ))
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_motion(
    rt: Option<&Runtime>,
    motion: u32,
    resumable: Option<&mut bool>,
) -> *mut Error {
    boundary(|| {
        let motion = match motion {
            0 => DebugMotion::Continue,
            1 => DebugMotion::Into,
            2 => DebugMotion::Next,
            3 => DebugMotion::Out,
            4 => DebugMotion::Abort,
            _ => return Err(Error::invalid("unknown debugger motion")),
        };
        *require_pointer(resumable)? = require_pointer(rt)?.debug_motion(motion)?;
        Ok(())
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_restarts(
    rt: Option<&Runtime>,
    out: Option<&mut *mut Values>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(Values::from_roots(require_pointer(rt)?.debug_restarts()?))
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_debug_invoke_restart(
    rt: Option<&Runtime>,
    restart: Option<&PersistentRoot>,
    argv: *const *const PersistentRoot,
    argc: usize,
    out: Option<&mut *mut Values>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        let restart = bind_value_to_runtime(rt, restart)?;
        let args = unsafe { bind_call_arguments(rt, argv, argc)? };
        Ok(Values::from_roots(rt.debug_invoke_restart(
            &restart,
            &args.iter().collect::<Vec<_>>(),
        )?))
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_debug_return_frame(
    rt: Option<&Runtime>,
    frame: usize,
    argv: *const *const PersistentRoot,
    argc: usize,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        let args = unsafe { bind_call_arguments(rt, argv, argc)? };
        Ok(rt.debug_return_frame(frame, &args.iter().collect::<Vec<_>>())?)
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_restart_frame(rt: Option<&Runtime>, frame: usize) -> *mut Error {
    boundary(|| Ok(require_pointer(rt)?.debug_restart_frame(frame)?))
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_add_breakpoint(
    rt: Option<&Runtime>,
    function: Option<&PersistentRoot>,
    pc: Option<&usize>,
    out: Option<&mut u64>,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        let out = require_pointer(out)?;
        *out = rt
            .add_breakpoint(&bind_value_to_runtime(rt, function)?, pc.copied())?
            .id;
        Ok(())
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_add_source_breakpoint(
    rt: Option<&Runtime>,
    function: Option<&PersistentRoot>,
    line: usize,
    out: Option<&mut u64>,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        let out = require_pointer(out)?;
        *out = rt
            .add_source_breakpoint(&bind_value_to_runtime(rt, function)?, line)?
            .id;
        Ok(())
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_remove_breakpoint(
    rt: Option<&Runtime>,
    id: u64,
    out: Option<&mut bool>,
) -> *mut Error {
    boundary(|| {
        *require_pointer(out)? = require_pointer(rt)?.remove_breakpoint(id);
        Ok(())
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_save_image(rt: Option<&Runtime>, path: Utf8View) -> *mut Error {
    boundary(|| {
        Ok(require_pointer(rt)?.save_image(std::path::Path::new(unsafe { path.read_utf8()? }))?)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_restore_image(rt: *mut Runtime, path: Utf8View) -> *mut Error {
    boundary(|| {
        // C supplies exclusive access, including no live evaluation handles.
        Ok(unsafe { require_mut_pointer(rt)? }
            .restore_image(std::path::Path::new(unsafe { path.read_utf8()? }))?)
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_profile_start(rt: Option<&Runtime>, sample_interval_ns: u64) -> *mut Error {
    boundary(|| {
        Ok(require_pointer(rt)?.start_profiling(ProfilerConfig {
            sample_interval: Duration::from_nanos(sample_interval_ns),
        })?)
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_profile_report(
    rt: Option<&Runtime>,
    stop: bool,
    out: Option<&mut *mut ProfileReport>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        Ok(if stop {
            rt.stop_profiling()
        } else {
            rt.profile_report()
        })
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_profile_reset(rt: Option<&Runtime>) -> *mut Error {
    boundary(|| {
        require_pointer(rt)?.reset_profiling();
        Ok(())
    })
}
#[repr(C)]
struct ProfileView {
    running: bool,
    elapsed_ns: u64,
    sampling_mode: Utf8View,
    samples: u64,
    allocated_bytes: u64,
    collections: u64,
    gc_time_ns: u64,
    function_count: usize,
}
#[repr(C)]
struct FunctionProfileView {
    id: u64,
    name: Utf8View,
    calls: u64,
    native_calls: u64,
    total_time_ns: u64,
    self_time_ns: u64,
    samples: u64,
    allocated_bytes: u64,
    self_allocated_bytes: u64,
}
fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_profile_view(
    v: Option<&ProfileReport>,
    out: Option<&mut std::mem::MaybeUninit<ProfileView>>,
) -> *mut Error {
    boundary(|| {
        let v = require_pointer(v)?;
        require_pointer(out)?.write(ProfileView {
            running: v.running,
            elapsed_ns: nanos(v.elapsed),
            sampling_mode: Utf8View::from_str(v.sampling_mode),
            samples: v.samples,
            allocated_bytes: v.allocated_bytes,
            collections: v.collections,
            gc_time_ns: nanos(v.gc_time),
            function_count: v.functions.len(),
        });
        Ok(())
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_profile_function(
    v: Option<&ProfileReport>,
    index: usize,
    out: Option<&mut std::mem::MaybeUninit<FunctionProfileView>>,
) -> *mut Error {
    boundary(|| {
        let v = require_pointer(v)?
            .functions
            .get(index)
            .ok_or_else(|| Error::invalid("profile index is out of range"))?;
        require_pointer(out)?.write(FunctionProfileView {
            id: v.id,
            name: Utf8View::from_str(&v.name),
            calls: v.calls,
            native_calls: v.native_calls,
            total_time_ns: nanos(v.total_time),
            self_time_ns: nanos(v.self_time),
            samples: v.samples,
            allocated_bytes: v.allocated_bytes,
            self_allocated_bytes: v.self_allocated_bytes,
        });
        Ok(())
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_profile_free(v: *mut ProfileReport) {
    unsafe { free_owned_handle(v) }
}
