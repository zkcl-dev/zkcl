use std::sync::atomic::{AtomicUsize, Ordering};

use crate::OutOfMemory;

use super::array::ArrayElement;
use super::object::{HeapObject, WordSlot};
use super::word::{Address, Header, ObjectKind};
use super::{Heap, Word};

/// An indexed Unicode edit can install a replacement with four bytes per scalar.
/// Its payload holds either indexed chars or their packed UTF-8 view in place.
#[repr(C)]
pub(super) struct LispString {
    header: Header,
    // A UTF-8 byte length, or SCALAR_STORAGE for an unpacked replacement.
    len: AtomicUsize,
    count: usize,
    pub(super) replacement: WordSlot,
}

// SAFETY: `LispString` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::String`.
unsafe impl HeapObject for LispString {
    const KIND: ObjectKind = ObjectKind::String;
}

const SCALAR_STORAGE: usize = usize::MAX;

enum StringLayout {
    Utf8(usize),
    Scalars,
}

/// Materialize a UTF-8 view without allocating. Packing proceeds left to right:
/// each encoded character fits within the four bytes occupied by its scalar.
/// The caller keeps storage live and ends all borrows before mutation or a safepoint.
unsafe fn pack_string(storage: Address) -> usize {
    let header = unsafe { &*storage.to_ptr::<LispString>() };
    let data = (storage + size_of::<LispString>()).to_mut_ptr::<u8>();
    let mut len = header.len.load(Ordering::Relaxed);
    if len == SCALAR_STORAGE {
        len = 0;
        for i in 0..header.count {
            let ch = unsafe { data.cast::<char>().add(i).read() };
            let mut buffer = [0; 4];
            let encoded = ch.encode_utf8(&mut buffer);
            unsafe {
                std::ptr::copy_nonoverlapping(encoded.as_ptr(), data.add(len), encoded.len())
            };
            len += encoded.len();
        }
        header.len.store(len, Ordering::Relaxed);
    }
    len
}

/// Restore fixed-width slots before modifying a replacement. Decoding backward
/// leaves the unread prefix intact. The caller has exclusive access to the payload.
unsafe fn unpack_string(storage: Address) {
    let header = unsafe { &*storage.to_ptr::<LispString>() };
    let mut end = header.len.load(Ordering::Relaxed);
    if end == SCALAR_STORAGE {
        return;
    }
    let data = (storage + size_of::<LispString>()).to_mut_ptr::<u8>();
    for i in (0..header.count).rev() {
        let prefix = unsafe { std::slice::from_raw_parts(data, end) };
        let ch = unsafe { std::str::from_utf8_unchecked(prefix) }
            .chars()
            .next_back()
            .expect("valid scalar count");
        end -= ch.len_utf8();
        unsafe { data.cast::<char>().add(i).write(ch) };
    }
    header.len.store(SCALAR_STORAGE, Ordering::Relaxed);
}

pub(crate) fn string_byte_range(
    text: &str,
    count: usize,
    range: std::ops::Range<usize>,
) -> std::ops::Range<usize> {
    if text.len() == count {
        return range;
    }
    let mut boundaries = text.char_indices().map(|(byte, _)| byte);
    let start = if range.start == count {
        text.len()
    } else {
        boundaries
            .nth(range.start)
            .expect("validated UTF-8 scalar range")
    };
    let end = if range.is_empty() {
        start
    } else if range.end == count {
        text.len()
    } else {
        boundaries
            .nth(range.len() - 1)
            .expect("validated UTF-8 scalar range")
    };
    start..end
}

impl Heap {
    pub(crate) fn string(&self, text: &str) -> Result<Word, OutOfMemory> {
        self.allocate_string(
            StringLayout::Utf8(text.len()),
            text.chars().count(),
            |output| {
                // SAFETY: The allocation provides text.len() bytes at output.
                unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), output, text.len()) };
            },
        )
    }

    fn allocate_string(
        &self,
        layout: StringLayout,
        count: usize,
        initialize: impl FnOnce(*mut u8),
    ) -> Result<Word, OutOfMemory> {
        let (capacity, len) = match layout {
            StringLayout::Utf8(len) => (len, len),
            StringLayout::Scalars => (
                count.checked_mul(size_of::<char>()).ok_or(OutOfMemory)?,
                SCALAR_STORAGE,
            ),
        };
        let size = size_of::<LispString>()
            .checked_add(capacity)
            .and_then(|size| size.checked_next_multiple_of(8))
            .ok_or(OutOfMemory)?;
        let address = self.allocate(size)?;
        // SAFETY: The allocation holds the header followed by capacity payload bytes.
        unsafe {
            address.to_mut_ptr::<LispString>().write(LispString {
                header: Header::new(ObjectKind::String, size),
                len: AtomicUsize::new(len),
                count,
                replacement: WordSlot::nil(),
            });
        }
        initialize((address + size_of::<LispString>()).to_mut_ptr::<u8>());
        Ok(Word::from_object_address(address))
    }

    pub(crate) fn repeated_string(
        &self,
        character: char,
        count: usize,
    ) -> Result<Word, OutOfMemory> {
        let mut buffer = [0; 4];
        let utf8 = character.encode_utf8(&mut buffer).as_bytes();
        let len = utf8.len().checked_mul(count).ok_or(OutOfMemory)?;
        self.allocate_string(StringLayout::Utf8(len), count, |output| {
            // SAFETY: The output has enough bytes for every copy of the character.
            unsafe {
                if let [byte] = utf8 {
                    output.write_bytes(*byte, count);
                } else {
                    for offset in (0..len).step_by(utf8.len()) {
                        std::ptr::copy_nonoverlapping(
                            utf8.as_ptr(),
                            output.add(offset),
                            utf8.len(),
                        );
                    }
                }
            }
        })
    }

    pub(super) fn string_from_characters(
        &self,
        count: usize,
        mut character: impl FnMut(usize) -> char,
    ) -> Result<Word, OutOfMemory> {
        let max_bytes = self
            .max_heap_bytes()
            .saturating_sub(size_of::<LispString>());
        if count > max_bytes {
            return Err(OutOfMemory);
        }
        let len = (0..count).try_fold(0usize, |len, index| {
            len.checked_add(character(index).len_utf8())
                .ok_or(OutOfMemory)
        })?;
        self.allocate_string(StringLayout::Utf8(len), count, |output| {
            // SAFETY: The fresh allocation has exactly len bytes available for initialization.
            let mut remaining = unsafe {
                std::slice::from_raw_parts_mut(output.cast::<std::mem::MaybeUninit<u8>>(), len)
            };
            let mut buffer = [0; 4];
            for index in 0..count {
                let bytes = character(index).encode_utf8(&mut buffer).as_bytes();
                let (slot, rest) = remaining.split_at_mut(bytes.len());
                slot.write_copy_of_slice(bytes);
                remaining = rest;
            }
            assert!(
                remaining.is_empty(),
                "character source changed during allocation"
            );
        })
    }

    pub(super) fn string_data_count(&self, word: Word) -> Option<usize> {
        Some(self.object_ref::<LispString>(word)?.count)
    }

    fn string_storage(&self, word: Word) -> Option<Address> {
        let (address, string) = self.object_with_address::<LispString>(word)?;
        Some(string.replacement.get().object_address().unwrap_or(address))
    }

    pub(super) fn string_data_character(&self, string: Word, index: usize) -> Option<char> {
        let storage = self.string_storage(string)?;
        // SAFETY: array_storage checked the scalar index and there is no safepoint.
        let header = unsafe { &*storage.to_ptr::<LispString>() };
        let len = header.len.load(Ordering::Relaxed);
        let data = storage + size_of::<LispString>();
        if len == SCALAR_STORAGE {
            // SAFETY: Every scalar slot contains an initialized char.
            return Some(unsafe { *data.to_ptr::<char>().add(index) });
        }
        // SAFETY: The payload is valid UTF-8 for its recorded byte length.
        let bytes = unsafe { std::slice::from_raw_parts(data.to_ptr::<u8>(), len) };
        if len == header.count {
            Some(char::from(bytes[index]))
        } else {
            unsafe { std::str::from_utf8_unchecked(bytes) }
                .chars()
                .nth(index)
        }
    }

    pub(super) fn set_string_data_character(
        &self,
        string: Word,
        index: usize,
        character: char,
    ) -> Result<bool, OutOfMemory> {
        let mut buffer = [0; 4];
        self.replace_string_data_range(string, index, 1, character.encode_utf8(&mut buffer))
    }

    pub(super) fn replace_string_data_range(
        &self,
        string: Word,
        start: usize,
        length: usize,
        replacement: &str,
    ) -> Result<bool, OutOfMemory> {
        self.assert_may_collect();
        let Some((base, original)) = self.object_with_address::<LispString>(string) else {
            return Ok(false);
        };
        if length == 0 {
            return Ok(true);
        }
        let installed = original.replacement.get();
        let count = original.count;
        let storage = if let Some(address) = installed.object_address() {
            // SAFETY: Replacements have capacity for count chars, and callers of
            // string_slice must release UTF-8 borrows before a Lisp mutation.
            unsafe { unpack_string(address) };
            address
        } else {
            let original_len = original.len.load(Ordering::Relaxed);
            // A full overwrite or an ASCII edit has a known byte range. An
            // indexed Unicode edit needs scalar addressing for subsequent writes.
            let direct_offset = if length == count && replacement.len() == original_len {
                Some(0)
            } else if original_len == count && replacement.len() == length {
                Some(start)
            } else {
                None
            };
            if let Some(offset) = direct_offset {
                let output = (base + size_of::<LispString>() + offset).to_mut_ptr::<u8>();
                // SAFETY: The nonaliasing source spans exactly the replaced UTF-8 bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(replacement.as_ptr(), output, replacement.len())
                };
                return Ok(true);
            }
            let string = self.root_slot(string);
            let wide = self.allocate_string(StringLayout::Scalars, count, |data| {
                // SAFETY: Reload the source from its root after allocation. The
                // destination has count aligned slots and initialization cannot collect.
                let source =
                    unsafe { self.string_data_slice(string.get()) }.expect("rooted string");
                for (i, ch) in source.chars().enumerate() {
                    unsafe { data.cast::<char>().add(i).write(ch) };
                }
            })?;
            let object = self.expect_object::<LispString>(string.get(), "rooted string");
            self.write_slot_object(object, &object.replacement, wide);
            wide.object_address().expect("allocated scalar storage")
        };
        let output = (storage + size_of::<LispString>()).to_mut_ptr::<char>();
        for (i, ch) in replacement.chars().enumerate() {
            // SAFETY: The caller validated the replacement's scalar range.
            unsafe { output.add(start + i).write(ch) };
        }
        Ok(true)
    }

    pub(crate) fn string_contents(&self, word: Word) -> Option<String> {
        self.with_string_slice(word, str::to_owned)
    }

    pub(crate) fn copy_string(&self, word: Word) -> Result<Word, OutOfMemory> {
        let (len, count) = self
            .with_string_slice(word, |text| (text.len(), text.chars().count()))
            .expect("validated string");
        let source = self.root_slot(word);
        self.allocate_string(StringLayout::Utf8(len), count, |output| {
            // SAFETY: The source is rooted, and the allocation provides len
            // bytes at output. There is no further safepoint during the copy.
            unsafe {
                let text = self.string_slice(source.get()).expect("rooted string");
                std::ptr::copy_nonoverlapping(text.as_ptr(), output, len);
            }
        })
    }

    /// Borrows a Lisp string's UTF-8 contents.
    ///
    /// # Safety
    ///
    /// The word must be live. The borrow must end before string mutation or a GC safepoint.
    pub(super) unsafe fn string_slice(&self, word: Word) -> Option<&str> {
        // SAFETY: The caller bounds all borrows into the string's storage.
        if let Some(text) = unsafe { self.string_data_slice(word) } {
            return Some(text);
        }
        let info = self.array_info(word)?;
        if info.rank != 1 || info.element != ArrayElement::Character {
            return None;
        }
        let length = info.fill_pointer.unwrap_or(info.size);
        // SAFETY: The caller bounds the borrow across mutation and collection.
        unsafe { self.array_character_slice(word, 0, length) }
    }

    /// Borrows the given row-major characters, independently of any fill pointer.
    ///
    /// # Safety
    ///
    /// The word must be live. The borrow must end before string mutation or a GC safepoint.
    pub(super) unsafe fn array_character_slice(
        &self,
        word: Word,
        start: usize,
        length: usize,
    ) -> Option<&str> {
        let (data, offset) = self.array_storage(word, start, length)?;
        // SAFETY: The displacement chain keeps data live and the caller bounds the borrow.
        let text = unsafe { self.string_data_slice(data) }?;
        let count = self.string_data_count(data)?;
        if offset == 0 && length == count {
            return Some(text);
        }
        Some(&text[string_byte_range(text, count, offset..offset + length)])
    }

    unsafe fn string_data_slice(&self, word: Word) -> Option<&str> {
        let address = self.string_storage(word)?;
        // SAFETY: Any outstanding UTF-8 borrow implies the storage is already
        // packed. Its bytes then stay untouched until a Lisp mutation.
        let len = unsafe { pack_string(address) };
        let data = (address + size_of::<LispString>()).to_ptr::<u8>();
        Some(unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(data, len)) })
    }

    pub(crate) fn is_string(&self, word: Word) -> bool {
        self.array_info(word)
            .is_some_and(|info| info.rank == 1 && info.element == ArrayElement::Character)
    }
}
