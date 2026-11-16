use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use zkcl_bytecode::{CompiledSource, FunctionDebug};

/// Source Map v3 anchors in emission order. Generated offsets are relative to
/// the step code, independent of how the caller delivers the runtime.
#[derive(Default)]
pub(super) struct SourceMap<'a> {
    sites: Vec<Site<'a>>,
}

struct Site<'a> {
    generated_byte: usize,
    source: &'a CompiledSource,
    original_byte: usize,
    name: Option<&'a str>,
}

impl<'a> SourceMap<'a> {
    pub(super) fn mark(
        &mut self,
        generated_byte: usize,
        source: Option<&'a CompiledSource>,
        debug: &FunctionDebug,
        at: usize,
        name: Option<&'a str>,
    ) {
        // `at` also serves as a key for generated forms. A position with no
        // recorded range does not establish an occurrence in the Lisp text.
        let Some(source) = source.filter(|source| source.text.is_some()) else {
            return;
        };
        let Some(range) = debug.source_at(at).and_then(|source| source.range.as_ref()) else {
            return;
        };
        self.sites.push(Site {
            generated_byte,
            source,
            original_byte: range.start,
            name,
        });
    }

    pub(super) fn append_inline(
        self,
        output: &mut String,
        steps_start: usize,
    ) -> Result<(), serde_json::Error> {
        let Some(last) = self.sites.last() else {
            return Ok(());
        };
        let generated = LineIndex::new(
            &output[..steps_start + last.generated_byte],
            LineEndings::JavaScript,
        );
        let mut sources = Vec::<OriginalSource>::new();
        let mut by_snapshot = HashMap::new();
        let mut by_url = HashMap::new();
        let mut names = Vec::new();
        let mut name_indices = HashMap::new();
        let mut mappings = Mappings::default();
        let mut mapped_line = None;
        for site in self.sites {
            let index = *by_snapshot
                .entry(std::ptr::from_ref(site.source))
                .or_insert_with(|| {
                    let url = source_url(site.source)?;
                    let text = site.source.text.as_deref()?;
                    if let Some(&index) = by_url.get(url.as_str()) {
                        // A consumer looks up snapshot text by URL. Ambiguous
                        // revisions cannot safely point at the same source.
                        let known: &OriginalSource<'_> = &sources[index];
                        return (known.lines.text == text).then_some(index);
                    }
                    let index = sources.len();
                    by_url.insert(url.clone(), index);
                    sources.push(OriginalSource {
                        url,
                        lines: LineIndex::new(text, LineEndings::Lisp),
                    });
                    Some(index)
                });
            let Some(index) = index.and_then(|value| i32::try_from(value).ok()) else {
                continue;
            };
            let Some(original) = sources[index as usize].lines.position(site.original_byte) else {
                continue;
            };
            let Some(generated) = generated.position(steps_start + site.generated_byte) else {
                continue;
            };
            if let Some(line) = mapped_line.filter(|&line| line < generated.line) {
                mappings.unmapped_line_after(line);
            }
            let name = site.name.filter(|name| !name.is_empty()).and_then(|name| {
                *name_indices.entry(name).or_insert_with(|| {
                    let index = i32::try_from(names.len()).ok()?;
                    names.push(name);
                    Some(index)
                })
            });
            mappings.mapped(generated, index, original, name);
            mapped_line = Some(generated.line);
        }
        let Some(line) = mapped_line else {
            return Ok(());
        };
        mappings.unmapped_line_after(line);

        // Serialize borrowed snapshots directly. A JSON value tree would copy
        // every retained Lisp source before encoding the map into the module.
        let urls: Vec<_> = sources.iter().map(|source| source.url.as_str()).collect();
        let texts: Vec<_> = sources.iter().map(|source| source.lines.text).collect();
        let mut json = Vec::from(&b"{\"version\":3,\"sources\":"[..]);
        serde_json::to_writer(&mut json, &urls)?;
        json.extend_from_slice(b",\"sourcesContent\":");
        serde_json::to_writer(&mut json, &texts)?;
        json.extend_from_slice(b",\"names\":");
        serde_json::to_writer(&mut json, &names)?;
        json.extend_from_slice(b",\"mappings\":");
        serde_json::to_writer(&mut json, &mappings.text)?;
        json.push(b'}');
        output.push_str("//# sourceMappingURL=data:application/json;base64,");
        STANDARD.encode_string(json, output);
        output.push('\n');
        Ok(())
    }
}

struct OriginalSource<'a> {
    url: String,
    lines: LineIndex<'a>,
}

fn source_url(source: &CompiledSource) -> Option<String> {
    if let Some(uri) = source
        .uri
        .as_deref()
        .and_then(|uri| url::Url::parse(uri).ok())
    {
        return Some(uri.into());
    }
    // An unqualified fragment only establishes positions inside its snapshot,
    // not in the file as a whole. An explicit URI can identify a virtual source.
    if source.base_offset != 0 {
        return None;
    }
    let path = source.path.as_deref().filter(|path| !path.is_empty())?;
    let path_obj = Path::new(path);
    if path_obj.is_absolute() {
        return url::Url::from_file_path(path_obj).ok().map(Into::into);
    }
    // Preserve the relative name recorded by the compiler. Encode the path as
    // a URL reference so that file names containing '#' and '?' stay intact.
    let mut relative = String::new();
    for byte in path.bytes() {
        match byte {
            b'\\' if cfg!(windows) => relative.push('/'),
            b'/' | b'.' | b'-' | b'_' | b'~' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => {
                relative.push(char::from(byte));
            }
            _ => write!(relative, "%{byte:02X}").unwrap(),
        }
    }
    Some(relative)
}

#[derive(Clone, Copy, Default)]
struct Position {
    line: i32,
    column: i32,
}

struct LineIndex<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

enum LineEndings {
    Lisp,
    JavaScript,
}

impl<'a> LineIndex<'a> {
    fn new(text: &'a str, endings: LineEndings) -> Self {
        let mut starts = vec![0];
        let mut chars = text.char_indices().peekable();
        while let Some((byte, ch)) = chars.next() {
            match ch {
                '\r' if chars.peek().is_some_and(|&(_, ch)| ch == '\n') => {
                    starts.push(byte + 2);
                    chars.next();
                }
                '\n' | '\r' => starts.push(byte + 1),
                '\u{2028}' | '\u{2029}' if matches!(endings, LineEndings::JavaScript) => {
                    starts.push(byte + ch.len_utf8());
                }
                _ => {}
            }
        }
        Self { text, starts }
    }

    fn position(&self, byte: usize) -> Option<Position> {
        let line = self.starts.partition_point(|&start| start <= byte) - 1;
        let mut text = self.text.get(self.starts[line]..byte)?;
        // A boundary inside CRLF shares the end column of the previous line.
        if self.text.as_bytes().get(byte) == Some(&b'\n') {
            text = text.strip_suffix('\r').unwrap_or(text);
        }
        Some(Position {
            line: line.try_into().ok()?,
            column: text.encode_utf16().count().try_into().ok()?,
        })
    }
}

#[derive(Default)]
struct Mappings {
    text: String,
    generated: Position,
    original: Position,
    source: i32,
    name: i32,
}

impl Mappings {
    fn mapped(&mut self, generated: Position, source: i32, original: Position, name: Option<i32>) {
        self.generated_segment(generated);
        write_vlq(&mut self.text, source - self.source);
        write_vlq(&mut self.text, original.line - self.original.line);
        write_vlq(&mut self.text, original.column - self.original.column);
        self.source = source;
        self.original = original;
        if let Some(name) = name {
            write_vlq(&mut self.text, name - self.name);
            self.name = name;
        }
    }

    fn unmapped_line_after(&mut self, line: i32) {
        // Close the segment at the next line so consumers using a global
        // closest-preceding search do not map dispatch or host scaffolding.
        if let Some(line) = line.checked_add(1) {
            self.generated_segment(Position { line, column: 0 });
        }
    }

    fn generated_segment(&mut self, position: Position) {
        if self.generated.line != position.line {
            for _ in self.generated.line..position.line {
                self.text.push(';');
            }
            self.generated.column = 0;
        } else if self
            .text
            .as_bytes()
            .last()
            .is_some_and(|&byte| byte != b';')
        {
            self.text.push(',');
        }
        write_vlq(&mut self.text, position.column - self.generated.column);
        self.generated = position;
    }
}

// ECMA-426 signed Base64 VLQ. Nonnegative coordinates bounded by i32 make
// their deltas representable without the reserved negative-zero encoding.
fn write_vlq(output: &mut String, delta: i32) {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut value = (delta.unsigned_abs() << 1) | u32::from(delta < 0);
    while value >= 32 {
        let digit = (value & 31) | 32;
        output.push(char::from(DIGITS[digit as usize]));
        value >>= 5;
    }
    output.push(char::from(DIGITS[value as usize]));
}
