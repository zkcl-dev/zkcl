//! Exact dependency graphs persisted as non-evaluating `zkcl.lock` data.
//!
//! Path source identities here are relative to the workspace lockfile root. The project
//! loader converts them to canonical paths before using the resolver.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::ProjectMetadataError;
use crate::manifest::{
    ModuleName, asd_path, module_name, nonempty_text, parse_version, relative_path_text,
    system_name,
};
use crate::sexp::{self, Value, quote_string};
use crate::solver::{ModuleId, ReleaseId, SourceId};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum BuildScope {
    #[default]
    Target,
    Build,
}

pub const LOCKFILE_NAME: &str = "zkcl.lock";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedGraph {
    pub project: ModuleName,
    pub scope: BuildScope,
    pub target_triple: String,
    pub features: BTreeSet<String>,
    pub roots: BTreeSet<ModuleId>,
    pub modules: BTreeMap<ModuleId, LockedModule>,
}

/// Identity of a root dependency configuration. Project identity distinguishes
/// workspace members; features are a set and therefore have a canonical order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LockKey {
    pub project: ModuleName,
    pub scope: BuildScope,
    pub target_triple: String,
    pub features: BTreeSet<String>,
}

/// All configurations persisted in one workspace lockfile.
///
/// Graphs remain independent: one configuration may select another release of
/// the same module. Path identities are relative to the shared lockfile directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lockfile {
    configurations: BTreeMap<LockKey, LockedGraph>,
}

impl Lockfile {
    pub fn selected(
        &self,
        project: &ModuleName,
        target: &str,
        features: &BTreeSet<String>,
    ) -> Option<&LockedGraph> {
        self.selected_for_scope(project, target, features, BuildScope::Target)
    }

    pub fn selected_for_scope(
        &self,
        project: &ModuleName,
        target: &str,
        features: &BTreeSet<String>,
        scope: BuildScope,
    ) -> Option<&LockedGraph> {
        self.configurations.get(&LockKey {
            project: project.clone(),
            scope,
            target_triple: target.to_owned(),
            features: features.clone(),
        })
    }

    /// Replace only the matching configuration, preserving all other roots.
    pub fn upsert(&mut self, graph: LockedGraph) -> Option<LockedGraph> {
        self.configurations.insert(graph.key(), graph)
    }

    pub fn remove_project(&mut self, project: &ModuleName) {
        self.configurations.retain(|key, _| &key.project != project);
    }

    pub fn configurations(&self) -> impl Iterator<Item = (&LockKey, &LockedGraph)> {
        self.configurations.iter()
    }

    /// Iterate all graphs, including configurations not selected for this build.
    pub fn graphs(&self) -> impl Iterator<Item = &LockedGraph> {
        self.configurations.values()
    }

    /// Format 2 stores complete, independently validated configuration graphs.
    pub fn render(&self) -> String {
        let mut output = String::from("(lockfile\n  (format 2)\n");
        for graph in self.graphs() {
            output.push_str("  (configuration\n");
            graph.render_fields(&mut output, "    ");
            output.push_str("  )\n");
        }
        output.push_str(")\n");
        output
    }
}

impl From<LockedGraph> for Lockfile {
    fn from(graph: LockedGraph) -> Self {
        let mut lockfile = Self::default();
        lockfile.upsert(graph);
        lockfile
    }
}

/// Read both legacy single-configuration format 1 and workspace format 2.
pub fn parse_lockfile(source: &str) -> Result<Lockfile, ProjectMetadataError> {
    let root = sexp::parse(source)?;
    let (head, fields) = parts(&root)?;
    if head != "lockfile" {
        return Err(root.error("expected a lockfile declaration"));
    }
    let mut format = None;
    for field in fields {
        let (key, values) = parts(field)?;
        if key == "format" {
            if format.is_some() {
                return Err(field.error("duplicate field \"format\""));
            }
            format = Some(one(field, values)?);
        }
    }
    let format = format.ok_or_else(|| root.error("lockfile needs a format version"))?;
    match format.symbol()? {
        "1" => return parse_graph(&root, fields, true).map(Lockfile::from),
        "2" => {}
        _ => return Err(format.error("unsupported lockfile format version")),
    }
    let mut lockfile = Lockfile::default();
    for field in fields {
        let (key, values) = parts(field)?;
        match key {
            "format" => {}
            "configuration" => {
                let graph = parse_graph(field, values, false)?;
                if lockfile.upsert(graph).is_some() {
                    return Err(field.error("duplicate lock configuration"));
                }
            }
            _ => return Err(field.error(format!("unknown lockfile field {key:?}"))),
        }
    }
    Ok(lockfile)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedModule {
    pub release: ReleaseId,
    pub checksum: Option<String>,
    pub features: BTreeSet<String>,
    pub dependencies: BTreeSet<ModuleId>,
    pub systems: BTreeMap<String, PathBuf>,
    pub asdf_aliases: BTreeMap<String, String>,
}

pub fn parse_locked_graph(source: &str) -> Result<LockedGraph, ProjectMetadataError> {
    let root = sexp::parse(source)?;
    let (head, fields) = parts(&root)?;
    if head != "lockfile" {
        return Err(root.error("expected a lockfile declaration"));
    }
    parse_graph(&root, fields, true)
}

fn parse_graph(
    root: &Value,
    fields: &[Value],
    require_format: bool,
) -> Result<LockedGraph, ProjectMetadataError> {
    let mut format = false;
    let mut project = None;
    let mut scope = BuildScope::Target;
    let mut target = None;
    let mut features = BTreeSet::new();
    let mut roots = None;
    let mut modules = BTreeMap::new();
    let mut module_spans = BTreeMap::new();
    let mut references = Vec::new();
    let mut seen = BTreeSet::new();
    for field in fields {
        let (key, values) = parts(field)?;
        if key == "module" {
            let (id, module) = parse_module(field, values, &mut references)?;
            if modules.insert(id.clone(), module).is_some() {
                return Err(field.error(format!("duplicate module {id}")));
            }
            module_spans.insert(id, field);
            continue;
        }
        unique(&mut seen, field, key)?;
        match key {
            "format" if require_format => {
                let version = one(field, values)?;
                if version.symbol()? != "1" {
                    return Err(version.error("unsupported lockfile format version"));
                }
                format = true;
            }
            "scope" => {
                let value = one(field, values)?;
                scope = match value.string()? {
                    "target" => BuildScope::Target,
                    "build" => BuildScope::Build,
                    _ => return Err(value.error("scope must be target or build")),
                };
            }
            "project" => project = Some(module_name(one(field, values)?)?),
            "target" => target = Some(text(one(field, values)?)?),
            "features" => features = feature_names(values)?,
            "roots" => roots = Some(module_references(values, &mut references)?),
            _ => return Err(field.error(format!("unknown lockfile field {key:?}"))),
        }
    }
    if require_format && !format {
        return Err(root.error("lockfile needs a format version"));
    }
    let lockfile = LockedGraph {
        scope,
        project: project.ok_or_else(|| root.error("lockfile needs a project"))?,
        target_triple: target.ok_or_else(|| root.error("lockfile needs a target"))?,
        features,
        roots: roots.ok_or_else(|| root.error("lockfile needs roots"))?,
        modules,
    };
    for (id, value) in references {
        if !lockfile.modules.contains_key(&id) {
            return Err(value.error(format!("module {id} is absent from the locked graph")));
        }
    }
    let mut reachable = BTreeSet::new();
    let mut pending: Vec<_> = lockfile.roots.iter().collect();
    while let Some(id) = pending.pop() {
        if reachable.insert(id) {
            pending.extend(&lockfile.modules[id].dependencies);
        }
    }
    if let Some(id) = lockfile.modules.keys().find(|id| !reachable.contains(id)) {
        return Err(module_spans[id].error(format!(
            "module {id} is outside the root dependency closure"
        )));
    }
    Ok(lockfile)
}

impl LockedGraph {
    pub fn key(&self) -> LockKey {
        LockKey {
            project: self.project.clone(),
            scope: self.scope,
            target_triple: self.target_triple.clone(),
            features: self.features.clone(),
        }
    }

    /// Render a deterministic lockfile from validated, project-relative data.
    pub fn render(&self) -> String {
        let mut output = String::from("(lockfile\n  (format 1)\n");
        self.render_fields(&mut output, "  ");
        output.push_str(")\n");
        output
    }

    fn render_fields(&self, output: &mut String, indentation: &str) {
        let mut fields = format!(
            "  (project {})\n  (target {})",
            quote_string(self.project.as_str()),
            quote_string(&self.target_triple),
        );
        if self.scope == BuildScope::Build {
            fields.push_str("\n  (scope \"build\")");
        }
        let output_fields = output;
        let output = &mut fields;
        render_features(output, "  ", &self.features);
        output.push_str("\n  (roots");
        for id in &self.roots {
            output.push_str("\n    ");
            output.push_str(&render_reference(id));
        }
        output.push_str(")\n");
        for (id, module) in &self.modules {
            output.push_str(&format!(
                "  (module {}\n    {}\n    ({} {})",
                quote_string(id.name.as_str()),
                render_source(&id.source),
                match module.release {
                    ReleaseId::Version(_) => "version",
                    ReleaseId::External(_) => "release",
                },
                quote_string(&module.release.to_string()),
            ));
            if let Some(checksum) = &module.checksum {
                output.push_str(&format!("\n    (checksum {})", quote_string(checksum)));
            }
            render_features(output, "    ", &module.features);
            if !module.dependencies.is_empty() {
                output.push_str("\n    (dependencies");
                for dependency in &module.dependencies {
                    output.push_str("\n      ");
                    output.push_str(&render_reference(dependency));
                }
                output.push(')');
            }
            for (system, path) in &module.systems {
                output.push_str(&format!(
                    "\n    (system {} {})",
                    quote_string(system),
                    quote_path(path),
                ));
            }
            for (name, version) in &module.asdf_aliases {
                output.push_str(&format!(
                    "\n    (asdf-alias {} {})",
                    quote_string(name),
                    quote_string(version),
                ));
            }
            output.push_str(")\n");
        }
        for line in fields.lines() {
            output_fields.push_str(indentation);
            output_fields.push_str(&line[2..]);
            output_fields.push('\n');
        }
    }
}

fn render_features(output: &mut String, indentation: &str, features: &BTreeSet<String>) {
    if features.is_empty() {
        return;
    }
    output.push('\n');
    output.push_str(indentation);
    output.push_str("(features");
    for feature in features {
        output.push(' ');
        output.push_str(&quote_string(feature));
    }
    output.push(')');
}

fn feature_names(values: &[Value]) -> Result<BTreeSet<String>, ProjectMetadataError> {
    let mut features = BTreeSet::new();
    for value in values {
        let feature = text(value)?;
        if !features.insert(feature.clone()) {
            return Err(value.error(format!("duplicate feature {feature:?}")));
        }
    }
    Ok(features)
}

fn render_reference(id: &ModuleId) -> String {
    format!(
        "(module {} {})",
        quote_string(id.name.as_str()),
        render_source(&id.source)
    )
}

fn render_source(source: &SourceId) -> String {
    let source = match source {
        SourceId::Path(path) => format!("path {}", quote_path(path)),
        SourceId::PathRequest { .. } => {
            unreachable!("unactivated path request cannot appear in a lockfile")
        }
        SourceId::Registry(identity) => format!("registry {}", quote_string(identity)),
        SourceId::Git { url, rev } => format!("git {} {}", quote_string(url), quote_string(rev)),
    };
    format!("(source ({source}))")
}

fn quote_path(path: &std::path::Path) -> String {
    let on_disk = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    quote_string(&on_disk)
}

fn parse_module<'a>(
    root: &'a Value,
    values: &'a [Value],
    references: &mut Vec<(ModuleId, &'a Value)>,
) -> Result<(ModuleId, LockedModule), ProjectMetadataError> {
    let (name, fields) = values
        .split_first()
        .ok_or_else(|| root.error("module needs a name"))?;
    let name = module_name(name)?;
    let mut source = None;
    let mut release = None;
    let mut checksum = None;
    let mut features = BTreeSet::new();
    let mut dependencies = BTreeSet::new();
    let mut systems = BTreeMap::new();
    let mut asdf_aliases = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for field in fields {
        let (key, values) = parts(field)?;
        if matches!(key, "system" | "asdf-alias") {
            let [name, binding] = values else {
                return Err(
                    field.error("expected an ASDF system name and its path or alias version")
                );
            };
            let name = system_name(name)?;
            if systems.contains_key(&name) || asdf_aliases.contains_key(&name) {
                return Err(field.error(format!("duplicate ASDF system {name:?}")));
            }
            if key == "system" {
                systems.insert(name, asd_path(binding)?);
            } else {
                asdf_aliases.insert(name, crate::manifest::asdf_version(binding)?);
            }
            continue;
        }
        unique(&mut seen, field, key)?;
        match key {
            "source" => source = Some(parse_source(one(field, values)?)?),
            "version" | "release" => {
                if release.is_some() {
                    return Err(field.error("module must select one version or release"));
                }
                let value = one(field, values)?;
                release = Some(if key == "version" {
                    ReleaseId::Version(parse_version(value)?)
                } else {
                    ReleaseId::External(text(value)?)
                });
            }
            "checksum" => {
                let value = one(field, values)?;
                let digest = value.string()?;
                if digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(
                        value.error("checksum must be a lowercase SHA-256 hexadecimal digest")
                    );
                }
                checksum = Some(digest.to_owned());
            }
            "features" => features = feature_names(values)?,
            "dependencies" => dependencies = module_references(values, references)?,
            _ => return Err(field.error(format!("unknown locked module field {key:?}"))),
        }
    }
    let source = source.ok_or_else(|| root.error("module needs a source"))?;
    if !matches!(source, SourceId::Path(_)) && checksum.is_none() {
        return Err(root.error("registry and git modules need a checksum"));
    }
    Ok((
        ModuleId { source, name },
        LockedModule {
            release: release.ok_or_else(|| root.error("module needs a version or release"))?,
            checksum,
            features,
            dependencies,
            systems,
            asdf_aliases,
        },
    ))
}

fn module_references<'a>(
    values: &'a [Value],
    references: &mut Vec<(ModuleId, &'a Value)>,
) -> Result<BTreeSet<ModuleId>, ProjectMetadataError> {
    let mut ids = BTreeSet::new();
    for value in values {
        let (key, values) = parts(value)?;
        let [name, source] = values else {
            return Err(value.error("module reference needs a name and source"));
        };
        if key != "module" {
            return Err(value.error("expected a module reference"));
        }
        let (key, sources) = parts(source)?;
        if key != "source" {
            return Err(source.error("expected a source field"));
        }
        let id = ModuleId {
            name: module_name(name)?,
            source: parse_source(one(source, sources)?)?,
        };
        if !ids.insert(id.clone()) {
            return Err(value.error(format!("duplicate module reference {id}")));
        }
        references.push((id, value));
    }
    Ok(ids)
}

fn parse_source(value: &Value) -> Result<SourceId, ProjectMetadataError> {
    let (kind, values) = parts(value)?;
    match kind {
        "path" => Ok(SourceId::Path(PathBuf::from(relative_path_text(one(
            value, values,
        )?)?))),
        "registry" => Ok(SourceId::Registry(text(one(value, values)?)?)),
        "git" => {
            let [url, rev] = values else {
                return Err(value.error("git source needs a URL and exact revision"));
            };
            Ok(SourceId::Git {
                url: text(url)?,
                rev: text(rev)?,
            })
        }
        _ => Err(value.error(format!("unknown source kind {kind:?}"))),
    }
}

fn parts(value: &Value) -> Result<(&str, &[Value]), ProjectMetadataError> {
    let (key, values) = value
        .list()?
        .split_first()
        .ok_or_else(|| value.error("expected a field name"))?;
    Ok((key.symbol()?, values))
}

fn one<'a>(field: &Value, values: &'a [Value]) -> Result<&'a Value, ProjectMetadataError> {
    match values {
        [value] => Ok(value),
        _ => Err(field.error("field needs exactly one value")),
    }
}

fn unique(
    seen: &mut BTreeSet<String>,
    field: &Value,
    key: &str,
) -> Result<(), ProjectMetadataError> {
    if seen.insert(key.into()) {
        Ok(())
    } else {
        Err(field.error(format!("duplicate field {key:?}")))
    }
}

fn text(value: &Value) -> Result<String, ProjectMetadataError> {
    nonempty_text(value).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH_MODULE: &str =
        r#"(module "acme/local" (source (path "../local")) (version "1.0.0"))"#;
    const PATH_REFERENCE: &str = r#"(module "acme/local" (source (path "../local")))"#;

    fn lock(roots: &str, modules: &str) -> String {
        format!(
            r#"(lockfile (format 1) (project "acme/app") (target "app") (roots {roots}) {modules})"#
        )
    }

    #[test]
    fn build_and_target_configurations_have_independent_keys() {
        let target = parse_locked_graph(&lock(PATH_REFERENCE, PATH_MODULE)).unwrap();
        let mut build = target.clone();
        build.scope = BuildScope::Build;
        let mut lockfile = Lockfile::from(target.clone());
        lockfile.upsert(build.clone());
        assert_eq!(lockfile.graphs().count(), 2);
        assert_eq!(
            lockfile.selected(&target.project, &target.target_triple, &target.features),
            Some(&target)
        );
        assert_eq!(
            lockfile.selected_for_scope(
                &build.project,
                &build.target_triple,
                &build.features,
                BuildScope::Build
            ),
            Some(&build)
        );
        assert_eq!(parse_lockfile(&lockfile.render()).unwrap(), lockfile);
        assert_eq!(parse_locked_graph(&build.render()).unwrap(), build);
        assert!(
            parse_locked_graph(
                &target
                    .render()
                    .replace("(target", "(scope \"invalid\") (target")
            )
            .is_err()
        );
    }

    #[test]
    fn set_reads_legacy_and_preserves_other_configuration_updates() {
        let graph = parse_locked_graph(&lock(PATH_REFERENCE, PATH_MODULE)).unwrap();
        let mut set = parse_lockfile(&graph.render()).unwrap();
        assert_eq!(set.graphs().collect::<Vec<_>>(), vec![&graph]);
        let mut feature_graph = graph.clone();
        feature_graph.features.insert("unicode".into());
        set.upsert(feature_graph.clone());
        let mut target_graph = graph.clone();
        target_graph.target_triple = "tests".into();
        set.upsert(target_graph.clone());
        let mut member_graph = graph.clone();
        member_graph.project = "acme/member".parse().unwrap();
        set.upsert(member_graph.clone());
        let mut replacement = feature_graph.clone();
        replacement.modules.values_mut().next().unwrap().release =
            ReleaseId::External("new-release".into());
        assert_eq!(set.upsert(replacement.clone()), Some(feature_graph));
        assert_eq!(set.graphs().count(), 4);
        assert_eq!(
            set.selected(&graph.project, &graph.target_triple, &graph.features),
            Some(&graph)
        );
        assert_eq!(
            set.selected(
                &replacement.project,
                &replacement.target_triple,
                &replacement.features
            ),
            Some(&replacement)
        );
        let rendered = set.render();
        assert!(rendered.contains("(format 2)"));
        assert_eq!(parse_lockfile(&rendered).unwrap(), set);
        assert_eq!(parse_lockfile(&rendered).unwrap().render(), rendered);
        let mut reversed = Lockfile::default();
        for graph in set.graphs().collect::<Vec<_>>().into_iter().rev() {
            reversed.upsert(graph.clone());
        }
        assert_eq!(reversed.render(), rendered);
    }

    #[test]
    fn set_rejects_duplicate_keys_and_validates_every_graph() {
        let graph = parse_locked_graph(&lock(PATH_REFERENCE, PATH_MODULE)).unwrap();
        let rendered = Lockfile::from(graph).render();
        let root = sexp::parse(&rendered).unwrap();
        let (_, fields) = parts(&root).unwrap();
        let configuration = fields
            .iter()
            .find(|field| parts(field).unwrap().0 == "configuration")
            .unwrap();
        let duplicate = format!(
            "(lockfile (format 2) {0} {0})",
            &rendered[configuration.span.clone()]
        );
        assert!(
            parse_lockfile(&duplicate)
                .unwrap_err()
                .message
                .contains("duplicate")
        );
        let canonical_duplicate = duplicate.replace(
            "(target \"app\")",
            "(features \"a\" \"b\") (target \"app\")",
        );
        assert!(
            parse_lockfile(&canonical_duplicate)
                .unwrap_err()
                .message
                .contains("duplicate")
        );
        let mut reordered = canonical_duplicate.clone();
        let position = reordered.rfind("(features \"a\" \"b\")").unwrap();
        reordered.replace_range(
            position..position + "(features \"a\" \"b\")".len(),
            "(features \"b\" \"a\")",
        );
        assert!(
            parse_lockfile(&reordered)
                .unwrap_err()
                .message
                .contains("duplicate")
        );
        assert!(
            parse_lockfile(&rendered.replace("(version \"1.0.0\")", "(version \"^1\")")).is_err()
        );
        assert!(parse_lockfile(&rendered.replace("(format 2)", "(format 2) (format 2)")).is_err());
        assert!(parse_lockfile(&rendered.replace("(format 2)", "(format 3)")).is_err());
        assert!(parse_lockfile(&rendered.replace("(configuration", "(unknown")).is_err());
        assert!(
            parse_lockfile(&rendered.replace("(target \"app\")", "(format 1) (target \"app\")"))
                .is_err()
        );
    }

    #[test]
    fn empty_lockfile_set_roundtrips() {
        let set = Lockfile::default();
        assert_eq!(parse_lockfile(&set.render()).unwrap(), set);
    }

    #[test]
    fn roundtrips_complete_graph_with_all_source_and_release_kinds() {
        let digest = "a1".repeat(32);
        let source = lock(
            PATH_REFERENCE,
            &format!(
                r#"
            (module "acme/local" (source (path "../local")) (version "1.2.3+build")
              (features "unicode" "fast")
              (dependencies
                (module "acme/remote" (source (registry "https://registry.example/中文")))
                (module "acme/git" (source (git "https://example/a\"b" "abc123"))))
              (system "Local/CORE" "lisp/local.asd"))
            (module "acme/remote" (source (registry "https://registry.example/中文"))
              (release "quicklisp-2026-01-01") (checksum "{digest}"))
            (module "acme/git" (source (git "https://example/a\"b" "abc123"))
              (version "2.0.0-rc.1") (checksum "{digest}"))
        "#
            ),
        );
        let parsed = parse_locked_graph(&source).unwrap();
        let rendered = parsed.render();
        assert_eq!(parse_locked_graph(&rendered).unwrap(), parsed);
        assert_eq!(parse_locked_graph(&rendered).unwrap().render(), rendered);
        assert_eq!(parsed.modules.len(), 3);
        assert!(rendered.contains(r#"(system "local/core" "lisp/local.asd")"#));
        assert!(
            parsed
                .modules
                .values()
                .any(|module| module.checksum.is_none())
        );
    }

    #[test]
    fn supports_empty_dependency_graph() {
        let parsed = parse_locked_graph(&lock("", "")).unwrap();
        assert!(parsed.modules.is_empty());
        assert_eq!(parse_locked_graph(&parsed.render()).unwrap(), parsed);
    }

    #[test]
    fn rejects_missing_references_and_unreachable_records_at_their_spans() {
        let source = lock(PATH_REFERENCE, "");
        let error = parse_locked_graph(&source).unwrap_err();
        assert_eq!(&source[error.span], PATH_REFERENCE);
        let source = lock("", PATH_MODULE);
        let error = parse_locked_graph(&source).unwrap_err();
        assert_eq!(&source[error.span], PATH_MODULE);
        let source = lock(PATH_REFERENCE, &PATH_MODULE.replace(
            "(version \"1.0.0\")",
            r#"(version "1.0.0") (dependencies (module "acme/missing" (source (path "../missing"))))"#,
        ));
        let error = parse_locked_graph(&source).unwrap_err();
        assert_eq!(
            &source[error.span],
            r#"(module "acme/missing" (source (path "../missing")))"#
        );
    }

    #[test]
    fn rejects_duplicate_records_roots_edges_features_and_systems() {
        for source in [
            lock(PATH_REFERENCE, &format!("{PATH_MODULE} {PATH_MODULE}")),
            lock(&format!("{PATH_REFERENCE} {PATH_REFERENCE}"), PATH_MODULE),
            lock(
                PATH_REFERENCE,
                &PATH_MODULE.replace(
                    "(version",
                    &format!("(dependencies {PATH_REFERENCE} {PATH_REFERENCE}) (version"),
                ),
            ),
            lock(
                PATH_REFERENCE,
                &PATH_MODULE.replace("(version", "(features \"a\" \"a\") (version"),
            ),
            lock(
                PATH_REFERENCE,
                &PATH_MODULE.replace(
                    "(version",
                    "(system \"a\" \"a.asd\") (system \"A\" \"b.asd\") (version",
                ),
            ),
        ] {
            assert!(
                parse_locked_graph(&source)
                    .unwrap_err()
                    .message
                    .contains("duplicate"),
                "{source}"
            );
        }
    }

    #[test]
    fn validates_source_and_system_paths_without_accessing_the_filesystem() {
        for bad in ["/absolute", "C:/absolute", "..\\local", ""] {
            let literal = quote_string(bad);
            let source = lock(
                &PATH_REFERENCE.replace("\"../local\"", &literal),
                &PATH_MODULE.replace("\"../local\"", &literal),
            );
            let error = parse_locked_graph(&source).unwrap_err();
            assert_eq!(&source[error.span], literal);
        }
        for bad in [
            "../outside.asd",
            "/absolute.asd",
            "C:/a.asd",
            "nested/../../a.asd",
            "a\\b.asd",
            "a.lisp",
            "",
        ] {
            let source = lock(
                PATH_REFERENCE,
                &PATH_MODULE.replace(
                    "(version",
                    &format!("(system \"local/core\" {}) (version", quote_string(bad)),
                ),
            );
            let error = parse_locked_graph(&source).unwrap_err();
            assert_eq!(&source[error.span], quote_string(bad));
        }
        let source = lock(
            PATH_REFERENCE,
            &PATH_MODULE.replace("(version", "(system \"local/..\" \"local.asd\") (version"),
        );
        let error = parse_locked_graph(&source).unwrap_err();
        assert_eq!(&source[error.span], r#""local/..""#);
    }

    #[test]
    fn requires_and_validates_immutable_content_checksums() {
        for source in [r#"(registry "main")"#, r#"(git "url" "revision")"#] {
            let reference = PATH_REFERENCE.replace(r#"(path "../local")"#, source);
            let module = PATH_MODULE.replace(r#"(path "../local")"#, source);
            assert!(
                parse_locked_graph(&lock(&reference, &module))
                    .unwrap_err()
                    .message
                    .contains("checksum")
            );
            for digest in ["a".repeat(63), "A".repeat(64), "g".repeat(64)] {
                let module =
                    module.replace("(version", &format!("(checksum \"{digest}\") (version"));
                let error = parse_locked_graph(&lock(&reference, &module)).unwrap_err();
                assert!(error.message.contains("SHA-256"));
            }
        }
    }

    #[test]
    fn rejects_unknown_formats_reader_effects_and_invalid_schema() {
        let good = lock(PATH_REFERENCE, PATH_MODULE);
        for source in [
            good.replace("(format 1)", "(format 2)"),
            good.replace("(format 1)", ""),
            good.replace("(target \"app\")", "(target \"app\") (target \"other\")"),
            good.replace(
                "(version \"1.0.0\")",
                "(version \"1.0.0\") (release \"external\")",
            ),
            good.replace("(version \"1.0.0\")", "(version \"^1\")"),
            good.replace("(version \"1.0.0\")", "(unknown \"x\")"),
            good.replace("(format 1)", "(format #.(effects))"),
            good.replace("(format 1)", "#+enabled (format 1)"),
            good.replace("(path \"../local\")", "(git \"url\")"),
        ] {
            assert!(parse_locked_graph(&source).is_err(), "accepted {source}");
        }
    }
}
