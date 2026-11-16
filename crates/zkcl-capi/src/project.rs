//! Project handles and instance-local environment configuration.
#[cfg(feature = "delivery")]
use crate::abi::free_owned_handle;
use crate::abi::{Error, Utf8View, boundary, input_slice, require_pointer, write_owned_handle};
use std::path::Path;
use zkcl::{PersistentRoot, Runtime, SymbolStatus};

fn host(error: impl std::fmt::Display) -> Error {
    Error::Host(error.to_string())
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_require_module(rt: Option<&Runtime>, name: Utf8View) -> *mut Error {
    boundary(|| Ok(require_pointer(rt)?.require_module(unsafe { name.read_utf8()? })?))
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_loaded_modules(
    rt: Option<&Runtime>,
    out: Option<&mut *mut String>,
) -> *mut Error {
    write_owned_handle(out, || {
        serde_json::to_string(&require_pointer(rt)?.loaded_modules()?).map_err(host)
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_add_module_directory(rt: Option<&Runtime>, path: Utf8View) -> *mut Error {
    boundary(|| {
        require_pointer(rt)?
            .add_module_directory(Path::new(unsafe { path.read_utf8()? }))
            .map_err(host)
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_set_working_directory(
    rt: Option<&Runtime>,
    path: Utf8View,
) -> *mut Error {
    boundary(|| {
        Ok(require_pointer(rt)?.set_working_directory(Path::new(unsafe { path.read_utf8()? }))?)
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_set_environment_variable(
    rt: Option<&Runtime>,
    name: Utf8View,
    value: Option<&Utf8View>,
) -> *mut Error {
    boundary(|| {
        require_pointer(rt)?
            .set_environment_variable(
                unsafe { name.read_utf8()? },
                value.map(|v| unsafe { v.read_utf8() }).transpose()?,
            )
            .map_err(host)
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_set_arguments(
    rt: Option<&Runtime>,
    arguments: *const Utf8View,
    count: usize,
) -> *mut Error {
    boundary(|| {
        let arguments = unsafe { input_slice(arguments, count)? }
            .iter()
            .map(|v| unsafe { v.read_utf8() }.map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()?;
        require_pointer(rt)?.set_command_line_arguments(arguments);
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_requested_exit_code(rt: Option<&Runtime>, out: Option<&mut u8>) -> *mut Error {
    boundary(|| {
        *require_pointer(out)? = require_pointer(rt)?.requested_exit_code();
        Ok(())
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_find_symbol(
    rt: Option<&Runtime>,
    package: Utf8View,
    name: Utf8View,
    status: Option<&mut u32>,
    out: Option<&mut *mut PersistentRoot>,
) -> *mut Error {
    boundary(|| {
        let out = require_pointer(out)?;
        *out = std::ptr::null_mut();
        let status = require_pointer(status)?;
        *status = 0;
        if let Some(found) = require_pointer(rt)?
            .find_symbol(unsafe { package.read_utf8()? }, unsafe {
                name.read_utf8()?
            })?
        {
            *status = match found.status {
                SymbolStatus::Internal => 1,
                SymbolStatus::External => 2,
                SymbolStatus::Inherited => 3,
            };
            *out = Box::into_raw(Box::new(found.symbol.into_persistent()));
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_namespace_snapshot(
    rt: Option<&Runtime>,
    out: Option<&mut *mut String>,
) -> *mut Error {
    write_owned_handle(out, || {
        let snapshot = require_pointer(rt)?.namespace_snapshot();
        let packages: Vec<_> = snapshot.packages.iter().map(|p| serde_json::json!({
            "name":p.name,"nicknames":p.nicknames,"use_list":p.use_list,"used_by":p.used_by,
            "local_nicknames":p.local_nicknames,"external_count":p.external_count,"total_count":p.total_count,"is_current":p.is_current
        })).collect();
        let symbols: Vec<_> = snapshot
            .symbols
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id":s.id,"name":s.name,"package":s.package,"home_package":s.home_package,
                    "external":s.external,"accessible":s.accessible,
                    "bindings":s.bindings.iter().map(|binding| serde_json::json!({
                        "kind":format!("{:?}",binding.kind),"lambda_list":binding.lambda_list,
                        "documentation":binding.documentation
                    })).collect::<Vec<_>>()
                })
            })
            .collect();
        Ok(serde_json::json!({"current_package":snapshot.current_package,"packages":packages,"symbols":symbols,"local_nicknames":snapshot.local_nicknames}).to_string())
    })
}

#[cfg(feature = "delivery")]
mod bundles {
    use super::*;
    use zkcl::project::{ProjectBundle, asdf};

    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_bundle_open(
        path: Utf8View,
        out: Option<&mut *mut ProjectBundle>,
    ) -> *mut Error {
        write_owned_handle(out, || {
            ProjectBundle::open(Path::new(unsafe { path.read_utf8()? }))
                .map_err(|e| super::projects_error(&e))
        })
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_bundle_free(bundle: *mut ProjectBundle) {
        unsafe { free_owned_handle(bundle) }
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_bundle_load(
        bundle: Option<&ProjectBundle>,
        rt: Option<&Runtime>,
        output: Utf8View,
    ) -> *mut Error {
        boundary(|| {
            require_pointer(bundle)?
                .load(
                    require_pointer(rt)?,
                    Path::new(unsafe { output.read_utf8()? }),
                )
                .map_err(|e| super::projects_error(&e))
        })
    }
    #[unsafe(no_mangle)]
    extern "C" fn zkcl_bundle_call(
        bundle: Option<&ProjectBundle>,
        rt: Option<&Runtime>,
        out: Option<&mut *mut crate::Values>,
    ) -> *mut Error {
        write_owned_handle(out, || {
            Ok(crate::Values::from_roots(
                require_pointer(bundle)?
                    .call_entry(require_pointer(rt)?)
                    .map_err(|e| super::projects_error(&e))?,
            ))
        })
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_asdf_operate(
        rt: Option<&Runtime>,
        system: Utf8View,
        operation: u32,
    ) -> *mut Error {
        boundary(|| {
            let operation = match operation {
                0 => asdf::AsdfOperation::Load,
                1 => asdf::AsdfOperation::Compile,
                2 => asdf::AsdfOperation::Test,
                _ => return Err(Error::invalid("unknown ASDF operation")),
            };
            asdf::operate(
                require_pointer(rt)?,
                unsafe { system.read_utf8()? },
                operation,
            )
            .map_err(|e| super::projects_error(e.as_ref()))
        })
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_asdf_system_info(
        rt: Option<&Runtime>,
        system: Utf8View,
        out: Option<&mut *mut String>,
    ) -> *mut Error {
        write_owned_handle(out, || {
            let info = asdf::system_info(require_pointer(rt)?, unsafe { system.read_utf8()? })
                .map_err(|e| super::projects_error(e.as_ref()))?;
            serde_json::to_string(&info).map_err(host)
        })
    }
}

#[cfg(feature = "delivery")]
fn projects_error(error: &(dyn std::error::Error + 'static)) -> Error {
    let message = error.to_string();
    let mut causes = Vec::new();
    let mut source = Some(error);
    let mut location = None;
    while let Some(error) = source {
        causes.push(error.to_string());
        if let Some(lisp) = error.downcast_ref::<zkcl::EvalError>() {
            location = Some(
                serde_json::json!({"kind":format!("{:?}",lisp.kind()),"range":[lisp.byte_range().start,lisp.byte_range().end],
                "path":lisp.source_document().and_then(|s| s.path()).map(|p| p.to_string_lossy()),"source_id":lisp.source_document().map(|s|s.id())}),
            );
        }
        #[cfg(feature = "project")]
        if let Some(zkcl::project::ProjectMetadataFileError::Parse {
            document,
            source: parse,
        }) = error.downcast_ref::<zkcl::project::ProjectMetadataFileError>()
        {
            location = Some(
                serde_json::json!({"path":document.path(),"range":[parse.span.start,parse.span.end],"kind":"ProjectMetadata"}),
            );
        }
        source = error.source();
    }
    Error::Project {
        message,
        details: serde_json::json!({"causes":causes,"location":location}).to_string(),
    }
}

#[cfg(feature = "project")]
mod projects {
    use super::*;
    use zkcl::project::{
        LoadOptions, LoadedProject, Project, ProjectSession, ResolveOptions, TargetSelection,
    };

    type BuildInitializer =
        unsafe extern "C" fn(*const Runtime, *mut std::ffi::c_void) -> *mut Error;

    #[repr(C)]
    struct EnvironmentVariable {
        name: Utf8View,
        value: *const Utf8View,
    }

    #[repr(C)]
    struct ProjectOptions {
        locked: bool,
        offline: bool,
        target_kind: u32,
        binary: Utf8View,
        target: Utf8View,
        registries: *const Utf8View,
        registry_count: usize,
        features: *const Utf8View,
        feature_count: usize,
        output_directory: Utf8View,
        allow_build_foreign_calls: bool,
        build_environment: *const EnvironmentVariable,
        build_environment_count: usize,
        initialize_build: Option<BuildInitializer>,
        build_userdata: *mut std::ffi::c_void,
    }

    impl ProjectOptions {
        unsafe fn read(options: Option<&Self>) -> Result<LoadOptions, Error> {
            let Some(options) = options else {
                return Ok(LoadOptions::default());
            };
            let mut result = LoadOptions::default();
            result.resolve = ResolveOptions {
                locked: options.locked,
                offline: options.offline,
                registries: unsafe { input_slice(options.registries, options.registry_count)? }
                    .iter()
                    .map(|s| unsafe { s.read_utf8() }.map(Into::into))
                    .collect::<Result<_, _>>()?,
                features: unsafe { input_slice(options.features, options.feature_count)? }
                    .iter()
                    .map(|s| unsafe { s.read_utf8() }.map(str::to_owned))
                    .collect::<Result<_, _>>()?,
                target: match unsafe { options.target.read_utf8()? } {
                    "" => None,
                    text => Some(text.to_owned()),
                },
            };
            let name = unsafe { options.binary.read_utf8()? };
            result.target = match options.target_kind {
                0 => TargetSelection::Library,
                1 => TargetSelection::Binary((!name.is_empty()).then(|| name.to_owned())),
                2 => TargetSelection::Test,
                _ => return Err(Error::invalid("unknown project target kind")),
            };
            let directory = unsafe { options.output_directory.read_utf8()? };
            result.output_directory = (!directory.is_empty()).then(|| directory.into());
            if options.allow_build_foreign_calls {
                #[cfg(feature = "ffi")]
                {
                    result.build.runtime = unsafe { result.build.runtime.allow_foreign_calls() };
                }
                #[cfg(not(feature = "ffi"))]
                return Err(Error::invalid(
                    "zkcl-capi was built without the ffi feature",
                ));
            }
            for variable in
                unsafe { input_slice(options.build_environment, options.build_environment_count)? }
            {
                result.build.environment.insert(
                    unsafe { variable.name.read_utf8()? }.to_owned(),
                    unsafe { variable.value.as_ref() }
                        .map(|v| unsafe { v.read_utf8() }.map(str::to_owned))
                        .transpose()?,
                );
            }
            if let Some(initialize) = options.initialize_build {
                let userdata = options.build_userdata;
                result.build.initialize_runtime = Some(std::rc::Rc::new(move |runtime| {
                    // SAFETY: C keeps callback/userdata alive through project loading.
                    unsafe { crate::abi::check_foreign_error(initialize(runtime, userdata)) }
                        .map_err(|e| e as Box<dyn std::error::Error>)
                }));
            }
            Ok(result)
        }
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_project_open(
        path: Utf8View,
        out: Option<&mut *mut Project>,
    ) -> *mut Error {
        write_owned_handle(out, || {
            Project::open(Path::new(unsafe { path.read_utf8()? }))
                .map_err(|e| super::projects_error(e.as_ref()))
        })
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_project_free(project: *mut Project) {
        unsafe { free_owned_handle(project) }
    }
    #[unsafe(no_mangle)]
    extern "C" fn zkcl_project_info(
        project: Option<&Project>,
        out: Option<&mut *mut String>,
    ) -> *mut Error {
        write_owned_handle(out, || {
            let project = require_pointer(project)?;
            Ok(serde_json::json!({"name":project.manifest.name.as_str(),"version":project.manifest.version.to_string(),
                "root":project.root,"manifest":project.manifest_path,"packages":project.manifest.packages,"systems":project.manifest.systems,
                "targets":project.manifest.targets.iter().map(|t| serde_json::json!({"kind":format!("{:?}",t.kind),"name":t.name,"files":t.files,"entry":t.entry,"suite":t.suite})).collect::<Vec<_>>()}).to_string())
        })
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_project_resolve(
        project: Option<&Project>,
        options: Option<&ProjectOptions>,
        out: Option<&mut *mut String>,
    ) -> *mut Error {
        write_owned_handle(out, || {
            let options = unsafe { ProjectOptions::read(options)? };
            let resolved = require_pointer(project)?
                .resolve(&options.resolve)
                .map_err(|e| super::projects_error(e.as_ref()))?;
            Ok(resolved.locked_graph().render())
        })
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_project_load(
        project: Option<&Project>,
        rt: Option<&Runtime>,
        options: Option<&ProjectOptions>,
        out: Option<&mut *mut LoadedProject<'static>>,
    ) -> *mut Error {
        write_owned_handle(out, || {
            let runtime = require_pointer(rt)?;
            // SAFETY: the C contract keeps the runtime alive until the session is freed.
            let runtime: &'static Runtime = unsafe { &*(runtime as *const Runtime) };
            require_pointer(project)?
                .load(runtime, &unsafe { ProjectOptions::read(options)? })
                .map_err(|e| super::projects_error(e.as_ref()))
        })
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_project_session_free(session: *mut LoadedProject<'static>) {
        unsafe { free_owned_handle(session) }
    }
    #[unsafe(no_mangle)]
    extern "C" fn zkcl_project_call(
        session: Option<&LoadedProject<'static>>,
        out: Option<&mut *mut crate::Values>,
    ) -> *mut Error {
        write_owned_handle(out, || {
            Ok(crate::Values::from_roots(
                require_pointer(session)?
                    .call_entry()
                    .map_err(|e| super::projects_error(e.as_ref()))?,
            ))
        })
    }
    #[unsafe(no_mangle)]
    extern "C" fn zkcl_project_test(session: Option<&LoadedProject<'static>>) -> *mut Error {
        boundary(|| {
            require_pointer(session)?
                .run_tests()
                .map_err(|e| super::projects_error(e.as_ref()))
        })
    }
    #[unsafe(no_mangle)]
    extern "C" fn zkcl_project_write_lock(session: Option<&LoadedProject<'static>>) -> *mut Error {
        boundary(|| {
            require_pointer(session)?
                .resolved()
                .write_lock()
                .map_err(|e| super::projects_error(e.as_ref()))
        })
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn zkcl_project_write_bundle(
        session: Option<&LoadedProject<'static>>,
        path: Utf8View,
    ) -> *mut Error {
        boundary(|| {
            require_pointer(session)?
                .write_bundle(Path::new(unsafe { path.read_utf8()? }))
                .map_err(|e| super::projects_error(&e))?;
            Ok(())
        })
    }
}
