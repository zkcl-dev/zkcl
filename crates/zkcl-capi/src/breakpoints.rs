use crate::abi::{
    Error, Utf8View, boundary, free_owned_handle, require_pointer, write_owned_handle,
};
use crate::bind_value_to_runtime;
use std::ptr;
use zkcl::{
    Breakpoint, BreakpointFollow, BreakpointLocation, BreakpointOptions, BreakpointStatus,
    PersistentRoot, Runtime, SourceDocument,
};

#[repr(C)]
struct Options {
    enabled: bool,
    condition: Utf8View,
    hit_count: u64,
    hit_every: u64,
    log: Utf8View,
    log_only: bool,
    follow: u32,
}
impl Options {
    unsafe fn read(&self) -> Result<BreakpointOptions, Error> {
        let condition = unsafe { self.condition.read_utf8()? };
        let log = unsafe { self.log.read_utf8()? };
        Ok(BreakpointOptions {
            enabled: self.enabled,
            condition: (!condition.is_empty()).then(|| condition.to_owned()),
            hit_count: (self.hit_count != 0).then_some(self.hit_count),
            hit_every: (self.hit_every != 0).then_some(self.hit_every),
            log: (!log.is_empty()).then(|| log.to_owned()),
            log_only: self.log_only,
            follow: match self.follow {
                0 => BreakpointFollow::Object,
                1 => BreakpointFollow::Name,
                2 => BreakpointFollow::Source,
                _ => return Err(Error::invalid("unknown breakpoint follow mode")),
            },
        })
    }
    fn view(options: &BreakpointOptions) -> Self {
        Self {
            enabled: options.enabled,
            condition: Utf8View::from_str(options.condition.as_deref().unwrap_or("")),
            hit_count: options.hit_count.unwrap_or(0),
            hit_every: options.hit_every.unwrap_or(0),
            log: Utf8View::from_str(options.log.as_deref().unwrap_or("")),
            log_only: options.log_only,
            follow: match options.follow {
                BreakpointFollow::Object => 0,
                BreakpointFollow::Name => 1,
                BreakpointFollow::Source => 2,
            },
        }
    }
}
struct Breakpoints(Vec<Breakpoint>);
#[repr(C)]
struct BreakpointView {
    id: u64,
    function: Utf8View,
    bytecode_pc: usize,
    source: *const SourceDocument,
    offset: usize,
    has_offset: bool,
    options: Options,
    hits: u64,
    verified: bool,
    pending_reason: Utf8View,
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_add_breakpoint_with_options(
    rt: Option<&Runtime>,
    function: Option<&PersistentRoot>,
    location: usize,
    source_line: bool,
    options: Option<&Options>,
    out: Option<&mut u64>,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        let out = require_pointer(out)?;
        let options = match options {
            Some(options) => unsafe { options.read()? },
            None => BreakpointOptions::default(),
        };
        let function = bind_value_to_runtime(rt, function)?;
        *out = if source_line {
            rt.add_source_breakpoint_with_options(&function, location, options)?
        } else {
            rt.add_breakpoint_with_options(&function, Some(location), options)?
        }
        .id;
        Ok(())
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_update_breakpoint(
    rt: Option<&Runtime>,
    id: u64,
    options: Option<&Options>,
) -> *mut Error {
    boundary(|| {
        require_pointer(rt)?.update_breakpoint(id, unsafe { require_pointer(options)?.read()? })?;
        Ok(())
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_enable_breakpoint(rt: Option<&Runtime>, id: u64, enabled: bool) -> *mut Error {
    boundary(|| {
        require_pointer(rt)?.enable_breakpoint(id, enabled)?;
        Ok(())
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_breakpoints(
    rt: Option<&Runtime>,
    out: Option<&mut *mut Breakpoints>,
) -> *mut Error {
    write_owned_handle(out, || Ok(Breakpoints(require_pointer(rt)?.breakpoints())))
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_breakpoints_len(v: Option<&Breakpoints>) -> usize {
    v.map_or(0, |v| v.0.len())
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_breakpoints_get(
    v: Option<&Breakpoints>,
    index: usize,
    out: Option<&mut std::mem::MaybeUninit<BreakpointView>>,
) -> *mut Error {
    boundary(|| {
        let v = require_pointer(v)?
            .0
            .get(index)
            .ok_or_else(|| Error::invalid("breakpoint index is out of range"))?;
        let (verified, reason) = match &v.status {
            BreakpointStatus::Verified => (true, ""),
            BreakpointStatus::Pending(reason) => (false, reason.as_str()),
        };
        require_pointer(out)?.write(BreakpointView {
            id: v.id,
            function: Utf8View::from_str(v.function.as_deref().unwrap_or("")),
            bytecode_pc: v.bytecode_pc,
            source: v.source.as_ref().map_or(ptr::null(), ptr::from_ref),
            offset: v.offset.unwrap_or(0),
            has_offset: v.offset.is_some(),
            options: Options::view(&v.options),
            hits: v.hits,
            verified,
            pending_reason: Utf8View::from_str(reason),
        });
        Ok(())
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_breakpoints_free(v: *mut Breakpoints) {
    unsafe { free_owned_handle(v) }
}
struct Locations(Vec<BreakpointLocation>);
#[repr(C)]
struct LocationView {
    bytecode_pc: usize,
    source: *const SourceDocument,
    offset: usize,
    has_offset: bool,
    line: usize,
    column: usize,
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_executable_locations(
    rt: Option<&Runtime>,
    function: Option<&PersistentRoot>,
    out: Option<&mut *mut Locations>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        Ok(Locations(rt.executable_locations(
            &bind_value_to_runtime(rt, function)?,
        )?))
    })
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_executable_locations_len(v: Option<&Locations>) -> usize {
    v.map_or(0, |v| v.0.len())
}
#[unsafe(no_mangle)]
extern "C" fn zkcl_executable_locations_get(
    v: Option<&Locations>,
    index: usize,
    out: Option<&mut std::mem::MaybeUninit<LocationView>>,
) -> *mut Error {
    boundary(|| {
        let v = require_pointer(v)?
            .0
            .get(index)
            .ok_or_else(|| Error::invalid("location index is out of range"))?;
        require_pointer(out)?.write(LocationView {
            bytecode_pc: v.bytecode_pc,
            source: v.source.as_ref().map_or(ptr::null(), ptr::from_ref),
            offset: v.offset.unwrap_or(0),
            has_offset: v.offset.is_some(),
            line: v.line.unwrap_or(0),
            column: v.column.unwrap_or(0),
        });
        Ok(())
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_executable_locations_free(v: *mut Locations) {
    unsafe { free_owned_handle(v) }
}
