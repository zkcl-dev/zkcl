//! Read-only project inspection and registry account commands.

use std::error::Error;
use std::process::ExitCode;

use serde_json::{Value, json};
use zkcl_project::manifest::{self, BuildTargetKind, DependencySource};
use zkcl_registry::remote::Control;

use super::{publishing, workspace::Project};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub(super) fn new_project(request: super::super::args::NewArgs) -> Result<ExitCode> {
    use std::{env, fs};

    let path = if request.path.is_absolute() {
        request.path
    } else {
        env::current_dir()?.join(request.path)
    };
    let name = request.name.as_str();
    let binary = name.split_once('/').expect("validated module name").1;
    let package = name.replace('/', ".").to_ascii_uppercase();
    let manifest = format!(
        "(project \"{name}\"\n  (version \"0.1.0\")\n  (packages \"{package}\")\n  (repl (package \"{package}\"))\n  (binary \"{binary}\"\n    (files \"src/main.lisp\")\n    (entry \"{package}:MAIN\")))\n"
    );
    manifest::parse_manifest(&manifest)?;
    let parent = path.parent().ok_or("project directory requires a parent")?;
    fs::create_dir_all(parent)?;
    let staged = tempfile::Builder::new()
        .prefix(".zkcl-new-")
        .tempdir_in(parent)?;
    let files = [
        ("zkcl.sexp", manifest),
        (".gitignore", "/target/\n".into()),
        (
            "src/main.lisp",
            format!(
                "(defpackage #:{package} (:use #:cl) (:export #:main))\n(in-package #:{package})\n\n(defun main ()\n  (write-line \"Hello from {name}.\"))\n"
            ),
        ),
    ];
    fs::create_dir(staged.path().join("src"))?;
    for (relative, contents) in files {
        fs::write(staged.path().join(relative), contents)?;
    }
    if path.symlink_metadata().is_ok() {
        return Err(format!("project directory already exists: {}", path.display()).into());
    }
    fs::rename(staged.path(), &path)?;
    println!(
        "Created project {name} at {}",
        zkcl_terminal::display_path(&path)
    );
    Ok(ExitCode::SUCCESS)
}

pub(super) fn project_info(project: &Project, as_json: bool) -> Result<ExitCode> {
    let info = project_metadata(project)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("{} {}", project.manifest.name, project.manifest.version);
        println!("Root: {}", zkcl_terminal::display_path(&project.root));
        println!(
            "Manifest: {}",
            zkcl_terminal::display_path(&project.manifest_path)
        );
        println!(
            "Lockfile: {}",
            zkcl_terminal::display_path(std::path::Path::new(info["lockfile"].as_str().unwrap()))
        );
        if let Some(toolchain) = &project.manifest.toolchain {
            println!("Toolchain: {toolchain}");
        }
        if let Some(package) = &project.manifest.repl_package {
            println!("REPL package: {package:?}");
        }
        for (identity, path) in info["registries"].as_object().expect("registry map") {
            println!("Registry: {identity} = {}", zkcl_terminal::display_path(std::path::Path::new(path.as_str().unwrap())));
        }
        for target in info["targets"].as_array().expect("target array") {
            println!(
                "Target: {} {}",
                target["kind"].as_str().unwrap(),
                target["name"].as_str().unwrap_or("(default)")
            );
            for file in target["files"].as_array().unwrap() {
                println!(
                    "  {}",
                    zkcl_terminal::display_path(std::path::Path::new(file.as_str().unwrap()))
                );
            }
        }
        for dependency in &project.manifest.dependencies {
            println!(
                "Dependency: {} = {} {}",
                dependency.alias,
                dependency.module,
                dependency
                    .requirement
                    .as_ref()
                    .map(ToString::to_string)
                    .or_else(|| dependency.release.clone())
                    .unwrap_or_default()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn project_metadata(project: &Project) -> Result<Value> {
    let manifest = &project.manifest;
    let workspace = project.workspace()?;
    let (lock_root, registry_manifest, registries) = workspace.as_ref().map_or(
        (&project.root, &project.manifest_path, &manifest.registries),
        |workspace| (&workspace.root, &workspace.manifest_path, &workspace.registries),
    );
    let targets: Vec<_> = manifest.targets.iter().map(|target| json!({
        "kind": match target.kind { BuildTargetKind::Library => "library", BuildTargetKind::Binary => "binary", BuildTargetKind::Test => "test" },
        "name": target.name, "files": target.files, "entry": target.entry, "suite": target.suite,
    })).collect();
    let dependencies: Vec<_> = manifest
        .dependencies
        .iter()
        .map(|dependency| {
            let source = match &dependency.source {
                DependencySource::Registry { registry } => {
                    json!({"kind": "registry", "registry": registry})
                }
                DependencySource::Path { path } => json!({"kind": "path", "path": path}),
                DependencySource::Git { url, rev } => {
                    json!({"kind": "git", "url": url, "rev": rev})
                }
            };
            json!({"alias": dependency.alias, "module": dependency.module.as_str(),
                "version": dependency.requirement.as_ref().map(ToString::to_string),
                "release": dependency.release, "source": source, "system": dependency.system,
                "asd": dependency.asd, "features": dependency.features,
                "condition": dependency.condition.as_ref().map(condition_metadata),
            })
        })
        .collect();
    let build = manifest.build.as_ref().map(|build| {
        json!({
            "path": build.path, "outputs": build.outputs, "inputs": build.inputs,
            "env": build.env, "dependencies": build.dependencies,
            "link_libraries": build.link_libraries, "link_search": build.link_search,
        })
    });
    Ok(
        json!({"module": manifest.name.as_str(), "version": manifest.version.to_string(),
            "root": project.root, "manifest": project.manifest_path,
            "lockfile": lock_root.join(zkcl_project::lockfile::LOCKFILE_NAME),
            "toolchain": manifest.toolchain.as_ref().map(ToString::to_string),
            "license": manifest.license, "packages": manifest.packages, "targets": targets,
            "repl_package": manifest.repl_package, "registries": registries,
            "registry_manifest": registry_manifest,
            "files": project.authored_target_files().collect::<Vec<_>>(), "dependencies": dependencies,
            "build": build, "include": manifest.include, "features": manifest.features,
            "exclusive_features": manifest.exclusive_features, "asdf_systems": manifest.systems,
            "asdf_aliases": manifest.asdf_aliases,
        }),
    )
}

fn condition_metadata(condition: &zkcl_project::solver::Condition) -> Value {
    use zkcl_project::solver::Condition;
    match condition {
        Condition::Feature(name) => json!({"feature": name}),
        Condition::Platform(name) => json!({"platform": name}),
        Condition::All(conditions) => {
            json!({"all": conditions.iter().map(condition_metadata).collect::<Vec<_>>()})
        }
        Condition::Any(conditions) => {
            json!({"any": conditions.iter().map(condition_metadata).collect::<Vec<_>>()})
        }
        Condition::Not(condition) => json!({"not": condition_metadata(condition)}),
    }
}

pub(super) fn registry_account_command(
    login: bool,
    request: super::super::args::AccountArgs,
) -> Result<ExitCode> {
    if login {
        publishing::login(&request.registry)?;
    } else {
        publishing::logout(&request.registry)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn owner_control(operation: super::super::args::OwnerOperation) -> Result<Control> {
    use super::super::args::OwnerOperation;
    Ok(match operation {
        OwnerOperation::Claim { namespace } => Control::ClaimNamespace { namespace },
        OwnerOperation::Add { module, subject } => {
            let _: manifest::ModuleName = module.parse()?;
            Control::PackagePublisher {
                module,
                publisher: subject,
                enabled: true,
            }
        }
        OwnerOperation::Remove { module, subject } => {
            let _: manifest::ModuleName = module.parse()?;
            Control::PackagePublisher {
                module,
                publisher: subject,
                enabled: false,
            }
        }
        OwnerOperation::NamespaceAdd { namespace, subject } => Control::NamespacePublisher {
            namespace,
            publisher: subject,
            enabled: true,
        },
        OwnerOperation::NamespaceRemove { namespace, subject } => Control::NamespacePublisher {
            namespace,
            publisher: subject,
            enabled: false,
        },
    })
}

pub(super) fn owner_command(request: super::super::args::OwnerArgs) -> Result<ExitCode> {
    let operation = owner_control(request.operation)?;
    publishing::control(
        request
            .registry
            .as_deref()
            .expect("clap requires registry account"),
        operation,
    )?;
    println!("Updated registry ownership");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_info_reports_generated_build_metadata_without_running_it() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.agents/tmp/zkcl/project-info");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        std::fs::write(
            dir.path().join("zkcl.sexp"),
            r#"(project "test/info" (version "1.2.3")
          (build "never-run.lisp" (output "generated.lisp") (input "data.txt")
            (env "BUILD_MODE") (link-library "widget") (link-search "lib"))
          (library (files "generated.lisp")))"#,
        )
        .unwrap();
        let project = Project::open(dir.path()).unwrap();
        let info = project_metadata(&project).unwrap();
        assert_eq!(info["module"], "test/info");
        assert_eq!(info["build"]["outputs"], json!(["generated.lisp"]));
        assert_eq!(info["build"]["inputs"], json!(["data.txt"]));
        assert_eq!(info["build"]["env"], json!(["BUILD_MODE"]));
        assert_eq!(info["build"]["link_libraries"], json!(["widget"]));
        assert_eq!(info["build"]["link_search"], json!(["lib"]));
        assert_eq!(info["targets"][0]["files"], json!(["generated.lisp"]));
        assert!(!dir.path().join("generated.lisp").exists());
    }

    #[test]
    fn typed_registry_owner_validates_package_identity() {
        use super::super::super::args::OwnerOperation;
        assert!(
            owner_control(OwnerOperation::Add {
                module: "invalid".into(),
                subject: "alice".into()
            })
            .is_err()
        );
        let control = owner_control(OwnerOperation::NamespaceRemove {
            namespace: "test".into(),
            subject: "alice".into(),
        })
        .unwrap();
        assert!(matches!(
            control,
            Control::NamespacePublisher { enabled: false, .. }
        ));
    }
}
