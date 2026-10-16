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
