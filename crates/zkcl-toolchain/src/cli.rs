//! The stable launcher owns only this prefix and the management branch.
//! Arguments belonging to the selected zkcl release remain operating-system strings.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use crate::selector::{Channel, ReleaseId, Selector};
pub(crate) use zkcl_terminal::ColorMode as Color;

#[derive(Debug)]
pub(crate) struct Invocation {
    pub(crate) directories: Vec<PathBuf>,
    pub(crate) toolchain: Option<Selector>,
    pub(crate) as_package_runner: bool,
    pub(crate) color: Color,
    pub(crate) operation: Operation,
}

#[derive(Debug)]
pub(crate) enum Operation {
    Launch(Vec<OsString>),
    Manage(Management),
    Bootstrap(Bootstrap),
}

#[derive(Debug)]
pub(crate) struct ParseError {
    pub(crate) error: clap::Error,
    pub(crate) color: Color,
}

impl ParseError {
    fn new<T: CommandFactory>(error: clap::Error, color: Color) -> Self {
        let stream = if error.use_stderr() {
            zkcl_terminal::Stream::Stderr
        } else {
            zkcl_terminal::Stream::Stdout
        };
        Self {
            error: error.with_cmd(&T::command().color(zkcl_terminal::clap_color(color, stream))),
            color,
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::ops::Deref for ParseError {
    type Target = clap::Error;

    fn deref(&self) -> &Self::Target {
        &self.error
    }
}

fn parse_owned<T: Parser>(args: Vec<OsString>, fallback: Color) -> Result<T, ParseError> {
    let color = zkcl_terminal::command_color(T::command(), &args, fallback);
    T::try_parse_from(args).map_err(|error| ParseError::new::<T>(error, color))
}

#[derive(Debug, Parser)]
#[command(
    name = "zkcl",
    disable_help_flag = true,
    disable_version_flag = true,
    args_override_self = true
)]
struct Prefix {
    /// Change directory before selecting a toolchain.
    #[arg(short = 'C', long = "directory", value_name = "DIRECTORY")]
    directories: Vec<PathBuf>,
    #[arg(long, value_enum)]
    color: Option<Color>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    arguments: Vec<OsString>,
}

#[derive(Parser)]
#[command(name = "zkcl", disable_help_flag = true)]
struct Override {
    #[arg(value_name = "+TOOLCHAIN")]
    toolchain: Selector,
}

#[derive(Debug, Parser)]
#[command(
    name = "zkcl toolchain",
    about = "Install and select zkcl toolchains",
    long_about = "Install and select zkcl toolchains. A toolchain is stable, beta, nightly, an exact version such as 0.1.0, or a dated nightly such as nightly-2026-11-23.",
    after_long_help = "Selection: +TOOLCHAIN overrides ZKCL_TOOLCHAIN, the nearest project pin, and the user default.\nExamples:\n  zkcl toolchain install stable\n  zkcl toolchain pin nightly\n  zkcl +beta run example.lisp",
    arg_required_else_help = true,
    args_override_self = true
)]
pub(crate) struct Management {
    /// Control colored terminal output.
    #[arg(long, value_enum, default_value = "auto", global = true)]
    pub(crate) color: Color,
    #[command(subcommand)]
    pub(crate) command: ManagementCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ManagementCommand {
    /// Install a toolchain, or the nearest project's pinned release.
    Install { toolchain: Option<Selector> },
    /// Show or set the user default, installing it if necessary.
    Default { toolchain: Option<Selector> },
    /// Install and pin an exact release for this project.
    Pin { toolchain: Selector },
    /// Remove this project's version pin.
    Unpin,
    /// Install the latest release and make its channel the default.
    Upgrade {
        #[arg(value_enum)]
        channel: Option<Channel>,
    },
    /// List installed releases and the default and active selection.
    List,
    /// Print the path to an installed zkcl executable.
    Which { toolchain: Option<Selector> },
    /// Remove an installed exact release.
    Uninstall { version: ReleaseId },
    /// Manage SDK targets for the active toolchain.
    Target {
        #[command(subcommand)]
        command: TargetCommand,
    },
    /// Manage optional components for the active toolchain.
    Component {
        #[command(subcommand)]
        command: ComponentCommand,
    },
    /// Update the launcher from a release channel.
    SelfUpdate {
        #[arg(default_value = "stable", value_enum)]
        channel: Channel,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum TargetCommand {
    /// List SDK targets available for the active release.
    List,
    /// Install an SDK target.
    Add { target: String },
    /// Remove an installed SDK target.
    Remove { target: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ComponentCommand {
    /// List optional components available for the active release.
    List,
    /// Install an optional component.
    Add { component: Component },
    /// Remove an installed optional component.
    Remove { component: Component },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum Component {
    Sdk,
}

impl Component {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Sdk => "sdk",
        }
    }
}

pub(crate) enum ComponentRequest {
    ListTargets,
    ListComponents,
    ModifyTarget { target: String, remove: bool },
    ModifyComponent { component: Component, remove: bool },
}

impl From<TargetCommand> for ComponentRequest {
    fn from(command: TargetCommand) -> Self {
        match command {
            TargetCommand::List => Self::ListTargets,
            TargetCommand::Add { target } => Self::ModifyTarget {
                target,
                remove: false,
            },
            TargetCommand::Remove { target } => Self::ModifyTarget {
                target,
                remove: true,
            },
        }
    }
}

impl From<ComponentCommand> for ComponentRequest {
    fn from(command: ComponentCommand) -> Self {
        match command {
            ComponentCommand::List => Self::ListComponents,
            ComponentCommand::Add { component } => Self::ModifyComponent {
                component,
                remove: false,
            },
            ComponentCommand::Remove { component } => Self::ModifyComponent {
                component,
                remove: true,
            },
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "zkcl __bootstrap",
    about = "Install a verified launcher release"
)]
pub(crate) struct Bootstrap {
    #[arg(long, value_name = "DIRECTORY")]
    pub(crate) prefix: PathBuf,
    #[arg(long = "from", value_name = "SOURCE")]
    pub(crate) source: String,
    #[arg(long = "toolchain", value_name = "TOOLCHAIN")]
    pub(crate) selector: Selector,
    #[arg(long = "resolved", value_name = "VERSION")]
    pub(crate) id: ReleaseId,
}

impl Bootstrap {
    fn validate(&self) -> Result<(), clap::Error> {
        let valid = match &self.selector {
            Selector::Exact(requested) => requested == &self.id,
            Selector::Channel(channel) => channel.accepts(&self.id),
        };
        if valid {
            Ok(())
        } else {
            Err(Self::command().error(
                clap::error::ErrorKind::ValueValidation,
                format!(
                    "resolved version {} does not match toolchain {}",
                    self.id, self.selector
                ),
            ))
        }
    }
}

/// Parse stable prefixes without applying them. Help and usage errors must not
/// change directories or inspect an installation.
pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, ParseError> {
    let mut args = args.into_iter();
    let executable = args.next().unwrap_or_else(|| OsString::from("zkcl"));
    let as_package_runner = Path::new(&executable)
        .file_name()
        .is_some_and(|name| name == "zkclx");
    let mut remaining: Vec<_> = args.collect();
    let mut directories = Vec::new();
    let mut toolchain = None;
    let mut color = Color::Auto;
    let mut forwarded_prefix = Vec::new();
    let operation = loop {
        if remaining.first().is_some_and(|arg| arg == "--") {
            forwarded_prefix.extend(remaining);
            break Operation::Launch(forwarded_prefix);
        }
        let prefix: Prefix = parse_owned(
            std::iter::once(OsString::from("zkcl"))
                .chain(remaining.iter().cloned())
                .collect(),
            color,
        )?;
        let consumed = remaining.len() - prefix.arguments.len();
        // Keep the spelling of shared color flags when handing control to an
        // evolving release. Clap has already validated this owned prefix.
        let mut owned = remaining[..consumed].iter();
        while let Some(argument) = owned.next() {
            if argument == "--color" {
                forwarded_prefix.push(argument.clone());
                forwarded_prefix.push(owned.next().expect("validated color value").clone());
            } else if argument
                .to_str()
                .is_some_and(|arg| arg.starts_with("--color="))
            {
                forwarded_prefix.push(argument.clone());
            }
        }
        directories.extend(prefix.directories);
        if let Some(choice) = prefix.color {
            color = choice;
        }
        // A delimiter belongs to the selected program, including when a stable
        // prefix was consumed before it. Never reinterpret what follows it.
        if consumed > 0 && remaining[consumed - 1] == "--" {
            forwarded_prefix.extend(remaining[consumed - 1..].iter().cloned());
            break Operation::Launch(forwarded_prefix);
        }
        remaining = prefix.arguments;
        // With an opaque hyphen-accepting positional, clap leaves an attached
        // short value in that positional. Normalize only its leading spelling
        // and parse this owned prefix again; downstream arguments stay opaque.
        if let Some(argument) = remaining.first()
            && argument.as_encoded_bytes().starts_with(b"-C")
            && argument.as_encoded_bytes().len() > 2
        {
            #[cfg(unix)]
            let directory = {
                use std::os::unix::ffi::OsStringExt;
                OsString::from_vec(argument.as_encoded_bytes()[2..].to_vec())
            };
            #[cfg(not(unix))]
            let directory = OsString::from(
                argument
                    .to_str()
                    .ok_or_else(|| {
                        ParseError::new::<Prefix>(
                            Prefix::command().error(
                                clap::error::ErrorKind::InvalidUtf8,
                                "attached directory must be UTF-8 on this platform",
                            ),
                            color,
                        )
                    })?
                    .strip_prefix("-C")
                    .expect("directory prefix"),
            );
            remaining[0] = OsString::from("-C");
            remaining.insert(1, directory);
            continue;
        }

        if let Some(argument) = remaining.first()
            && argument.as_encoded_bytes().starts_with(b"+")
        {
            if toolchain.is_some() {
                return Err(ParseError::new::<Prefix>(
                    Prefix::command().error(
                        clap::error::ErrorKind::ArgumentConflict,
                        "toolchain override specified more than once",
                    ),
                    color,
                ));
            }
            let value = argument
                .to_str()
                .and_then(|value| value.strip_prefix('+'))
                .ok_or_else(|| {
                    ParseError::new::<Prefix>(
                        Prefix::command().error(
                            clap::error::ErrorKind::InvalidUtf8,
                            "toolchain selectors must be UTF-8",
                        ),
                        color,
                    )
                })?;
            toolchain = Some(
                Override::try_parse_from(["zkcl", value])
                    .map_err(|error| ParseError::new::<Override>(error, color))?
                    .toolchain,
            );
            remaining.remove(0);
            continue;
        }
        if !as_package_runner && remaining.first().is_some_and(|arg| arg == "toolchain") {
            let request: Management = parse_owned(
                std::iter::once(OsString::from("zkcl toolchain"))
                    .chain(forwarded_prefix)
                    .chain(remaining.into_iter().skip(1))
                    .collect(),
                color,
            )?;
            color = request.color;
            break Operation::Manage(request);
        }
        if !as_package_runner && remaining.first().is_some_and(|arg| arg == "__bootstrap") {
            let request: Bootstrap = parse_owned(
                std::iter::once(OsString::from("zkcl __bootstrap"))
                    .chain(remaining.into_iter().skip(1))
                    .collect(),
                color,
            )?;
            request
                .validate()
                .map_err(|error| ParseError::new::<Bootstrap>(error, color))?;
            break Operation::Bootstrap(request);
        }
        forwarded_prefix.extend(remaining);
        break Operation::Launch(forwarded_prefix);
    };
    Ok(Invocation {
        directories,
        toolchain,
        as_package_runner,
        color,
        operation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Invocation {
        parse(
            std::iter::once("zkcl")
                .chain(args.iter().copied())
                .map(OsString::from),
        )
        .unwrap()
    }

    fn forwarded(invocation: Invocation) -> Vec<OsString> {
        let Operation::Launch(args) = invocation.operation else {
            panic!("expected forwarding")
        };
        args
    }

    #[test]
    fn preserves_evolving_downstream_arguments() {
        let args = [
            "run",
            "--future-option=value",
            "--future-input",
            "some value",
            "--",
            "--help",
            "+beta",
            "-C",
            "child",
        ];
        assert_eq!(forwarded(parse_args(&args)), args.map(OsString::from));
        assert_eq!(
            forwarded(parse_args(&["--help"])),
            [OsString::from("--help")]
        );
        assert_eq!(
            forwarded(parse_args(&["--future-option", "value", "toolchain"])),
            ["--future-option", "value", "toolchain"].map(OsString::from)
        );
    }

    #[test]
    fn consumes_stable_prefixes_once_and_in_order() {
        let invocation = parse_args(&[
            "-C",
            "parent",
            "+beta",
            "--directory=child",
            "--color=always",
            "-Cgrandchild",
            "run",
            "--future",
        ]);
        assert_eq!(
            invocation.directories,
            ["parent", "child", "grandchild"].map(PathBuf::from)
        );
        assert_eq!(invocation.toolchain, Some(Selector::Channel(Channel::Beta)));
        assert_eq!(invocation.color, Color::Always);
        assert_eq!(
            forwarded(invocation),
            ["--color=always", "run", "--future"].map(OsString::from)
        );
    }

    #[test]
    fn preserves_color_spelling_and_delimiter() {
        assert_eq!(
            forwarded(parse_args(&["--color", "never", "-C", "child", "run"])),
            ["--color", "never", "run"].map(OsString::from)
        );
        assert_eq!(
            forwarded(parse_args(&[
                "-C",
                "child",
                "--",
                "+beta",
                "toolchain",
                "-C",
                "other"
            ])),
            ["--", "+beta", "toolchain", "-C", "other"].map(OsString::from)
        );
        assert_eq!(
            forwarded(parse_args(&["--color=never", "--", "toolchain"])),
            ["--color=never", "--", "toolchain"].map(OsString::from)
        );
    }

    #[test]
    fn package_runner_keeps_management_names_opaque() {
        let invocation = parse(
            [
                "/installed/bin/zkclx",
                "+nightly",
                "toolchain",
                "install",
                "stable",
            ]
            .map(OsString::from),
        )
        .unwrap();
        assert!(invocation.as_package_runner);
        assert_eq!(
            invocation.toolchain,
            Some(Selector::Channel(Channel::Nightly))
        );
        assert_eq!(
            forwarded(invocation),
            ["toolchain", "install", "stable"].map(OsString::from)
        );
    }

    #[test]
    fn parses_global_color_on_either_side_of_management() {
        for args in [
            vec!["--color", "never", "toolchain", "list"],
            vec!["toolchain", "list", "--color=never"],
            vec![
                "--color=always",
                "+stable",
                "toolchain",
                "list",
                "--color=never",
            ],
        ] {
            let invocation = parse_args(&args);
            assert_eq!(invocation.color, Color::Never);
            assert!(matches!(
                invocation.operation,
                Operation::Manage(Management {
                    command: ManagementCommand::List,
                    ..
                })
            ));
        }
    }

    #[test]
    fn parses_bootstrap_fields_in_any_order() {
        let invocation = parse_args(&[
            "__bootstrap",
            "--resolved",
            "1.2.0",
            "--toolchain",
            "stable",
            "--from",
            "https://example.com/releases",
            "--prefix",
            "a directory",
        ]);
        let Operation::Bootstrap(request) = invocation.operation else {
            panic!("expected bootstrap")
        };
        assert_eq!(request.prefix, PathBuf::from("a directory"));
        assert_eq!(request.selector, Selector::Channel(Channel::Stable));
        assert_eq!(request.id.to_string(), "1.2.0");
    }

    #[test]
    fn help_does_not_need_an_installation_or_existing_directory() {
        for args in [
            vec!["toolchain", "install", "stable", "--help"],
            vec![
                "-C",
                "nonexistent-directory",
                "toolchain",
                "pin",
                "stable",
                "--help",
            ],
            vec![
                "toolchain",
                "target",
                "add",
                "x86_64-unknown-linux-gnu",
                "--help",
            ],
            vec!["__bootstrap", "--prefix", "nonexistent", "--help"],
        ] {
            let error = parse(std::iter::once("zkcl").chain(args).map(OsString::from)).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
            assert_eq!(error.exit_code(), 0);
        }
    }

    #[test]
    fn help_uses_the_declared_color_option_on_either_side_of_help() {
        for args in [
            vec!["--color=never", "toolchain", "install", "stable", "--help"],
            vec!["toolchain", "install", "stable", "--help", "--color=never"],
            vec!["toolchain", "install", "stable", "--color=never", "--help"],
        ] {
            let error = parse(std::iter::once("zkcl").chain(args).map(OsString::from)).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
            assert_eq!(error.color, Color::Never);
        }
    }

    #[test]
    fn invalid_owned_arguments_are_usage_errors() {
        for args in [
            vec!["-C"],
            vec!["+stable", "+beta", "--version"],
            vec!["+invalid", "--version"],
            vec!["--color", "violet", "toolchain", "list"],
            vec!["toolchain", "install", "stable", "extra"],
            vec!["toolchain", "pin"],
            vec!["toolchain", "component", "add", "unknown"],
            vec!["toolchain", "upgrade", "1.2.0"],
            vec!["toolchain", "install", "--future"],
            vec!["__bootstrap", "--prefix", "prefix"],
            vec![
                "__bootstrap",
                "--prefix",
                "prefix",
                "--from",
                "https://example.invalid/releases",
                "--toolchain",
                "beta",
                "--resolved",
                "1.0.0",
            ],
        ] {
            let error = parse(
                std::iter::once("zkcl")
                    .chain(args.iter().copied())
                    .map(OsString::from),
            )
            .unwrap_err();
            assert_eq!(error.exit_code(), 2, "{args:?}: {error:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn forwards_non_utf8_paths_and_values() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"file-\xff.lisp".to_vec());
        let directory = OsString::from_vec(b"directory-\xff".to_vec());
        let invocation = parse([
            "zkcl".into(),
            "-C".into(),
            directory.clone(),
            "run".into(),
            path.clone(),
            "--future".into(),
            path.clone(),
        ])
        .unwrap();
        assert_eq!(invocation.directories, [PathBuf::from(directory.clone())]);
        let mut attached = b"-C".to_vec();
        attached.extend(directory.as_encoded_bytes());
        let invocation_attached = parse([
            "zkcl".into(),
            OsString::from_vec(attached),
            "+beta".into(),
            "run".into(),
            path.clone(),
        ])
        .unwrap();
        assert_eq!(invocation_attached.directories, [PathBuf::from(directory)]);
        assert_eq!(forwarded(invocation_attached), ["run".into(), path.clone()]);
        assert_eq!(
            forwarded(invocation),
            ["run".into(), path.clone(), "--future".into(), path]
        );
    }

    #[test]
    fn command_declarations_are_consistent() {
        Prefix::command().debug_assert();
        Management::command().debug_assert();
        Bootstrap::command().debug_assert();
    }
}
