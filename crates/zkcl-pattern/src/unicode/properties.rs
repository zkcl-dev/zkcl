use std::ops::RangeInclusive;

use icu_properties::props::*;
use icu_properties::script::ScriptWithExtensions;
use icu_properties::{
    CodePointMapData, CodePointSetData, CodePointSetDataBorrowed, PropertyParser,
};

#[derive(Clone, Copy, Debug)]
pub enum Property {
    Binary(CodePointSetDataBorrowed<'static>, bool),
    Category(GeneralCategoryGroup),
    Script(Script, bool),
    Ascii(bool),
    Any(bool),
    Assigned(bool),
}

#[derive(Debug, thiserror::Error)]
#[error("unknown Unicode property or value: {0}")]
pub struct PropertyError(pub String);

fn normalize_property_alias(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' ' | '\t' | '\n' | '\r'))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

macro_rules! binary {
    ($name:expr, $($kind:ident => $($value:literal)|+),+ $(,)?) => {
        match $name {
            $($($value)|+ => Some(CodePointSetData::new::<$kind>()),)+
            _ => None,
        }
    }
}

impl Property {
    pub fn parse(name: &str, value: Option<&str>) -> Result<Self, PropertyError> {
        let key = normalize_property_alias(name);
        let invalid =
            || PropertyError(value.map_or_else(|| name.into(), |value| format!("{name}={value}")));
        match key.as_str() {
            "any" | "ascii" | "assigned" => {
                let expected_membership = match value.map(normalize_property_alias).as_deref() {
                    None | Some("y" | "yes" | "t" | "true") => true,
                    Some("n" | "no" | "f" | "false") => false,
                    _ => return Err(invalid()),
                };
                Ok(match key.as_str() {
                    "any" => Self::Any(expected_membership),
                    "ascii" => Self::Ascii(expected_membership),
                    _ => Self::Assigned(expected_membership),
                })
            }
            "gc" | "generalcategory" => PropertyParser::<GeneralCategoryGroup>::new()
                .get_loose(value.ok_or_else(invalid)?)
                .map(Self::Category)
                .ok_or_else(invalid),
            "sc" | "script" | "scx" | "scriptextensions" => PropertyParser::<Script>::new()
                .get_loose(value.ok_or_else(invalid)?)
                .map(|script| {
                    Self::Script(script, matches!(key.as_str(), "scx" | "scriptextensions"))
                })
                .ok_or_else(invalid),
            key => {
                let set = binary!(key,
                    Alphabetic => "alpha" | "alphabetic",
                    BidiControl => "bidic" | "bidicontrol",
                    BidiMirrored => "bidim" | "bidimirrored",
                    Cased => "cased",
                    CaseIgnorable => "ci" | "caseignorable",
                    Dash => "dash",
                    DefaultIgnorableCodePoint => "di" | "defaultignorablecodepoint",
                    Deprecated => "dep" | "deprecated",
                    Diacritic => "dia" | "diacritic",
                    Emoji => "emoji",
                    EmojiPresentation => "epres" | "emojipresentation",
                    EmojiModifier => "emod" | "emojimodifier",
                    EmojiModifierBase => "ebase" | "emojimodifierbase",
                    ExtendedPictographic => "extpict" | "extendedpictographic",
                    GraphemeExtend => "grext" | "graphemeextend",
                    HexDigit => "hex" | "hexdigit",
                    AsciiHexDigit => "ahex" | "asciihexdigit",
                    IdContinue => "idc" | "idcontinue",
                    IdStart => "ids" | "idstart",
                    Ideographic => "ideo" | "ideographic",
                    JoinControl => "joinc" | "joincontrol",
                    Lowercase => "lower" | "lowercase",
                    Math => "math",
                    NoncharacterCodePoint => "nchar" | "noncharactercodepoint",
                    PatternSyntax => "patsyn" | "patternsyntax",
                    PatternWhiteSpace => "patws" | "patternwhitespace",
                    QuotationMark => "qmark" | "quotationmark",
                    RegionalIndicator => "ri" | "regionalindicator",
                    SoftDotted => "sd" | "softdotted",
                    TerminalPunctuation => "term" | "terminalpunctuation",
                    UnifiedIdeograph => "uideo" | "unifiedideograph",
                    Uppercase => "upper" | "uppercase",
                    VariationSelector => "vs" | "variationselector",
                    WhiteSpace => "space" | "wspace" | "whitespace",
                    XidContinue => "xidc" | "xidcontinue",
                    XidStart => "xids" | "xidstart",
                );
                if let Some(set) = set {
                    let expected_membership = match value.map(normalize_property_alias).as_deref() {
                        None | Some("y" | "yes" | "t" | "true") => true,
                        Some("n" | "no" | "f" | "false") => false,
                        _ => return Err(invalid()),
                    };
                    Ok(Self::Binary(set, expected_membership))
                } else if value.is_none() {
                    PropertyParser::<GeneralCategoryGroup>::new()
                        .get_loose(name)
                        .map(Self::Category)
                        .ok_or_else(invalid)
                } else {
                    Err(invalid())
                }
            }
        }
    }

    pub fn contains(self, character: char) -> bool {
        match self {
            Self::Binary(set, expected_membership) => {
                set.contains(character) == expected_membership
            }
            Self::Category(group) => {
                group.contains(CodePointMapData::<GeneralCategory>::new().get(character))
            }
            Self::Script(script, false) => {
                CodePointMapData::<Script>::new().get(character) == script
            }
            Self::Script(script, true) => ScriptWithExtensions::new().has_script(character, script),
            Self::Ascii(expected) => character.is_ascii() == expected,
            Self::Any(expected) => expected,
            Self::Assigned(expected) => {
                (CodePointMapData::<GeneralCategory>::new().get(character)
                    != GeneralCategory::Unassigned)
                    == expected
            }
        }
    }

    pub fn ranges(self) -> Vec<RangeInclusive<u32>> {
        match self {
            Self::Binary(set, true) => set.iter_ranges().collect(),
            Self::Binary(set, false) => set.iter_ranges_complemented().collect(),
            Self::Category(group) => CodePointMapData::<GeneralCategory>::new()
                .iter_ranges_for_group(group)
                .collect(),
            Self::Script(script, false) => CodePointMapData::<Script>::new()
                .iter_ranges_for_value(script)
                .collect(),
            Self::Script(script, true) => ScriptWithExtensions::new()
                .get_script_extensions_ranges(script)
                .collect(),
            Self::Ascii(true) => std::iter::once(0..=0x7f).collect(),
            Self::Ascii(false) => vec![0x80..=0xd7ff, 0xe000..=0x10ffff],
            Self::Any(true) => vec![0..=0xd7ff, 0xe000..=0x10ffff],
            Self::Any(false) => Vec::new(),
            Self::Assigned(true) => CodePointMapData::<GeneralCategory>::new()
                .iter_ranges_for_group(GeneralCategoryGroup::Unassigned.complement())
                .collect(),
            Self::Assigned(false) => CodePointMapData::<GeneralCategory>::new()
                .iter_ranges_for_group(GeneralCategoryGroup::Unassigned)
                .collect(),
        }
    }
}
