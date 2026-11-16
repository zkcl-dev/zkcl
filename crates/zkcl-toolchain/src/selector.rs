use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use clap::ValueEnum;
use jiff::civil::Date;
use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Channel {
    Stable,
    Beta,
    Nightly,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) enum ReleaseId {
    Version(Version),
    Nightly(Date),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) enum Selector {
    Channel(Channel),
    Exact(ReleaseId),
}

impl Channel {
    pub(crate) fn accepts(self, id: &ReleaseId) -> bool {
        match (self, id) {
            (Self::Stable, ReleaseId::Version(version)) => version.pre.is_empty(),
            (Self::Beta, ReleaseId::Version(version)) => {
                matches!(version.pre.as_str().split('.').next(), Some("beta" | "rc"))
            }
            (Self::Nightly, ReleaseId::Nightly(_)) => true,
            _ => false,
        }
    }
}

impl ReleaseId {
    pub(crate) fn tag(&self) -> String {
        match self {
            Self::Version(version) => format!("v{version}"),
            Self::Nightly(_) => self.to_string(),
        }
    }
}

impl FromStr for Channel {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        match text {
            "stable" => Ok(Self::Stable),
            "beta" => Ok(Self::Beta),
            "nightly" => Ok(Self::Nightly),
            _ => bail!("expected stable, beta, or nightly, got {text:?}"),
        }
    }
}

impl FromStr for ReleaseId {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        if let Some(day) = text.strip_prefix("nightly-")
            && day.len() == 10
            && let Ok(date) = day.parse::<Date>()
            && date.year() > 0
            && date.to_string() == day
        {
            return Ok(Self::Nightly(date));
        }
        if text.len() <= 96
            && let Ok(version) = Version::parse(text)
        {
            return Ok(Self::Version(version));
        }
        bail!("expected an exact version such as 0.1.0 or nightly-2026-11-23, got {text:?}")
    }
}

impl FromStr for Selector {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        match text.parse() {
            Ok(channel) => Ok(Self::Channel(channel)),
            Err(_) => text.parse().map(Self::Exact),
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Nightly => "nightly",
        })
    }
}

impl fmt::Display for ReleaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version(version) => write!(f, "{version}"),
            Self::Nightly(date) => write!(f, "nightly-{date}"),
        }
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Channel(channel) => channel.fmt(f),
            Self::Exact(id) => id.fmt(f),
        }
    }
}

impl From<ReleaseId> for String {
    fn from(id: ReleaseId) -> Self {
        id.to_string()
    }
}

impl TryFrom<String> for ReleaseId {
    type Error = anyhow::Error;

    fn try_from(text: String) -> Result<Self> {
        text.parse()
    }
}

impl From<Selector> for String {
    fn from(selector: Selector) -> Self {
        selector.to_string()
    }
}

impl TryFrom<String> for Selector {
    type Error = anyhow::Error;

    fn try_from(text: String) -> Result<Self> {
        text.parse()
    }
}
