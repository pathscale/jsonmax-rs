//! Bounded JSON Pointer paths for JSON values and schema locations.

use alloc::string::String;
use core::convert::TryFrom;

use crate::{path_error, BoundedPath, ErrorCode, PathKind, PathLimits, PathSegment, Result};

impl BoundedPath {
    /// Creates an empty path with immutable kind and resource limits.
    pub fn new(kind: PathKind, limits: PathLimits) -> Self {
        Self {
            kind,
            limits,
            segments: alloc::vec::Vec::new(),
            encoded_len: 0,
        }
    }

    /// Appends a key segment, JSON Pointer-escaping `~` and `/`.
    ///
    /// All limits and allocations are checked before the path's logical state
    /// changes. Capacity may have grown if a later allocation fails.
    pub fn push_key(&mut self, key: &str) -> Result<()> {
        let next_count = self.segments.len().checked_add(1).ok_or_else(|| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path segment count overflow")
        })?;
        if next_count > usize::from(self.limits.max_segments) {
            return Err(path_error(
                self.kind,
                ErrorCode::BudgetExhausted,
                "path segment limit exceeded",
            ));
        }

        let mut segment_len = 1u32; // leading slash
        for byte in key.as_bytes() {
            let byte_len = if *byte == b'~' || *byte == b'/' { 2 } else { 1 };
            segment_len = segment_len.checked_add(byte_len).ok_or_else(|| {
                path_error(self.kind, ErrorCode::BudgetExhausted, "path length overflow")
            })?;
        }
        let next_len = self.encoded_len.checked_add(segment_len).ok_or_else(|| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path length overflow")
        })?;
        if next_len > self.limits.max_encoded_bytes {
            return Err(path_error(
                self.kind,
                ErrorCode::BudgetExhausted,
                "path byte limit exceeded",
            ));
        }

        let mut owned_key = String::new();
        owned_key.try_reserve(key.len()).map_err(|_| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path allocation failed")
        })?;
        owned_key.push_str(key);
        self.segments.try_reserve(1).map_err(|_| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path allocation failed")
        })?;

        self.segments.push(PathSegment::Key(owned_key));
        self.encoded_len = next_len;
        Ok(())
    }

    /// Appends an array index as an unpadded base-10 segment.
    pub fn push_index(&mut self, index: u32) -> Result<()> {
        let next_count = self.segments.len().checked_add(1).ok_or_else(|| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path segment count overflow")
        })?;
        if next_count > usize::from(self.limits.max_segments) {
            return Err(path_error(
                self.kind,
                ErrorCode::BudgetExhausted,
                "path segment limit exceeded",
            ));
        }

        let mut digits = 1u32;
        let mut remaining = index;
        while remaining >= 10 {
            remaining /= 10;
            digits += 1;
        }
        let segment_len = digits.checked_add(1).ok_or_else(|| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path length overflow")
        })?;
        let next_len = self.encoded_len.checked_add(segment_len).ok_or_else(|| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path length overflow")
        })?;
        if next_len > self.limits.max_encoded_bytes {
            return Err(path_error(
                self.kind,
                ErrorCode::BudgetExhausted,
                "path byte limit exceeded",
            ));
        }

        self.segments.try_reserve(1).map_err(|_| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path allocation failed")
        })?;
        self.segments.push(PathSegment::Index(index));
        self.encoded_len = next_len;
        Ok(())
    }

    /// Removes and returns the final segment, if any.
    pub fn pop(&mut self) -> Option<PathSegment> {
        let segment = self.segments.pop()?;
        let segment_len = match &segment {
            PathSegment::Key(key) => {
                let mut length = 1u32;
                for byte in key.as_bytes() {
                    length += if *byte == b'~' || *byte == b'/' { 2 } else { 1 };
                }
                length
            }
            PathSegment::Index(index) => {
                let mut digits = 1u32;
                let mut remaining = *index;
                while remaining >= 10 {
                    remaining /= 10;
                    digits += 1;
                }
                digits + 1
            }
        };
        self.encoded_len -= segment_len;
        Some(segment)
    }

    /// Removes every segment while retaining the vector's allocated capacity.
    pub fn clear(&mut self) {
        self.segments.clear();
        self.encoded_len = 0;
    }

    /// Returns the JSON Pointer spelling, or a bounded allocation error.
    pub fn to_pointer(&self) -> Result<String> {
        if self.segments.len() > usize::from(self.limits.max_segments) {
            return Err(path_error(
                self.kind,
                ErrorCode::BudgetExhausted,
                "path segment limit exceeded",
            ));
        }
        let mut verified_len = 0u32;
        for segment in &self.segments {
            let segment_len = match segment {
                PathSegment::Key(key) => {
                    let mut length = 1u32;
                    for byte in key.as_bytes() {
                        let byte_len = if *byte == b'~' || *byte == b'/' { 2 } else { 1 };
                        length = length.checked_add(byte_len).ok_or_else(|| {
                            path_error(self.kind, ErrorCode::BudgetExhausted, "path length overflow")
                        })?;
                    }
                    length
                }
                PathSegment::Index(index) => {
                    let mut digits = 1u32;
                    let mut remaining = *index;
                    while remaining >= 10 {
                        remaining /= 10;
                        digits += 1;
                    }
                    digits + 1
                }
            };
            verified_len = verified_len.checked_add(segment_len).ok_or_else(|| {
                path_error(self.kind, ErrorCode::BudgetExhausted, "path length overflow")
            })?;
        }
        if verified_len != self.encoded_len {
            return Err(path_error(
                self.kind,
                ErrorCode::BudgetExhausted,
                "path length invariant violated",
            ));
        }
        if verified_len > self.limits.max_encoded_bytes {
            return Err(path_error(
                self.kind,
                ErrorCode::BudgetExhausted,
                "path byte limit exceeded",
            ));
        }

        let capacity = usize::try_from(self.encoded_len).map_err(|_| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path allocation failed")
        })?;
        let mut pointer = String::new();
        pointer.try_reserve(capacity).map_err(|_| {
            path_error(self.kind, ErrorCode::BudgetExhausted, "path allocation failed")
        })?;

        for segment in &self.segments {
            pointer.push('/');
            match segment {
                PathSegment::Key(key) => {
                    for character in key.chars() {
                        match character {
                            '~' => pointer.push_str("~0"),
                            '/' => pointer.push_str("~1"),
                            other => pointer.push(other),
                        }
                    }
                }
                PathSegment::Index(index) => append_decimal(&mut pointer, *index),
            }
        }
        Ok(pointer)
    }
}

fn append_decimal(output: &mut String, value: u32) {
    let mut digits = [0u8; 10];
    let mut position = digits.len();
    let mut remaining = value;
    loop {
        position -= 1;
        digits[position] = match remaining % 10 {
            0 => b'0',
            1 => b'1',
            2 => b'2',
            3 => b'3',
            4 => b'4',
            5 => b'5',
            6 => b'6',
            7 => b'7',
            8 => b'8',
            _ => b'9',
        };
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    for byte in &digits[position..] {
        output.push(char::from(*byte));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorSource;
    use alloc::string::ToString;

    fn limits(max_segments: u16, max_encoded_bytes: u32) -> PathLimits {
        PathLimits { max_segments, max_encoded_bytes }
    }

    fn assert_budget_error(result: Result<()>, kind: PathKind) {
        let error = result.expect_err("operation should exceed its path budget");
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert_eq!(
            error.source,
            match kind {
                PathKind::Json => ErrorSource::Syntax,
                PathKind::Schema => ErrorSource::SchemaCompile,
            },
        );
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn root_empty_key_and_pointer_escapes_have_exact_byte_lengths() {
        let mut path = BoundedPath::new(PathKind::Json, limits(4, 32));
        assert_eq!(path.to_pointer().unwrap(), "");
        assert_eq!(path.encoded_len(), 0);
        assert!(path.is_empty());

        path.push_key("").unwrap();
        assert_eq!(path.to_pointer().unwrap(), "/");
        assert_eq!(path.encoded_len(), 1);
        path.push_key("a~/b").unwrap();
        assert_eq!(path.to_pointer().unwrap(), "//a~0~1b");
        assert_eq!(path.encoded_len(), 8);
    }

    #[test]
    fn utf8_is_preserved_and_indexes_are_unpadded_decimal() {
        let mut path = BoundedPath::new(PathKind::Schema, PathLimits::default());
        path.push_key("雪/é").unwrap();
        path.push_index(0).unwrap();
        path.push_index(42).unwrap();
        path.push_index(u32::MAX).unwrap();
        assert_eq!(path.to_pointer().unwrap(), "/雪~1é/0/42/4294967295");
        assert_eq!(path.encoded_len(), 24);
        assert_eq!(path.kind(), PathKind::Schema);
        assert_eq!(path.segments()[1], PathSegment::Index(0));
        assert_eq!(path.segments()[0], PathSegment::Key("雪/é".to_string()));
    }

    #[test]
    fn equal_pointer_spellings_keep_key_and_index_distinct() {
        let mut key_path = BoundedPath::new(PathKind::Json, limits(1, 2));
        let mut index_path = BoundedPath::new(PathKind::Json, limits(1, 2));
        key_path.push_key("0").unwrap();
        index_path.push_index(0).unwrap();
        assert_eq!(key_path.to_pointer().unwrap(), index_path.to_pointer().unwrap());
        assert_eq!(key_path.segments(), &[PathSegment::Key("0".to_string())]);
        assert_eq!(index_path.segments(), &[PathSegment::Index(0)]);
    }

    #[test]
    fn zero_limits_allow_only_the_empty_root() {
        let mut path = BoundedPath::new(PathKind::Json, limits(0, 0));
        assert_eq!(path.to_pointer().unwrap(), "");
        assert_budget_error(path.push_key(""), PathKind::Json);
        assert!(path.is_empty());
        assert_eq!(path.encoded_len(), 0);

        let mut byte_limited = BoundedPath::new(PathKind::Schema, limits(2, 0));
        assert_budget_error(byte_limited.push_index(0), PathKind::Schema);
        assert!(byte_limited.is_empty());
    }

    #[test]
    fn exact_segment_and_encoded_byte_boundaries_are_inclusive() {
        let mut path = BoundedPath::new(PathKind::Json, limits(2, 5));
        path.push_key("a").unwrap(); // /a = 2 bytes
        path.push_key("~").unwrap(); // /~0 = 3 more bytes
        assert_eq!(path.encoded_len(), 5);
        assert_eq!(path.to_pointer().unwrap(), "/a/~0");
        assert_budget_error(path.push_index(0), PathKind::Json);
        assert_eq!(path.encoded_len(), 5);
        assert_eq!(path.segments().len(), 2);

        let mut byte_boundary = BoundedPath::new(PathKind::Json, limits(1, 2));
        byte_boundary.push_key("x").unwrap();
        assert_eq!(byte_boundary.encoded_len(), 2);
        assert_budget_error(byte_boundary.push_key(""), PathKind::Json);
        assert_eq!(byte_boundary.to_pointer().unwrap(), "/x");
    }

    #[test]
    fn failed_mutations_preserve_existing_logical_state() {
        let mut path = BoundedPath::new(PathKind::Schema, limits(1, 3));
        path.push_key("ok").unwrap();
        let before = path.clone();
        assert_budget_error(path.push_index(7), PathKind::Schema);
        assert_eq!(path, before);
        assert_budget_error(path.push_key("too long"), PathKind::Schema);
        assert_eq!(path, before);
        assert_eq!(path.kind(), PathKind::Schema);
        assert_eq!(path.limits(), limits(1, 3));
    }

    #[test]
    fn arithmetic_overflow_returns_budget_error_without_mutation() {
        let mut path = BoundedPath::new(PathKind::Json, limits(u16::MAX, u32::MAX));
        path.encoded_len = u32::MAX;
        let before = path.clone();
        assert_budget_error(path.push_key("x"), PathKind::Json);
        assert_eq!(path, before);
        assert_budget_error(path.push_index(0), PathKind::Json);
        assert_eq!(path, before);
        let pointer_error = path.to_pointer().expect_err("inconsistent state must be rejected");
        assert_eq!(pointer_error.code, ErrorCode::BudgetExhausted);
        assert_eq!(pointer_error.source, ErrorSource::Syntax);
        assert_eq!(path, before);
    }

    #[test]
    fn pop_and_clear_restore_root_and_handle_empty_paths() {
        let mut path = BoundedPath::new(PathKind::Json, limits(4, 64));
        assert_eq!(path.pop(), None);
        path.push_key("a~/").unwrap();
        path.push_index(12345).unwrap();
        let full = path.encoded_len();
        assert_eq!(path.pop(), Some(PathSegment::Index(12345)));
        assert_eq!(path.encoded_len(), 6);
        assert_eq!(path.to_pointer().unwrap(), "/a~0~1");
        assert!(path.encoded_len() < full);
        assert_eq!(path.pop(), Some(PathSegment::Key("a~/".to_string())));
        assert!(path.is_empty());
        assert_eq!(path.encoded_len(), 0);
        path.push_index(u32::MAX).unwrap();
        path.clear();
        assert!(path.is_empty());
        assert_eq!(path.encoded_len(), 0);
        assert_eq!(path.to_pointer().unwrap(), "");
    }

    #[test]
    fn json_and_schema_paths_share_encoding_but_keep_their_kind() {
        let mut json = BoundedPath::new(PathKind::Json, limits(1, 8));
        let mut schema = BoundedPath::new(PathKind::Schema, limits(1, 8));
        json.push_key("field/name").unwrap();
        schema.push_key("field/name").unwrap();
        assert_eq!(json.to_pointer().unwrap(), schema.to_pointer().unwrap());
        assert_eq!(json.kind(), PathKind::Json);
        assert_eq!(schema.kind(), PathKind::Schema);
    }

    #[test]
    fn maximum_index_and_slash_escape_boundary_are_exact() {
        let mut path = BoundedPath::new(PathKind::Json, limits(1, 11));
        path.push_index(u32::MAX).unwrap();
        assert_eq!(path.to_pointer().unwrap(), "/4294967295");
        assert_eq!(path.encoded_len(), 11);
        assert_budget_error(path.push_key(""), PathKind::Json);

        let mut escaped = BoundedPath::new(PathKind::Json, limits(1, 3));
        escaped.push_key("/").unwrap();
        assert_eq!(escaped.to_pointer().unwrap(), "/~1");
        assert_eq!(escaped.encoded_len(), 3);
    }
}
