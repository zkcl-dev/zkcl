use std::env;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::process::ExitCode;

use zkcl_project::manifest::{MANIFEST_NAME, Manifest, ModuleName};
use zkcl_project::solver::{Dependency, ModuleId, ReleaseId, SourceId, VersionSpec};
use zkcl_registry::Digest;

use super::registries::configured_identity;
use super::workspace::RegistryDeclarations;
use super::{Project, ProjectArgs, ResolvedProject, execute_resolved_target, select_binary};

#[derive(Debug, thiserror::Error)]
#[error("cannot select a binary from {coordinate}")]
struct TargetError {
    coordinate: String,
    #[source]
    source: Box<dyn Error>,
}

pub(super) fn execute(
    options: ProjectArgs,
    heap: Option<usize>,
) -> Result<ExitCode, Box<dyn Error>> {
    let coordinate = options
        .package
        .as_deref()
        .expect("typed execute request has package");
    let (name, version) = coordinate
        .split_once('@')
        .ok_or("expected a package and exact version in the form namespace/name@version")?;
    let name: ModuleName = name.parse()?;
    // The cache project owns the lockfile and compilation output. The solver
    // starts from the immutable tool release itself.
    let manifest = Manifest {
        name: name.clone(),
        version: version
            .parse()
            .map_err(|error| format!("invalid exact version {version:?}: {error}"))?,
        license: None,
        packages: Vec::new(),
        systems: Default::default(),
        asdf_aliases: Default::default(),
        repl_package: None,
        registries: Default::default(),
        include: Vec::new(),
        toolchain: None,
        build: None,
        targets: Vec::new(),
        dependencies: Vec::new(),
        features: Default::default(),
        exclusive_features: Vec::new(),
    };
    let declarations = RegistryDeclarations::discover(&env::current_dir()?)?;
    let registry = match (options.from.as_deref(), options.registries.as_slice()) {
        (Some(id), _) if id.is_empty() => return Err("--from requires a registry identity".into()),
        (Some(id), _) => id.to_owned(),
        (None, [path]) if !options.offline => configured_identity(path)?,
        (None, []) => {
            let mut identities = declarations.identities();
            match (identities.next(), identities.next()) {
                (Some(identity), None) => identity.to_owned(),
                _ => "zkcl".to_owned(),
            }
        }
        _ => "zkcl".to_owned(),
    };
    let key = Digest::of_bytes(format!("{name}\n{}\n{registry}", manifest.version).as_bytes());
    let root = super::cache::cache_root()?
        .join("tools")
        .join(key.to_string());
    fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;

    let tool = Dependency {
        module: ModuleId {
            name,
            source: SourceId::Registry(registry),
        },
        requirement: VersionSpec::Exact(ReleaseId::Version(manifest.version.clone())),
        required_system: None,
        features: Default::default(),
        condition: None,
    };
    let project = Project {
        manifest_path: root.join(MANIFEST_NAME),
        root,
        manifest,
    };
    let resolved = {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(project.root.join("resolve.lock"))?;
        lock.lock()?;
        let resolved = ResolvedProject::resolve_tool_with_declarations(
            project,
            options.offline,
            &options.registries,
            tool,
            declarations,
        )?
        .with_build_options(super::build_options()?);
        resolved.fetch()?;
        resolved.write_lock()?;
        resolved
    };
    let project = resolved.materialize_tool()?;
    let command = format!("x {coordinate}");
    let target = select_binary(&project, options.bin.as_deref(), &command).map_err(|source| {
        TargetError {
            coordinate: coordinate.to_owned(),
            source,
        }
    })?;
    execute_resolved_target(&resolved, &project, target, "run", options, heap)
}
