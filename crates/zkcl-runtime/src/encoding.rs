use std::borrow::Cow;
use std::io::{self, Read, Seek};

use crate::condition::{self, Class, Fault};
use crate::heap::Word;
use crate::package::Package;
use crate::{EvalError, Runtime};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ExternalFormat {
    #[default]
    Default,
    Utf8,
    Latin1,
}

impl ExternalFormat {
    pub(crate) fn parse(
        runtime: &Runtime,
        value: Option<Word>,
        operator: &str,
        at: usize,
    ) -> Result<Self, Fault> {
        let Some(value) = value else {
            return Ok(Self::Default);
        };
        match condition::named_word(runtime, value, Package::KEYWORD).as_deref() {
            Some("DEFAULT") => Ok(Self::Default),
            Some("UTF-8") => Ok(Self::Utf8),
            Some("LATIN-1" | "ISO-8859-1") => Ok(Self::Latin1),
            _ => Err(Fault::plain(
                Class::Error,
                EvalError::new(
                    at,
                    format!("{operator} supports :DEFAULT, :UTF-8 and :LATIN-1 external formats"),
                ),
            )),
        }
    }

    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::Utf8 => "UTF-8",
            Self::Latin1 => "LATIN-1",
        }
    }

    pub(crate) fn decode(self, bytes: &[u8]) -> Result<Cow<'_, str>, std::str::Utf8Error> {
        match self {
            Self::Latin1 => Ok(Cow::Owned(decode_latin1(bytes))),
            _ => std::str::from_utf8(bytes).map(Cow::Borrowed),
        }
    }

    pub(crate) fn decode_owned(self, bytes: Vec<u8>) -> Result<String, std::str::Utf8Error> {
        match self {
            Self::Latin1 => Ok(decode_latin1(&bytes)),
            _ => String::from_utf8(bytes).map_err(|error| error.utf8_error()),
        }
    }

    pub(crate) fn read_char(self, input: &mut (impl Read + Seek)) -> io::Result<Option<char>> {
        let mut bytes = [0; 4];
        match input.read_exact(&mut bytes[..1]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        if self == Self::Latin1 || bytes[0].is_ascii() {
            return Ok(Some(char::from(bytes[0])));
        }
        let width = match bytes[0] {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 1,
        };
        let mut tail = (&mut *input).take((width - 1) as u64);
        let decoded = tail.read_exact(&mut bytes[1..width]).and_then(|()| {
            std::str::from_utf8(&bytes[..width])
                .map(|text| text.chars().next())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        });
        if decoded.is_err() {
            // Report the error at the start of the character, even when its
            // final bytes are missing or belong to a subsequent character.
            let read = width as i64 - tail.limit() as i64;
            input.seek_relative(-read)?;
        }
        decoded
    }

    pub(crate) fn encode(self, text: &str) -> io::Result<Cow<'_, [u8]>> {
        if self != Self::Latin1 {
            return Ok(Cow::Borrowed(text.as_bytes()));
        }
        latin1_bytes(text)
            .collect::<io::Result<Vec<_>>>()
            .map(Cow::Owned)
    }

    pub(crate) fn encoded_byte_length(self, text: &str) -> io::Result<usize> {
        match self {
            Self::Latin1 => latin1_bytes(text).try_fold(0, |length, byte| byte.map(|_| length + 1)),
            _ => Ok(text.len()),
        }
    }

    pub(crate) fn decoded_char_length(self, character: char) -> usize {
        match self {
            Self::Latin1 => 1,
            _ => character.len_utf8(),
        }
    }
}

fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().copied().map(char::from).collect()
}

fn latin1_bytes(text: &str) -> impl Iterator<Item = io::Result<u8>> {
    text.chars().map(|character| {
        u8::try_from(u32::from(character)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("character {character:?} cannot be encoded as Latin-1"),
            )
        })
    })
}
