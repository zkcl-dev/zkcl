//! Target properties visible to the Lisp compiler, independent of the code generator.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use target_lexicon::{
    Architecture, Endianness, Environment, OperatingSystem as Os, PointerWidth, Triple,
};

#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error(transparent)]
    Parse(#[from] target_lexicon::ParseError),
    #[error("target `{0}` requires a known 32-bit or 64-bit pointer width and byte order")]
    UnsupportedLayout(Triple),
}

/// The target for which Lisp source is read, macroexpanded, and compiled.
///
/// A JavaScript target has its own object representation and no machine ABI.
/// Machine layout is available only through [`Self::machine`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompilationTarget {
    kind: TargetKind,
    features: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TargetKind {
    Machine(MachineTarget),
    JavaScript,
}

/// A machine ABI used by native and linear-memory WebAssembly code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineTarget {
    triple: Triple,
    pointer_width: PointerWidth,
    endianness: Endianness,
    architecture: Cow<'static, str>,
    os: Cow<'static, str>,
    environment: Environment,
}

/// JavaScript fixnums are exact integer numbers in this inclusive range.
///
/// Products outside the range promote to arbitrary precision. This bound does
/// not describe a JS engine's private tagged representation or pointer width.
pub const JAVASCRIPT_FIXNUM_BOUNDS: (i64, i64) = (-(1 << 28), (1 << 28) - 1);

/// Number of low tag bits in a Lisp word.
pub const FIXNUM_SHIFT: u32 = 3;

/// Inclusive bounds of a signed integer immediate at a target pointer width.
pub const fn fixnum_bounds_for_bits(bits: u8) -> Option<(i64, i64)> {
    match bits {
        16 | 32 | 64 => {
            let magnitude = 1i64 << (bits as u32 - FIXNUM_SHIFT - 1);
            Some((-magnitude, magnitude - 1))
        }
        _ => None,
    }
}

/// Inclusive bounds for a target-lexicon pointer width.
pub fn fixnum_bounds(width: PointerWidth) -> (i64, i64) {
    fixnum_bounds_for_bits(width.bits()).expect("target-lexicon pointer width")
}

impl CompilationTarget {
    /// The target for which the running program was built.
    pub fn native() -> Self {
        Self::from_triple(Triple::host()).expect("zkcl requires a known native word layout")
    }

    pub fn javascript() -> Self {
        Self {
            kind: TargetKind::JavaScript,
            features: [
                "ZKCL",
                "COMMON-LISP",
                "PACKAGE-LOCAL-NICKNAMES",
                "JAVASCRIPT",
            ]
            .map(str::to_owned)
            .into(),
        }
    }

    pub fn is_javascript(&self) -> bool {
        matches!(self.kind, TargetKind::JavaScript)
    }

    pub fn machine(&self) -> Option<&MachineTarget> {
        match &self.kind {
            TargetKind::Machine(machine) => Some(machine),
            TargetKind::JavaScript => None,
        }
    }

    pub fn from_triple(triple: Triple) -> Result<Self, TargetError> {
        let (Ok(pointer_width @ (PointerWidth::U64 | PointerWidth::U32)), Ok(endianness)) =
            (triple.pointer_width(), triple.endianness())
        else {
            return Err(TargetError::UnsupportedLayout(triple));
        };
        let cpu = triple.architecture;
        let architecture: Cow<'static, str> = match cpu {
            Architecture::Aarch64(_) => "aarch64".into(),
            Architecture::Arm(_) => "arm".into(),
            Architecture::X86_32(_) => "x86".into(),
            Architecture::X86_64h => "x86_64".into(),
            Architecture::Powerpc64le => "powerpc64".into(),
            Architecture::Riscv32(_) => "riscv32".into(),
            Architecture::Riscv64(_) => "riscv64".into(),
            Architecture::Mips32(_) => "mips".into(),
            Architecture::Mips64(_) => "mips64".into(),
            _ => cpu.into_str(),
        };
        let os: Cow<'static, str> = match triple.operating_system {
            Os::Linux
                if matches!(
                    triple.environment,
                    Environment::Android | Environment::Androideabi
                ) =>
            {
                "android".into()
            }
            Os::Darwin(_) | Os::MacOSX(_) => "macos".into(),
            Os::IOS(_) => "ios".into(),
            Os::TvOS(_) => "tvos".into(),
            Os::WatchOS(_) => "watchos".into(),
            Os::XROS(_) | Os::VisionOS(_) => "visionos".into(),
            os => os.into_str(),
        };
        // Rust's original Intel iOS simulator triples omit the environment field.
        let environment = match (triple.environment, triple.operating_system, cpu) {
            (
                Environment::Unknown,
                Os::IOS(_) | Os::WatchOS(_) | Os::TvOS(_),
                Architecture::X86_32(_) | Architecture::X86_64,
            ) => Environment::Sim,
            (environment, _, _) => environment,
        };
        let mut features: Vec<String> = ["ZKCL", "COMMON-LISP", "PACKAGE-LOCAL-NICKNAMES"]
            .map(str::to_owned)
            .into();
        features.push(format!("{}-BIT", pointer_width.bits()));
        features.push(
            match endianness {
                Endianness::Little => "LITTLE-ENDIAN",
                Endianness::Big => "BIG-ENDIAN",
            }
            .into(),
        );
        features.push(match architecture.as_ref() {
            "aarch64" => "ARM64".into(),
            "x86_64" => "X86-64".into(),
            "powerpc" => "PPC".into(),
            "powerpc64" => "PPC64".into(),
            "mips64" => "MIPS".into(),
            arch => arch.to_ascii_uppercase(),
        });
        if !matches!(triple.operating_system, Os::Unknown | Os::None_) {
            features.push(os.to_ascii_uppercase());
        }
        let darwin = triple.operating_system.is_like_darwin();
        if darwin {
            features.push("DARWIN".into());
        }
        if darwin
            || matches!(
                triple.operating_system,
                Os::Freebsd | Os::Openbsd | Os::Netbsd | Os::Dragonfly | Os::Bitrig
            )
        {
            features.push("BSD".into());
            features.push("UNIX".into());
        } else if matches!(
            triple.operating_system,
            Os::Aix
                | Os::Cygwin
                | Os::Linux
                | Os::Solaris
                | Os::Illumos
                | Os::Haiku
                | Os::Hurd
                | Os::Fuchsia
                | Os::Redox
                | Os::Emscripten
                | Os::Espidf
                | Os::L4re
                | Os::VxWorks
        ) {
            features.push("UNIX".into());
        }
        if matches!(triple.operating_system, Os::Solaris | Os::Illumos) {
            features.push("SUNOS".into());
        }
        if os == "android" {
            features.push("LINUX".into());
        }
        match environment {
            Environment::Sim => features.push("SIMULATOR".into()),
            Environment::Android | Environment::Unknown | Environment::None => (),
            abi => features.push(abi.into_str().to_ascii_uppercase()),
        }
        Ok(Self {
            kind: TargetKind::Machine(MachineTarget {
                triple,
                pointer_width,
                endianness,
                architecture,
                os,
                environment,
            }),
            features,
        })
    }

    /// Initial keyword names for `*FEATURES*`, before host extensions or Lisp bindings.
    pub fn features(&self) -> impl Iterator<Item = &str> {
        self.features.iter().map(String::as_str)
    }

    /// Inclusive fixnum bounds for this target's Lisp object representation.
    pub fn fixnum_bounds(&self) -> (i64, i64) {
        match &self.kind {
            TargetKind::Machine(machine) => fixnum_bounds(machine.pointer_width),
            TargetKind::JavaScript => JAVASCRIPT_FIXNUM_BOUNDS,
        }
    }

    /// Deployment versions do not change the Lisp target. CPU, vendor, OS family,
    /// ABI environment and object format identify the platform of a compiled file.
    pub fn is_compatible_with(&self, other: &Self) -> bool {
        match (&self.kind, &other.kind) {
            (TargetKind::Machine(left), TargetKind::Machine(right)) => {
                left.is_compatible_with(right)
            }
            (TargetKind::JavaScript, TargetKind::JavaScript) => true,
            _ => false,
        }
    }
}

impl MachineTarget {
    pub fn triple(&self) -> &Triple {
        &self.triple
    }

    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    pub fn operating_system(&self) -> &str {
        &self.os
    }

    pub fn environment(&self) -> Environment {
        self.environment
    }

    pub fn pointer_width(&self) -> PointerWidth {
        self.pointer_width
    }

    pub fn endianness(&self) -> Endianness {
        self.endianness
    }

    fn is_compatible_with(&self, other: &Self) -> bool {
        self.triple.architecture == other.triple.architecture
            && self.triple.vendor == other.triple.vendor
            && self.os == other.os
            && self.environment == other.environment
            && self.triple.binary_format == other.triple.binary_format
    }
}

impl fmt::Display for CompilationTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TargetKind::Machine(machine) => machine.triple.fmt(formatter),
            TargetKind::JavaScript => formatter.write_str("js"),
        }
    }
}

impl FromStr for CompilationTarget {
    type Err = TargetError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "js" => Ok(Self::javascript()),
            _ => Self::from_triple(s.parse()?),
        }
    }
}

impl Default for CompilationTarget {
    fn default() -> Self {
        Self::native()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(triple: &str) -> CompilationTarget {
        triple.parse().unwrap()
    }

    #[test]
    fn target_features_describe_architecture_os_and_abi() {
        for (triple, expected, absent) in [
            (
                "aarch64-apple-darwin",
                &["ARM64", "64-BIT", "MACOS", "DARWIN", "BSD", "UNIX"][..],
                &["IOS", "SIMULATOR"][..],
            ),
            (
                "aarch64-apple-ios",
                &["ARM64", "IOS", "DARWIN"],
                &["MACOS", "SIMULATOR"],
            ),
            (
                "aarch64-apple-ios-sim",
                &["ARM64", "IOS", "SIMULATOR"],
                &["MACOS"],
            ),
            (
                "x86_64-apple-ios",
                &["X86-64", "IOS", "SIMULATOR"],
                &["MACOS"],
            ),
            (
                "aarch64-linux-android",
                &["ARM64", "ANDROID", "LINUX", "UNIX"],
                &["GNU", "DARWIN"],
            ),
            (
                "x86_64-unknown-linux-gnu",
                &["X86-64", "LINUX", "UNIX", "GNU"],
                &["ANDROID", "DARWIN"],
            ),
            (
                "x86_64-pc-windows-msvc",
                &["X86-64", "64-BIT", "LITTLE-ENDIAN", "WINDOWS", "MSVC"],
                &["X86", "UNIX"],
            ),
            (
                "i686-pc-windows-gnu",
                &["X86", "32-BIT", "WINDOWS", "GNU"],
                &["X86-64", "64-BIT", "MSVC"],
            ),
            (
                "x86_64-unknown-linux-gnux32",
                &["X86-64", "32-BIT", "GNUX32"],
                &["64-BIT"],
            ),
            (
                "s390x-unknown-linux-gnu",
                &["S390X", "BIG-ENDIAN"],
                &["LITTLE-ENDIAN"],
            ),
        ] {
            let target = target(triple);
            let features = target.features().collect::<Vec<_>>();
            assert!(
                expected.iter().all(|feature| features.contains(feature)),
                "{triple}: {features:?}"
            );
            assert!(
                absent.iter().all(|feature| !features.contains(feature)),
                "{triple}: {features:?}"
            );
        }
        let native = CompilationTarget::native();
        let machine = native.machine().unwrap();
        assert_eq!(machine.pointer_width.bits() as u32, usize::BITS);
        assert_eq!(
            machine.endianness,
            if cfg!(target_endian = "little") {
                Endianness::Little
            } else {
                Endianness::Big
            }
        );
    }

    #[test]
    fn javascript_has_language_identity_without_a_machine_layout() {
        let javascript = target("js");
        assert_eq!(javascript, CompilationTarget::javascript());
        assert_eq!(javascript.to_string(), "js");
        assert!(javascript.is_javascript());
        assert!(javascript.machine().is_none());
        assert_eq!(javascript.fixnum_bounds(), JAVASCRIPT_FIXNUM_BOUNDS);
        let features = javascript.features().collect::<Vec<_>>();
        assert!(features.contains(&"JAVASCRIPT"));
        assert!(!features.contains(&"32-BIT"));
        assert!(!features.contains(&"WASM32"));
        assert!(javascript.is_compatible_with(&target("js")));
        assert!(!javascript.is_compatible_with(&target("wasm32-unknown-unknown")));
        assert!(!javascript.is_compatible_with(&CompilationTarget::native()));
        assert!(!CompilationTarget::native().is_compatible_with(&javascript));
    }

    #[test]
    fn compiled_file_identity_preserves_abi_and_ignores_deployment_versions() {
        assert!(
            target("aarch64-apple-darwin")
                .is_compatible_with(&target("aarch64-apple-macosx14.0.0"))
        );
        assert!(target("x86_64-apple-ios").is_compatible_with(&target("x86_64-apple-ios-sim")));
        assert!(!target("aarch64-apple-ios").is_compatible_with(&target("aarch64-apple-ios-sim")));
        assert!(!target("aarch64-apple-ios").is_compatible_with(&target("aarch64-apple-darwin")));
        assert!(
            !target("aarch64-linux-android")
                .is_compatible_with(&target("aarch64-unknown-linux-gnu"))
        );
        assert!(
            !target("x86_64-pc-windows-msvc").is_compatible_with(&target("x86_64-pc-windows-gnu"))
        );
        assert_eq!(
            target("i686-pc-windows-gnu").fixnum_bounds(),
            (-(1 << 28), (1 << 28) - 1)
        );
    }
}
