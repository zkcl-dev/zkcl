//! Typed declarations for `zkcl.sexp`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use semver::{Version, VersionReq};

use crate::ProjectMetadataError;
use crate::sexp::{self, Value};
use crate::solver::Condition;

pub const MANIFEST_NAME: &str = "zkcl.sexp";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleName(Box<str>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "module name must be namespace/name with lowercase ASCII letters, digits and single hyphens"
)]
pub struct ModuleNameError;

impl ModuleName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModuleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ModuleName {
    type Err = ModuleNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let (namespace, module) = name.split_once('/').ok_or(ModuleNameError)?;
        let valid_part = |part: &str| {
            part.split('-').all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            })
        };
        if !valid_part(namespace) || !valid_part(module) {
            return Err(ModuleNameError);
        }
        Ok(Self(name.into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "Keep the owned Manifest payload in the public API and parse projects without an extra allocation"
)]
pub enum ManifestFile {
    Project(Manifest),
    Workspace(Workspace),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: ModuleName,
    pub version: Version,
    pub license: Option<String>,
    pub packages: Vec<String>,
    pub systems: BTreeMap<String, PathBuf>,
    /// ASDF compatibility systems delegated to this project's native library.
    pub asdf_aliases: BTreeMap<String, String>,
    pub include: Vec<PathBuf>,
    pub toolchain: Option<VersionReq>,
    pub build: Option<BuildScript>,
    pub targets: Vec<BuildTarget>,
    pub dependencies: Vec<Dependency>,
    /// Features this project accepts from its consumers.
    pub features: BTreeSet<String>,
    /// Each group allows at most one enabled feature.
    pub exclusive_features: Vec<BTreeSet<String>>,
}

impl Manifest {
    pub fn is_build_output(&self, path: &Path) -> bool {
        self.build
            .as_ref()
            .is_some_and(|build| build.outputs.contains(path))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildScript {
    pub path: PathBuf,
    /// Paths to files produced in the build output directory.
    pub outputs: BTreeSet<PathBuf>,
    /// None means the task must run on every invocation; an empty declaration is explicit.
    pub inputs: Option<BTreeSet<PathBuf>>,
    pub env: BTreeSet<String>,
    /// Aliases of dependencies loaded only into the isolated build process.
    pub dependencies: BTreeSet<String>,
    pub link_libraries: Vec<String>,
    /// Paths beneath the generated output directory, passed to the native linker.
    pub link_search: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub members: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildTargetKind {
    Library,
    Binary,
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildTarget {
    pub kind: BuildTargetKind,
    pub name: Option<String>,
    pub files: Vec<PathBuf>,
    pub entry: Option<String>,
    pub suite: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub alias: String,
    pub module: ModuleName,
    pub requirement: Option<VersionReq>,
    pub release: Option<String>,
    pub source: DependencySource,
    pub system: Option<String>,
    pub asd: Option<PathBuf>,
    pub features: Vec<String>,
    pub condition: Option<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySource {
    Registry { registry: Option<String> },
    Path { path: PathBuf },
    Git { url: String, rev: String },
}

pub fn parse_manifest(source: &str) -> Result<ManifestFile, ProjectMetadataError> {
    let root = sexp::parse(source)?;
    let values = root.list()?;
    let head = values
        .first()
        .ok_or_else(|| root.error("expected project or workspace declaration"))?;
    match head.symbol()? {
        "project" => parse_project(&root, &values[1..]).map(ManifestFile::Project),
        "workspace" => parse_workspace(&root, &values[1..]).map(ManifestFile::Workspace),
        name => Err(head.error(format!("unknown declaration {name:?}"))),
    }
}

/// Append a registry dependency while preserving all existing source bytes.
///
/// New arguments are validated against the manifest data grammar. Diagnostics
/// for those arguments point to the project declaration in the original text.
pub fn insert_dependency(
    source: &str,
    alias: &str,
    module: &ModuleName,
    requirement: &str,
    registry: Option<&str>,
    system: Option<&str>,
) -> Result<String, ProjectMetadataError> {
    let root = sexp::parse(source)?;
    let values = root.list()?;
    if values.first().map(Value::symbol).transpose()? != Some("project") {
        return Err(root.error("dependencies can only be added to a project declaration"));
    }
    let manifest = parse_project(&root, &values[1..])?;
    if manifest
        .dependencies
        .iter()
        .any(|dependency| dependency.alias == alias)
    {
        return Err(root.error(format!("duplicate dependency {alias:?}")));
    }
    let argument = |text: &str| Value {
        kind: sexp::ValueKind::String(text.into()),
        span: root.span.clone(),
    };
    nonempty_text(&argument(alias))?;
    parse_requirement(&argument(requirement))?;
    if let Some(registry) = registry {
        nonempty_text(&argument(registry))?;
    }
    let system = system
        .map(|name| system_name(&argument(name)))
        .transpose()?;

    let mut declaration = format!(
        "(dependency {} (module {}) (version {})",
        sexp::quote_string(alias),
        sexp::quote_string(module.as_str()),
        sexp::quote_string(requirement),
    );
    if let Some(registry) = registry {
        declaration.push_str(&format!(" (registry {})", sexp::quote_string(registry)));
    }
    if let Some(system) = system {
        declaration.push_str(&format!(" (system {})", sexp::quote_string(&system)));
    }
    declaration.push(')');

    let closing = root.span.end - 1;
    let closing_line = line_start(source, closing);
    let newline = source[..closing].rfind('\n').map_or("\n", |offset| {
        if offset > 0 && source.as_bytes()[offset - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        }
    });
    let root_prefix = &source[line_start(source, root.span.start)..root.span.start];
    let root_indent = if horizontal_whitespace(root_prefix) {
        root_prefix
    } else {
        ""
    };
    let default_indent = format!("{root_indent}  ");
    let indent = values[2..]
        .iter()
        .map(|field| &source[line_start(source, field.span.start)..field.span.start])
        .find(|prefix| horizontal_whitespace(prefix))
        .unwrap_or(&default_indent);

    let (offset, insertion) = if horizontal_whitespace(&source[closing_line..closing]) {
        (closing_line, format!("{indent}{declaration}{newline}"))
    } else {
        (
            closing,
            format!("{newline}{indent}{declaration}{newline}{root_indent}"),
        )
    };
    let mut edited = String::with_capacity(source.len() + insertion.len());
    edited.push_str(&source[..offset]);
    edited.push_str(&insertion);
    edited.push_str(&source[offset..]);
    Ok(edited)
}

fn line_start(source: &str, offset: usize) -> usize {
    source[..offset]
        .rfind('\n')
        .map_or(0, |newline| newline + 1)
}

fn horizontal_whitespace(text: &str) -> bool {
    text.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
}

fn parse_project(root: &Value, values: &[Value]) -> Result<Manifest, ProjectMetadataError> {
    let (name, fields) = values
        .split_first()
        .ok_or_else(|| root.error("project needs a module name"))?;
    let name = module_name(name)?;
    let mut version = None;
    let mut license = None;
    let mut packages = Vec::new();
    let mut systems = BTreeMap::new();
    let mut asdf_aliases = BTreeMap::new();
    let mut include = Vec::new();
    let mut toolchain = None;
    let mut build = None;
    let mut targets = Vec::new();
    let mut dependencies = Vec::new();
    let mut features = BTreeSet::new();
    let mut exclusive_features = Vec::new();
    let mut seen = BTreeSet::new();
    let mut target_names = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    for field in fields {
        let (key, arguments) = field_parts(field)?;
        match key {
            "library" | "binary" | "test" => {
                let target = parse_target(field, key, arguments)?;
                if !target_names.insert((key.to_owned(), target.name.clone())) {
                    return Err(field.error("duplicate target"));
                }
                targets.push(target);
            }
            "dependency" => {
                let dependency = parse_dependency(field, arguments)?;
                if !aliases.insert(dependency.alias.clone()) {
                    return Err(field.error(format!("duplicate dependency {:?}", dependency.alias)));
                }
                dependencies.push(dependency);
            }
            "features" => {
                unique_field(&mut seen, field, key)?;
                features = strings(arguments)?.into_iter().collect();
            }
            "exclusive-features" => {
                let group: BTreeSet<_> = strings(arguments)?.into_iter().collect();
                if group.len() < 2 {
                    return Err(field.error("exclusive-features needs at least two features"));
                }
                if exclusive_features.contains(&group) {
                    return Err(field.error("duplicate exclusive feature group"));
                }
                exclusive_features.push(group);
            }
            "asdf-alias" => {
                let [name, version] = arguments else {
                    return Err(field.error("asdf-alias needs a system name and (version ...)"));
                };
                let name = system_name(name)?;
                let (key, values) = field_parts(version)?;
                if key != "version" {
                    return Err(version.error("asdf-alias needs an explicit version field"));
                }
                let value = one(version, values)?;
                let version = asdf_version(value)?;
                if asdf_aliases.insert(name.clone(), version).is_some() {
                    return Err(field.error(format!("duplicate ASDF alias {name:?}")));
                }
            }
            "asdf-system" => {
                let [name, file] = arguments else {
                    return Err(field.error("asdf-system needs a system name and .asd path"));
                };
                let name = system_name(name)?;
                let file = asd_path(file)?;
                if systems.insert(name.clone(), file).is_some() {
                    return Err(field.error(format!("duplicate ASDF system {name:?}")));
                }
            }
            "build" => {
                unique_field(&mut seen, field, key)?;
                build = Some(parse_build_script(field, arguments)?);
            }
            "version" | "license" | "packages" | "toolchain" | "include" => {
                unique_field(&mut seen, field, key)?;
                match key {
                    "version" => version = Some(parse_version(one(field, arguments)?)?),
                    "license" => license = Some(nonempty_string(one(field, arguments)?)?),
                    "packages" => packages = strings(arguments)?,
                    "include" => include = paths(arguments)?,
                    "toolchain" => toolchain = Some(parse_requirement(one(field, arguments)?)?),
                    _ => unreachable!(),
                }
            }
            _ => return Err(field.error(format!("unknown project field {key:?}"))),
        }
    }
    // Validate after collecting every declaration, so field order is immaterial.
    for field in fields {
        let (key, arguments) = field_parts(field)?;
        if key == "asdf-alias" {
            if !targets
                .iter()
                .any(|target| target.kind == BuildTargetKind::Library)
            {
                return Err(field.error("asdf-alias requires a native library target"));
            }
            let alias = system_name(&arguments[0])?;
            let primary = alias.split('/').next().unwrap();
            if systems
                .keys()
                .any(|name| name.split('/').next() == Some(primary))
            {
                return Err(arguments[0].error(format!(
                    "ASDF primary system {primary:?} cannot mix native aliases with .asd providers"
                )));
            }
        } else if key == "exclusive-features" {
            for value in arguments {
                declared_feature(value, &features)?;
            }
        } else if key == "dependency" {
            for dependency_field in &arguments[1..] {
                let (key, arguments) = field_parts(dependency_field)?;
                if key == "when" {
                    validate_condition(one(dependency_field, arguments)?, &features, false)?;
                }
            }
        }
    }
    if let Some(build) = &build {
        for alias in &build.dependencies {
            if !aliases.contains(alias) {
                return Err(root.error(format!("unknown build dependency {alias:?}")));
            }
        }
    }
    Ok(Manifest {
        name,
        version: version.ok_or_else(|| root.error("project needs a version"))?,
        license,
        packages,
        systems,
        asdf_aliases,
        include,
        toolchain,
        build,
        targets,
        dependencies,
        features,
        exclusive_features,
    })
}

fn parse_build_script(root: &Value, values: &[Value]) -> Result<BuildScript, ProjectMetadataError> {
    let Some((path, fields)) = values.split_first() else {
        return Err(root.error("build needs a project-relative script path"));
    };
    let path = safe_path(path)?;
    let mut outputs = BTreeSet::new();
    let mut inputs = None;
    let mut env = BTreeSet::new();
    let mut dependencies = BTreeSet::new();
    let mut link_libraries = Vec::new();
    let mut link_search = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for field in fields {
        let (key, arguments) = field_parts(field)?;
        unique_field(&mut seen, field, key)?;
        match key {
            "output" => outputs = paths(arguments)?.into_iter().collect(),
            "input" => inputs = Some(paths(arguments)?.into_iter().collect()),
            "env" => {
                for argument in arguments {
                    let name = nonempty_string(argument)?;
                    if name.contains(['=', '\0']) || name.starts_with("ZKCL_") {
                        return Err(argument.error("build environment name must not contain = or NUL or use reserved ZKCL_ prefix"));
                    }
                    if !env.insert(name) {
                        return Err(argument.error("duplicate build environment name"));
                    }
                }
            }
            "dependency" => dependencies = strings(arguments)?.into_iter().collect(),
            "link-library" => {
                link_libraries = strings(arguments)?;
                for (argument, name) in arguments.iter().zip(&link_libraries) {
                    if !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-.+".contains(&byte))
                        || name.starts_with('-')
                    {
                        return Err(argument.error(
                            "link-library needs a library name, without linker flags or paths",
                        ));
                    }
                }
            }
            "link-search" => link_search = paths(arguments)?.into_iter().collect(),
            _ => return Err(field.error(format!("unknown build field {key:?}"))),
        }
    }
    Ok(BuildScript {
        path,
        outputs,
        inputs,
        env,
        dependencies,
        link_libraries,
        link_search,
    })
}

fn parse_workspace(root: &Value, fields: &[Value]) -> Result<Workspace, ProjectMetadataError> {
    let mut members = None;
    for field in fields {
        let (key, arguments) = field_parts(field)?;
        if key != "members" {
            return Err(field.error(format!("unknown workspace field {key:?}")));
        }
        if members.is_some() {
            return Err(field.error("duplicate workspace members field"));
        }
        members = Some(paths(arguments)?);
    }
    let members = members.ok_or_else(|| root.error("workspace needs members"))?;
    if members.is_empty() {
        return Err(root.error("workspace needs at least one member"));
    }
    Ok(Workspace { members })
}

fn parse_target(
    root: &Value,
    kind: &str,
    values: &[Value],
) -> Result<BuildTarget, ProjectMetadataError> {
    let (name, fields) = if values.first().is_some_and(|value| value.string().is_ok()) {
        let name = nonempty_string(&values[0])?;
        if !name.as_bytes()[0].is_ascii_alphanumeric()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        {
            return Err(values[0].error(
                "target name must start with an ASCII letter or digit and contain only letters, digits, _, . and -",
            ));
        }
        (Some(name), &values[1..])
    } else {
        (None, values)
    };
    let kind = match kind {
        "library" => BuildTargetKind::Library,
        "binary" => BuildTargetKind::Binary,
        "test" => BuildTargetKind::Test,
        _ => unreachable!(),
    };
    if kind == BuildTargetKind::Binary && name.is_none() {
        return Err(root.error("binary needs a target name"));
    }
    let mut files = None;
    let mut entry = None;
    let mut suite = None;
    let mut seen = BTreeSet::new();
    for field in fields {
        let (key, arguments) = field_parts(field)?;
        unique_field(&mut seen, field, key)?;
        match key {
            "files" => files = Some(paths(arguments)?),
            "entry" if kind == BuildTargetKind::Binary => {
                entry = Some(nonempty_string(one(field, arguments)?)?)
            }
            "suite" if kind == BuildTargetKind::Test => {
                suite = Some(nonempty_string(one(field, arguments)?)?)
            }
            _ => return Err(field.error(format!("unknown field {key:?} for this target"))),
        }
    }
    let files = files.ok_or_else(|| root.error("target needs files"))?;
    if files.is_empty() {
        return Err(root.error("target needs at least one file"));
    }
    if kind == BuildTargetKind::Binary && entry.is_none() {
        return Err(root.error("binary needs an entry"));
    }
    Ok(BuildTarget {
        kind,
        name,
        files,
        entry,
        suite,
    })
}

fn parse_dependency(root: &Value, values: &[Value]) -> Result<Dependency, ProjectMetadataError> {
    let (alias, fields) = values
        .split_first()
        .ok_or_else(|| root.error("dependency needs an alias"))?;
    let alias = nonempty_string(alias)?;
    let mut module = None;
    let mut requirement = None;
    let mut release = None;
    let mut registry = None;
    let mut path = None;
    let mut git = None;
    let mut rev = None;
    let mut system = None;
    let mut asd = None;
    let mut features = Vec::new();
    let mut condition = None;
    let mut seen = BTreeSet::new();
    for field in fields {
        let (key, arguments) = field_parts(field)?;
        unique_field(&mut seen, field, key)?;
        if key == "features" {
            features = strings(arguments)?;
            continue;
        }
        let value = one(field, arguments)?;
        match key {
            "when" => condition = Some(parse_condition(value)?),
            "module" => module = Some(module_name(value)?),
            "version" => requirement = Some(parse_requirement(value)?),
            "release" => release = Some(nonempty_string(value)?),
            "registry" => registry = Some(nonempty_string(value)?),
            "path" => path = Some(PathBuf::from(relative_path_text(value)?)),
            "git" => git = Some(nonempty_string(value)?),
            "rev" => rev = Some(nonempty_string(value)?),
            "system" => system = Some(system_name(value)?),
            "asd" => asd = Some(asd_path(value)?),
            _ => return Err(field.error(format!("unknown dependency field {key:?}"))),
        }
    }
    if requirement.is_some() && release.is_some() {
        return Err(root.error("dependency cannot specify both version and release"));
    }
    let source_count =
        usize::from(registry.is_some()) + usize::from(path.is_some()) + usize::from(git.is_some());
    if source_count > 1 {
        return Err(root.error("dependency must select a single registry, path or git source"));
    }
    let source = if let Some(path) = path {
        DependencySource::Path { path }
    } else if let Some(url) = git {
        DependencySource::Git {
            url,
            rev: rev
                .take()
                .ok_or_else(|| root.error("git dependency needs a rev"))?,
        }
    } else {
        if requirement.is_none() && release.is_none() {
            return Err(root.error("registry dependency needs a version or release"));
        }
        DependencySource::Registry { registry }
    };
    if rev.is_some() {
        return Err(root.error("rev requires a git source"));
    }
    if asd.is_some() && system.is_none() {
        return Err(root.error("asd requires a system name"));
    }
    Ok(Dependency {
        alias,
        module: module.ok_or_else(|| root.error("dependency needs a module name"))?,
        requirement,
        release,
        source,
        system,
        asd,
        features,
        condition,
    })
}

/// Conditions use their own data grammar; no Lisp reader or evaluator runs here.
fn parse_condition(value: &Value) -> Result<Condition, ProjectMetadataError> {
    let (key, arguments) = field_parts(value)?;
    match key {
        "feature" => Ok(Condition::Feature(nonempty_string(one(value, arguments)?)?)),
        "platform" => Ok(Condition::Platform(nonempty_string(one(
            value, arguments,
        )?)?)),
        "not" => Ok(Condition::Not(Box::new(parse_condition(one(
            value, arguments,
        )?)?))),
        "all" | "any" => {
            if arguments.is_empty() {
                return Err(value.error("all and any need at least one condition"));
            }
            let conditions = arguments
                .iter()
                .map(parse_condition)
                .collect::<Result<_, _>>()?;
            Ok(if key == "all" {
                Condition::All(conditions)
            } else {
                Condition::Any(conditions)
            })
        }
        _ => Err(value.error(format!("unknown condition {key:?}"))),
    }
}

fn declared_feature(
    value: &Value,
    features: &BTreeSet<String>,
) -> Result<(), ProjectMetadataError> {
    let name = nonempty_text(value)?;
    if !features.contains(name) {
        return Err(value.error(format!("undeclared project feature {name:?}")));
    }
    Ok(())
}

fn validate_condition(
    value: &Value,
    features: &BTreeSet<String>,
    negated: bool,
) -> Result<(), ProjectMetadataError> {
    let (key, arguments) = field_parts(value)?;
    match key {
        "feature" => {
            let feature = one(value, arguments)?;
            declared_feature(feature, features)?;
            if negated {
                return Err(feature
                    .error("negative feature conditions are not monotonic under feature union"));
            }
        }
        "not" => validate_condition(one(value, arguments)?, features, !negated)?,
        "all" | "any" => {
            for argument in arguments {
                validate_condition(argument, features, negated)?;
            }
        }
        "platform" => {}
        _ => unreachable!("condition grammar was already checked"),
    }
    Ok(())
}

fn field_parts(field: &Value) -> Result<(&str, &[Value]), ProjectMetadataError> {
    let values = field.list()?;
    let (head, values) = values
        .split_first()
        .ok_or_else(|| field.error("expected a field name"))?;
    Ok((head.symbol()?, values))
}

fn one<'a>(field: &Value, values: &'a [Value]) -> Result<&'a Value, ProjectMetadataError> {
    if let [value] = values {
        Ok(value)
    } else {
        Err(field.error("field needs exactly one value"))
    }
}

fn unique_field(
    seen: &mut BTreeSet<String>,
    field: &Value,
    key: &str,
) -> Result<(), ProjectMetadataError> {
    if !seen.insert(key.into()) {
        return Err(field.error(format!("duplicate field {key:?}")));
    }
    Ok(())
}

fn nonempty_string(value: &Value) -> Result<String, ProjectMetadataError> {
    nonempty_text(value).map(str::to_owned)
}

pub(crate) fn nonempty_text(value: &Value) -> Result<&str, ProjectMetadataError> {
    let text = value.string()?;
    if text.is_empty() || text.contains('\0') {
        return Err(value.error("expected a nonempty string without NUL"));
    }
    Ok(text)
}

fn strings(values: &[Value]) -> Result<Vec<String>, ProjectMetadataError> {
    let mut seen = BTreeSet::new();
    values
        .iter()
        .map(|value| {
            let text = nonempty_string(value)?;
            if !seen.insert(text.clone()) {
                return Err(value.error(format!("duplicate value {text:?}")));
            }
            Ok(text)
        })
        .collect()
}

pub(crate) fn module_name(value: &Value) -> Result<ModuleName, ProjectMetadataError> {
    value
        .string()?
        .parse()
        .map_err(|error: ModuleNameError| value.error(error.to_string()).with_source(error))
}

pub(crate) fn parse_version(value: &Value) -> Result<Version, ProjectMetadataError> {
    Version::parse(value.string()?).map_err(|error| {
        value
            .error(format!("invalid version: {error}"))
            .with_source(error)
    })
}

pub(crate) fn asdf_version(value: &Value) -> Result<String, ProjectMetadataError> {
    let version = parse_version(value)?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(value.error(
            "ASDF alias version must be major.minor.patch without prerelease or build metadata",
        ));
    }
    Ok(version.to_string())
}

fn parse_requirement(value: &Value) -> Result<VersionReq, ProjectMetadataError> {
    VersionReq::parse(value.string()?).map_err(|error| {
        value
            .error(format!("invalid version requirement: {error}"))
            .with_source(error)
    })
}

pub(crate) fn relative_path_text(value: &Value) -> Result<&str, ProjectMetadataError> {
    let text = nonempty_text(value)?;
    if text.contains('\\') || text.contains(':') || Path::new(text).is_absolute() {
        return Err(value.error("expected a relative path using / separators"));
    }
    Ok(text)
}

fn safe_path(value: &Value) -> Result<PathBuf, ProjectMetadataError> {
    let text = relative_path_text(value)?;
    if text
        .split('/')
        .any(|component| matches!(component, "" | "." | ".."))
    {
        return Err(
            value.error("expected a project-relative path without empty, . or .. components")
        );
    }
    Ok(PathBuf::from(text))
}

pub(crate) fn asd_path(value: &Value) -> Result<PathBuf, ProjectMetadataError> {
    let path = safe_path(value)?;
    if path.extension().is_none_or(|extension| extension != "asd") {
        return Err(value.error("ASDF definition path must end in .asd"));
    }
    Ok(path)
}

pub(crate) fn system_name(value: &Value) -> Result<String, ProjectMetadataError> {
    let name = nonempty_text(value)?;
    if !name.split('/').all(|part| {
        !matches!(part, "" | "." | "..")
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+".contains(&byte))
    }) {
        return Err(value.error(
            "ASDF system name must contain ASCII letters, digits, -, _, . or + in nonempty /-separated parts",
        ));
    }
    Ok(name.to_ascii_lowercase())
}

fn paths(values: &[Value]) -> Result<Vec<PathBuf>, ProjectMetadataError> {
    let mut seen = BTreeSet::new();
    values
        .iter()
        .map(|value| {
            let path = safe_path(value)?;
            if !seen.insert(path.clone()) {
                return Err(value.error("duplicate path"));
            }
            Ok(path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn build_tasks_declare_inputs_environment_dependencies_and_linking() {
        let ManifestFile::Project(manifest) = parse_manifest(
            r#"
            (project "acme/app" (version "1.0.0")
              (build "tools/generate.lisp" (input "data/input.txt")
                (env "PATH" "MODE") (dependency "generator")
                (output "generated/value.lisp")
                (link-library "widget") (link-search "native/lib"))
              (dependency "generator" (module "acme/generator") (path "../generator")))
        "#,
        )
        .unwrap() else {
            panic!("expected project")
        };
        let build = manifest.build.unwrap();
        assert_eq!(
            build.inputs.unwrap(),
            BTreeSet::from([PathBuf::from("data/input.txt")])
        );
        assert_eq!(build.env, BTreeSet::from(["MODE".into(), "PATH".into()]));
        assert_eq!(build.dependencies, BTreeSet::from(["generator".into()]));
        assert_eq!(build.link_libraries, ["widget"]);
        assert_eq!(
            build.link_search,
            BTreeSet::from([PathBuf::from("native/lib")])
        );
    }

    #[test]
    fn build_tasks_distinguish_unknown_inputs_from_declared_empty_inputs() {
        for (field, expected) in [("", None), ("(input)", Some(BTreeSet::new()))] {
            let ManifestFile::Project(manifest) = parse_manifest(&format!(
                r#"(project "acme/app" (version "1.0.0") (build "build.lisp" {field}))"#
            ))
            .unwrap() else {
                panic!("expected project")
            };
            assert_eq!(manifest.build.unwrap().inputs, expected);
        }
    }

    #[test]
    fn rejects_invalid_build_task_metadata() {
        for field in [
            r#"(input "../outside")"#,
            r#"(input "a") (input "b")"#,
            r#"(env "A=B")"#,
            r#"(env "ZKCL_OUT_DIR")"#,
            r#"(env "A" "A")"#,
            r#"(dependency "unknown")"#,
            r#"(link-library "-lwidget")"#,
            r#"(link-library "../widget")"#,
            r#"(link-search "../lib")"#,
        ] {
            let source =
                format!(r#"(project "acme/app" (version "1.0.0") (build "build.lisp" {field}))"#);
            assert!(parse_manifest(&source).is_err(), "accepted {field}");
        }
    }

    #[test]
    fn build_scripts_are_explicit_and_use_project_relative_paths() {
        let ManifestFile::Project(implicit) =
            parse_manifest(r#"(project "acme/app" (version "1.0.0"))"#).unwrap()
        else {
            panic!("expected project");
        };
        assert!(implicit.build.is_none());
        let ManifestFile::Project(configured) = parse_manifest(
            r#"(project "acme/app" (version "1.0.0")
                 (build "tools/generate.lisp"
                   (output "generated/config.lisp" "generated/version.lisp")))"#,
        )
        .unwrap() else {
            panic!("expected project");
        };
        assert_eq!(
            configured.build.as_ref().unwrap().path,
            Path::new("tools/generate.lisp")
        );
        assert!(configured.is_build_output(Path::new("generated/config.lisp")));
        assert!(configured.is_build_output(Path::new("generated/version.lisp")));

        for field in [
            "(build)",
            "(build nil)",
            r#"(build "../build.lisp")"#,
            r#"(build "build.lisp" (output "../generated.lisp"))"#,
            r#"(build "build.lisp" (output "a.lisp") (output "b.lisp"))"#,
            r#"(build "build.lisp") (build "other.lisp")"#,
        ] {
            let source = format!(r#"(project "acme/app" (version "1.0.0") {field})"#);
            assert!(parse_manifest(&source).is_err(), "accepted {field}");
        }
    }

    #[test]
    fn parses_declared_features_and_nested_conditions_independent_of_field_order() {
        let source = r#"(project "acme/app" (version "1.0.0")
            (dependency "text" (module "acme/text") (version "^1")
              (features "unicode")
              (when (all (feature "fast")
                         (any (platform "linux") (not (platform "windows")))
                         (not (not (feature "fast"))))))
            (exclusive-features "fast" "small")
            (features "fast" "small"))"#;
        let ManifestFile::Project(manifest) = parse_manifest(source).unwrap() else {
            panic!("expected project")
        };
        assert_eq!(
            manifest.features,
            BTreeSet::from(["fast".into(), "small".into()])
        );
        assert_eq!(manifest.exclusive_features, vec![manifest.features.clone()]);
        assert_eq!(manifest.dependencies[0].features, ["unicode"]);
        assert_eq!(
            manifest.dependencies[0].condition,
            Some(Condition::All(vec![
                Condition::Feature("fast".into()),
                Condition::Any(vec![
                    Condition::Platform("linux".into()),
                    Condition::Not(Box::new(Condition::Platform("windows".into())))
                ]),
                Condition::Not(Box::new(Condition::Not(Box::new(Condition::Feature(
                    "fast".into()
                ))))),
            ]))
        );
    }

    #[test]
    fn rejects_invalid_features_groups_and_condition_grammar() {
        for fields in [
            r#"(features "fast" "fast")"#,
            r#"(features "fast") (features "small")"#,
            r#"(exclusive-features "fast")"#,
            r#"(features "fast") (exclusive-features "fast" "unknown")"#,
            r#"(features "fast" "small") (exclusive-features "fast" "small")
                (exclusive-features "small" "fast")"#,
        ] {
            let source = format!(r#"(project "acme/app" (version "1.0.0") {fields})"#);
            assert!(parse_manifest(&source).is_err(), "accepted {fields}");
        }
        for condition in [
            r#"(feature "unknown")"#,
            r#"(not (feature "fast"))"#,
            r#"(not (any (platform "windows") (feature "fast")))"#,
            r#"(all)"#,
            r#"(any)"#,
            r#"(feature "fast" "small")"#,
            r#"(not (platform "linux") (platform "windows"))"#,
            r#"(platform "")"#,
            r#"(execute "code")"#,
        ] {
            let source = format!(
                r#"(project "acme/app" (version "1.0.0")
              (features "fast")
              (dependency "text" (module "acme/text") (version "^1") (when {condition})))"#
            );
            assert!(parse_manifest(&source).is_err(), "accepted {condition}");
        }
    }

    #[test]
    fn feature_diagnostics_retain_original_utf8_byte_locations() {
        for clause in [
            r#"(when (feature "missing"))"#,
            r#"(when (not (feature "fast")))"#,
        ] {
            let source = format!(
                r#"(project "acme/app" (version "1.0.0") (license "中文")
              (features "fast")
              (dependency "text" (module "acme/text") (version "^1") {clause}))"#
            );
            let error = parse_manifest(&source).unwrap_err();
            let expected = if clause.contains("missing") {
                r#""missing""#
            } else {
                r#""fast""#
            };
            assert_eq!(&source[error.span], expected);
        }
        let source = r#"(project "acme/app" (version "1.0.0")
          (exclusive-features "fast" "missing") (features "fast"))"#;
        let error = parse_manifest(source).unwrap_err();
        assert_eq!(&source[error.span], r#""missing""#);
    }

    #[test]
    fn parses_design_manifest() {
        let ManifestFile::Project(manifest) = parse_manifest(
            r#"
            (project "acme/report"
              (version "1.3.0") (license "MIT")
              (packages "ACME.REPORT" "ACME.REPORT.CLI")
              (toolchain ">=0.1, <1")
              (library (files "src/package.lisp" "src/report.lisp"))
              (binary "report" (files "src/main.lisp") (entry "ACME.REPORT.CLI:MAIN"))
              (dependency "esrap" (module "common-lisp/esrap")
                (release "quicklisp-2026-01-01") (system "esrap"))
              (test (files "tests/report.lisp") (suite "ACME.REPORT::REPORT-TESTS")))
        "#,
        )
        .unwrap() else {
            panic!("expected project")
        };
        assert_eq!(manifest.name.as_str(), "acme/report");
        assert_eq!(manifest.version, Version::new(1, 3, 0));
        assert_eq!(manifest.targets.len(), 3);
        assert_eq!(manifest.dependencies[0].system.as_deref(), Some("esrap"));
    }

    #[test]
    fn supports_all_dependency_sources_and_features() {
        let ManifestFile::Project(manifest) = parse_manifest(
            r#"
            (project "acme/app" (version "1.0.0")
              (dependency "text" (module "acme/text") (version "^2.1")
                (registry "private") (features "unicode"))
              (dependency "peer" (module "acme/peer") (path "../peer")
                (system "peer/core") (asd "lisp/peer.asd"))
              (dependency "git" (module "acme/git") (git "https://example.test/source")
                (rev "0123456789abcdef")))
        "#,
        )
        .unwrap() else {
            panic!("expected project")
        };
        assert_eq!(manifest.dependencies[0].features, ["unicode"]);
        assert!(matches!(
            manifest.dependencies[1].source,
            DependencySource::Path { .. }
        ));
        assert!(matches!(
            manifest.dependencies[2].source,
            DependencySource::Git { .. }
        ));
    }

    #[test]
    fn declares_native_asdf_aliases_with_explicit_numeric_versions() {
        let ManifestFile::Project(manifest) = parse_manifest(
            r#"
            (project "acme/text" (version "2.0.0")
              (asdf-alias "OLD-TEXT" (version "1.2.3"))
              (asdf-alias "old-text/tests" (version "1.2.4"))
              (library (files "src/text.lisp")))
        "#,
        )
        .unwrap() else {
            panic!("expected project")
        };
        assert_eq!(
            manifest.asdf_aliases,
            BTreeMap::from([
                ("old-text".into(), "1.2.3".into()),
                ("old-text/tests".into(), "1.2.4".into()),
            ])
        );
        assert!(manifest.systems.is_empty());
    }

    #[test]
    fn rejects_ambiguous_or_invalid_native_asdf_aliases() {
        for clause in [
            r#"(asdf-alias "old")"#,
            r#"(asdf-alias "old" "1.2.3")"#,
            r#"(asdf-alias "old" (release "1.2.3"))"#,
            r#"(asdf-alias "old" (version "1.2.3-rc.1"))"#,
            r#"(asdf-alias "old" (version "1.2.3+build"))"#,
            r#"(asdf-alias "old" (version "1.2"))"#,
            r#"(asdf-alias "old" (version "1.2.3")) (asdf-alias "OLD" (version "1.2.3"))"#,
            r#"(asdf-alias "old" (version "1.2.3")) (asdf-system "old" "old.asd")"#,
            r#"(asdf-alias "old/native" (version "1.2.3")) (asdf-system "old/external" "old.asd")"#,
            r#"(asdf-system "old/external" "old.asd") (asdf-alias "old" (version "1.2.3"))"#,
        ] {
            let source = format!(
                r#"(project "acme/text" (version "2.0.0")
              (library (files "text.lisp")) {clause})"#
            );
            assert!(parse_manifest(&source).is_err(), "accepted {clause}");
        }
        let source = r#"(project "acme/text" (version "2.0.0")
          (asdf-alias "old" (version "1.2.3")))"#;
        let error = parse_manifest(source).unwrap_err();
        assert_eq!(
            &source[error.span],
            r#"(asdf-alias "old" (version "1.2.3"))"#
        );
    }

    #[test]
    fn declares_official_asdf_provider_and_publish_contents() {
        let ManifestFile::Project(manifest) = parse_manifest(
            r#"
            (project "zk/llvm" (version "1.0.0")
              (packages "ZK-LLVM")
              (asdf-system "ZK-LLVM" "zk-llvm.asd")
              (asdf-system "zk-llvm/tests" "zk-llvm.asd")
              (include "README.md" "src/package.lisp" "src/llvm.lisp")
              (dependency "llvm" (module "zk/llvm-runtime") (version "^1")
                (system "ZK-LLVM-RUNTIME/CORE")))
            "#,
        )
        .unwrap() else {
            panic!("expected project")
        };
        assert_eq!(manifest.name.as_str(), "zk/llvm");
        assert_eq!(manifest.packages, ["ZK-LLVM"]);
        assert_eq!(manifest.systems["zk-llvm"], PathBuf::from("zk-llvm.asd"));
        assert_eq!(
            manifest.systems["zk-llvm/tests"],
            manifest.systems["zk-llvm"]
        );
        assert_eq!(
            manifest.include,
            ["README.md", "src/package.lisp", "src/llvm.lisp"].map(PathBuf::from)
        );
        assert_eq!(
            manifest.dependencies[0].system.as_deref(),
            Some("zk-llvm-runtime/core")
        );
    }

    #[test]
    fn rejects_duplicate_systems_and_unsafe_publish_declarations() {
        for body in [
            "(asdf-system \"ZK-LLVM\" \"zk-llvm.asd\") (asdf-system \"zk-llvm\" \"other.asd\")",
            "(asdf-system \"zk-llvm\")",
            "(asdf-system \"zk-llvm\" \"zk-llvm.lisp\")",
            "(asdf-system \"zk-llvm\" \"../zk-llvm.asd\")",
            "(asdf-system \"zk-llvm//tests\" \"zk-llvm.asd\")",
            "(asdf-system \"zk-llvm/../tests\" \"zk-llvm.asd\")",
            "(asdf-system \"zk llvm\" \"zk-llvm.asd\")",
            "(include \"../README.md\")",
            "(include \"README.md\" \"README.md\")",
            "(include \"README.md\") (include \"src/llvm.lisp\")",
            "(dependency \"llvm\" (module \"zk/llvm\") (version \"^1\") (system \"zk-llvm//core\"))",
        ] {
            let source = format!("(project \"zk/llvm\" (version \"1.0.0\") {body})");
            assert!(parse_manifest(&source).is_err(), "accepted {body}");
        }
    }

    #[test]
    fn rejects_unknown_duplicate_and_ill_typed_fields() {
        for body in [
            "(wat 1)",
            "(version \"2.0.0\")",
            "(license MIT)",
            "(packages \"A\" \"A\")",
            "(library (files))",
            "(binary \"app\" (files \"app.lisp\"))",
            "(binary \"../../app\" (files \"app.lisp\") (entry \"MAIN\"))",
            "(library (files \"x.lisp\") (entry \"MAIN\"))",
            "(dependency \"x\" (module \"acme/x\"))",
            "(dependency \"x\" (module \"acme/x\") (path \"x\") (registry \"r\"))",
            "(dependency \"x\" (module \"acme/x\") (git \"https://example.test\"))",
            "(dependency \"x\" (module \"acme/x\") (version \"^1\") (rev \"abc\"))",
            "(dependency \"x\" (module \"acme/x\") (version \"^1\") (release \"q\"))",
            "(dependency \"x\" (module \"acme/x\") (path \"x\") (asd \"x.asd\"))",
        ] {
            let source = format!("(project \"acme/app\" (version \"1.0.0\") {body})");
            assert!(parse_manifest(&source).is_err(), "accepted {body}");
        }
    }

    #[test]
    fn prevents_file_and_workspace_path_escape() {
        for path in [
            "../x.lisp",
            "/tmp/x.lisp",
            "C:/x.lisp",
            "a\\\\b.lisp",
            "a/../../b.lisp",
            ".",
            "./a",
            "a/./b",
            "a//b",
            "a/",
        ] {
            let source = format!(
                "(project \"acme/app\" (version \"1.0.0\") (library (files {})))",
                sexp::quote_string(path)
            );
            assert!(parse_manifest(&source).is_err(), "accepted {path}");
            let source = format!("(workspace (members {}))", sexp::quote_string(path));
            assert!(parse_manifest(&source).is_err());
        }
        assert!(parse_manifest("(workspace (members \"a\" \"b\"))").is_ok());
        assert!(parse_manifest("(workspace (members \"a\" \"a\"))").is_err());
    }

    #[test]
    fn validates_canonical_module_names() {
        for name in ["a/b", "alice/json2", "acme-org/text-lib"] {
            assert_eq!(name.parse::<ModuleName>().unwrap().as_str(), name);
        }
        for name in [
            "json",
            "a/b/c",
            "/b",
            "a/",
            "Acme/json",
            "a/-b",
            "a/b-",
            "a/b--c",
            "a/b_c",
        ] {
            assert!(name.parse::<ModuleName>().is_err(), "accepted {name}");
        }
    }

    #[test]
    fn locates_field_error_in_original_source() {
        let source = "(project \"a/b\" (version \"1.0.0\") (unknown \"中文\"))";
        let error = parse_manifest(source).unwrap_err();
        assert_eq!(&source[error.span], "(unknown \"中文\")");
    }

    #[test]
    fn preserves_parse_error_sources_and_original_byte_ranges() {
        let source = r#"(project "a/b" (license "中文") (version "1.q.0"))"#;
        let error = parse_manifest(source).unwrap_err();
        assert_eq!(&source[error.span.clone()], r#""1.q.0""#);
        assert!(error.source().unwrap().is::<semver::Error>());

        let source = r#"(project "a/B" (version "1.0.0"))"#;
        let error = parse_manifest(source).unwrap_err();
        assert_eq!(&source[error.span.clone()], r#""a/B""#);
        assert!(error.source().unwrap().is::<ModuleNameError>());
    }

    #[test]
    fn inserts_dependency_into_inline_project_without_rewriting_existing_text() {
        let source = "(project \"acme/app\" (version \"1.0.0\")) ; keep trailing comment\n";
        let edited = insert_dependency(
            source,
            "text",
            &"zk/text".parse().unwrap(),
            "^1.2",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            edited,
            "(project \"acme/app\" (version \"1.0.0\")\n  (dependency \"text\" (module \"zk/text\") (version \"^1.2\"))\n) ; keep trailing comment\n"
        );
        let ManifestFile::Project(manifest) = parse_manifest(&edited).unwrap() else {
            panic!("expected project")
        };
        assert_eq!(manifest.dependencies[0].alias, "text");
    }

    #[test]
    fn dependency_insertion_preserves_crlf_tabs_comments_and_closing_indent() {
        let source = concat!(
            "; 中文 header\r\n",
            "  (project \"acme/app\"\r\n",
            "\t(version \"1.0.0\") ; version comment\r\n",
            "\t#| final local comment |#\r\n",
            "  ) ; trailing comment\r\n",
        );
        let edited = insert_dependency(
            source,
            "llvm",
            &"zk/llvm".parse().unwrap(),
            ">=1, <2",
            Some("private"),
            Some("ZK-LLVM/CORE"),
        )
        .unwrap();
        assert_eq!(
            edited,
            concat!(
                "; 中文 header\r\n",
                "  (project \"acme/app\"\r\n",
                "\t(version \"1.0.0\") ; version comment\r\n",
                "\t#| final local comment |#\r\n",
                "\t(dependency \"llvm\" (module \"zk/llvm\") (version \">=1, <2\") (registry \"private\") (system \"zk-llvm/core\"))\r\n",
                "  ) ; trailing comment\r\n",
            )
        );
        assert!(parse_manifest(&edited).is_ok());
    }

    #[test]
    fn dependency_insertion_keeps_inline_block_comments_and_escapes_arguments() {
        let source = "(project \"acme/app\"\n    (version \"1.0.0\") #| retain this |#)";
        let edited = insert_dependency(
            source,
            "text \"quoted\" \\ alias",
            &"zk/text".parse().unwrap(),
            "^1",
            Some("registry \\ \"name\""),
            None,
        )
        .unwrap();
        assert_eq!(
            edited,
            concat!(
                "(project \"acme/app\"\n",
                "    (version \"1.0.0\") #| retain this |#\n",
                "    (dependency \"text \\\"quoted\\\" \\\\ alias\" (module \"zk/text\") (version \"^1\") (registry \"registry \\\\ \\\"name\\\"\"))\n",
                ")"
            )
        );
        let ManifestFile::Project(manifest) = parse_manifest(&edited).unwrap() else {
            panic!("expected project")
        };
        assert_eq!(manifest.dependencies[0].alias, "text \"quoted\" \\ alias");
    }

    #[test]
    fn dependency_insertion_rejects_existing_alias_non_projects_and_bad_arguments() {
        let module = "zk/text".parse().unwrap();
        let source = concat!(
            "(project \"acme/app\" (version \"1.0.0\") ",
            "(dependency \"text\" (module \"zk/text\") (version \"^1\")))",
        );
        let error = insert_dependency(source, "text", &module, "^2", None, None).unwrap_err();
        assert!(error.message.contains("duplicate dependency"));
        for source in [
            "(workspace (members \"app\"))",
            "()",
            "(project \"acme/app\"",
            "#.(run)",
        ] {
            assert!(insert_dependency(source, "text", &module, "^1", None, None).is_err());
        }
        let source = "(project \"acme/app\" (version \"1.0.0\"))";
        for (alias, requirement, registry, system) in [
            ("", "^1", None, None),
            ("text", "bad version", None, None),
            ("text", "^1", Some(""), None),
            ("text", "^1", None, Some("bad//system")),
        ] {
            let error = insert_dependency(source, alias, &module, requirement, registry, system)
                .unwrap_err();
            assert_eq!(&source[error.span], source);
        }
    }
}
