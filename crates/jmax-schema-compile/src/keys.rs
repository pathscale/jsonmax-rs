//! Deterministic object-key tables with bounded perfect-hash construction.
//!
//! Built spans and hash indices are local; lowering rebases them exactly once.
//! The public lookup accepts an unwitnessed image, so it validates the complete
//! table before searching. Validation and lookup allocate no heap storage.

use super::*;

// Field identifiers cover the entire u16 domain, including u16::MAX. Only
// required-bit numbering reserves that value. A fixed bitmap avoids quadratic
// duplicate checks and allocation while inspecting an untrusted table.
struct FieldSet([u64; 1024]);

impl FieldSet {
    fn new() -> Self {
        Self([0; 1024])
    }

    fn insert(&mut self, field: u16) -> bool {
        let index = usize::from(field);
        let word = &mut self.0[index / 64];
        let bit = 1u64 << (index % 64);
        let fresh = *word & bit == 0;
        *word |= bit;
        fresh
    }
}

fn budget() -> Error {
    compile_error(ErrorCode::BudgetExhausted, "key table resource limit exceeded")
}

fn malformed(image: &SchemaImage, field: u32, detail: &'static str) -> Error {
    compile_error(ErrorCode::SchemaImageInvalid, detail)
        .with_schema(image.header.schema_id, field)
}

fn count(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| budget())
}

fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<()> {
    count(values.len().checked_add(additional).ok_or_else(budget)?)?;
    values.try_reserve_exact(additional).map_err(|_| budget())
}

fn hash_capacity(key_count: u32) -> Result<u32> {
    key_count.checked_mul(2).ok_or_else(budget)?
        .max(2).checked_next_power_of_two().ok_or_else(budget)
}

fn bounded_size(key_count: u32, byte_count: u32, slots: u32, limit: u32) -> Result<()> {
    // These are wire record sizes, never native struct sizes. This is a lower
    // bound on the final image; lowering enforces its complete encoded length.
    let size = key_count.checked_mul(24)
        .and_then(|size| size.checked_add(byte_count))
        .and_then(|size| slots.checked_mul(4).and_then(|bytes| size.checked_add(bytes)))
        .ok_or_else(budget)?;
    if size > limit { Err(budget()) } else { Ok(()) }
}

/// Build sorted, unique keys and choose the first collision-free hash seed.
///
/// Resource failures are reported rather than silently changing strategy.
/// Only exhaustion of the configured seed search falls back to SWAR.
pub fn build(specs: &[KeySpec], options: &CompilerOptions) -> Result<BuiltKeys> {
    let key_count = count(specs.len())?;
    if key_count > options.limits.max_keys {
        return Err(budget());
    }

    let mut fields = FieldSet::new();
    let mut byte_count = 0u32;
    let mut required_total = 0u32;
    for spec in specs {
        let field = u16::try_from(spec.field_id.0).map_err(|_| {
            compile_error(ErrorCode::UnsupportedSchemaFeature, "field identifier does not fit u16")
                .with_schema(0, spec.field_id.0)
        })?;
        if !fields.insert(field) {
            return Err(compile_error(ErrorCode::ConstraintViolation, "duplicate key field identifier")
                .with_schema(0, spec.field_id.0));
        }
        let name_len = count(spec.name.len())
            .map_err(|error| error.with_schema(0, spec.field_id.0))?;
        byte_count = byte_count.checked_add(name_len)
            .ok_or_else(|| budget().with_schema(0, spec.field_id.0))?;
        if spec.required {
            required_total = required_total.checked_add(1).ok_or_else(budget)?;
            if required_total > u32::from(NO_REQUIRED_BIT) {
                return Err(compile_error(ErrorCode::UnsupportedSchemaFeature,
                    "too many required object properties").with_schema(0, spec.field_id.0));
            }
        }
    }
    bounded_size(key_count, byte_count, 0, options.max_image_bytes)?;

    let mut sorted = Vec::new();
    reserve(&mut sorted, specs.len())?;
    sorted.extend(specs.iter());
    // Unstable sorting is allocation-free and deterministic for unique names.
    // Field IDs break ties to make duplicate-name diagnostics deterministic.
    sorted.sort_unstable_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes())
        .then(a.field_id.0.cmp(&b.field_id.0)));
    for pair in sorted.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(compile_error(ErrorCode::ConstraintViolation, "duplicate key name")
                .with_schema(0, pair[1].field_id.0));
        }
    }

    let mut entries = Vec::new();
    let mut bytes = Vec::new();
    reserve(&mut entries, specs.len())?;
    reserve(&mut bytes, usize::try_from(byte_count).map_err(|_| budget())?)?;
    let mut required_count = 0u16;
    for spec in &sorted {
        let name = spec.name.as_bytes();
        let required_bit = if spec.required {
            let bit = required_count;
            required_count = required_count.checked_add(1).ok_or_else(budget)?;
            bit
        } else {
            NO_REQUIRED_BIT
        };
        entries.push(KeyEntry {
            swar_head: swar_head(name),
            span: Span32::new(count(bytes.len())?, count(name.len())?),
            field_id: u16::try_from(spec.field_id.0).map_err(|_| budget())?,
            required_bit,
            target_state: spec.target.0,
        });
        bytes.extend_from_slice(name);
    }

    let strategy = if key_count <= u32::from(options.swar_threshold)
        || options.max_hash_attempts == 0
    {
        KeyStrategy::Swar
    } else {
        build_hash(&sorted, key_count, byte_count, options)?
    };
    Ok(BuiltKeys { entries, bytes, strategy, required_count })
}

fn build_hash(
    sorted: &[&KeySpec],
    key_count: u32,
    byte_count: u32,
    options: &CompilerOptions,
) -> Result<KeyStrategy> {
    let capacity = hash_capacity(key_count)?;
    let len = usize::try_from(capacity).map_err(|_| budget())?;
    let mask = u64::from(capacity - 1);
    let mut slots = Vec::new();
    reserve(&mut slots, len)?;
    slots.resize(len, EMPTY_KEY_SLOT);
    for attempt in 0..options.max_hash_attempts {
        let seed = u64::from(attempt);
        slots.fill(EMPTY_KEY_SLOT);
        let mut collision = false;
        for (local, spec) in sorted.iter().enumerate() {
            let index = usize::try_from(key_hash(spec.name.as_bytes(), seed) & mask)
                .map_err(|_| budget())?;
            let slot = slots.get_mut(index).ok_or_else(budget)?;
            if *slot != EMPTY_KEY_SLOT {
                collision = true;
                break;
            }
            *slot = count(local)?;
        }
        if !collision {
            bounded_size(key_count, byte_count, capacity, options.max_image_bytes)?;
            return Ok(KeyStrategy::PerfectHash { seed, slots });
        }
    }
    Ok(KeyStrategy::Swar)
}

fn table_entries<'a>(image: &'a SchemaImage, table: &KeyTable) -> Result<&'a [KeyEntry]> {
    let invalid = || malformed(image, 0, "key table range is outside the image");
    let end = table.first_key.checked_add(table.key_count).ok_or_else(invalid)?;
    let start = usize::try_from(table.first_key).map_err(|_| invalid())?;
    let end = usize::try_from(end).map_err(|_| invalid())?;
    image.keys.get(start..end).ok_or_else(invalid)
}

fn entry_bytes<'a>(image: &'a SchemaImage, entry: &KeyEntry) -> Result<&'a [u8]> {
    entry.span.slice(&image.key_bytes).ok_or_else(|| {
        malformed(image, u32::from(entry.field_id), "key byte span is outside the image")
    })
}

fn candidate<'a>(image: &'a SchemaImage, table: &KeyTable, index: u32) -> Result<&'a KeyEntry> {
    let invalid = || malformed(image, 0, "hash slot is outside its owning key table");
    let local = index.checked_sub(table.first_key).ok_or_else(invalid)?;
    if local >= table.key_count {
        return Err(invalid());
    }
    let index = usize::try_from(index).map_err(|_| invalid())?;
    image.keys.get(index).ok_or_else(invalid)
}

/// Validate this table's names, metadata and strategy without trusting a witness.
///
/// Header consistency, cross-table ownership and target-state bounds belong to
/// `image::validate`: `SchemaImage` does not contain the program's state vector.
/// Within this table every name must be UTF-8, strictly sorted and unique.
/// Hash validation accepts structurally valid power-of-two capacities; complete
/// image validation additionally verifies canonical construction with options.
pub fn validate(image: &SchemaImage, table: &KeyTable) -> Result<()> {
    let entries = table_entries(image, table)?;
    if entries.len() > 65_536 {
        return Err(malformed(image, 0, "key table exceeds the unique u16 field domain"));
    }
    let mut fields = FieldSet::new();
    let mut previous: Option<&[u8]> = None;
    let mut required_count = 0u16;
    for entry in entries {
        let name = entry_bytes(image, entry)?;
        let invalid = |detail| malformed(image, u32::from(entry.field_id), detail);
        if core::str::from_utf8(name).is_err() {
            return Err(invalid("key name is not UTF-8"));
        }
        if previous.is_some_and(|prior| prior >= name) {
            return Err(invalid("key names are not strictly sorted and unique"));
        }
        if entry.swar_head != swar_head(name) {
            return Err(invalid("key SWAR head does not match its name"));
        }
        if !fields.insert(entry.field_id) {
            return Err(invalid("duplicate key field identifier"));
        }
        if entry.required_bit != NO_REQUIRED_BIT {
            if entry.required_bit != required_count {
                return Err(invalid("required key bits are not dense in name order"));
            }
            required_count = required_count.checked_add(1)
                .ok_or_else(|| invalid("required key count overflows u16"))?;
        }
        previous = Some(name);
    }

    if let KeyStrategy::PerfectHash { seed, slots } = &table.strategy {
        let invalid = |detail| malformed(image, 0, detail);
        if entries.is_empty() {
            return Err(invalid("empty key tables must use SWAR"));
        }
        let capacity = u32::try_from(slots.len())
            .map_err(|_| invalid("hash capacity does not fit u32"))?;
        if capacity < 2 || !capacity.is_power_of_two() || slots.len() < entries.len() {
            return Err(invalid("invalid perfect-hash capacity"));
        }
        let mask = u64::from(capacity - 1);
        let mut occupied = 0u32;
        for (position, &index) in slots.iter().enumerate() {
            if index == EMPTY_KEY_SLOT {
                continue;
            }
            let entry = candidate(image, table, index)?;
            let expected = key_hash(entry_bytes(image, entry)?, *seed) & mask;
            let position = u64::try_from(position)
                .map_err(|_| invalid("hash position does not fit u64"))?;
            if expected != position {
                return Err(malformed(image, u32::from(entry.field_id), "key occupies the wrong hash slot"));
            }
            occupied = occupied.checked_add(1)
                .ok_or_else(|| invalid("hash occupancy overflows u32"))?;
        }
        // A key can only occur at its single computed position. Thus correct
        // placements plus equal cardinality prove every entry occurs once,
        // with all remaining slots equal to EMPTY_KEY_SLOT.
        if occupied != table.key_count {
            return Err(invalid("perfect-hash slots do not cover every key"));
        }
    }
    Ok(())
}

fn matches_key(image: &SchemaImage, entry: &KeyEntry, key: &[u8], head: u64) -> Result<bool> {
    let name = entry_bytes(image, entry)?;
    Ok(name.len() == key.len() && entry.swar_head == head && name == key)
}

/// Find an exact decoded key. Unknown-key hash collisions are ordinary misses.
///
/// Even an empty query or an early SWAR match cannot hide a malformed entry or
/// slot elsewhere in this table. Consequently the raw-image API has linear
/// validation cost in addition to its strategy's candidate search cost.
pub fn lookup<'a>(image: &'a SchemaImage, table: &KeyTable, key: &[u8]) -> Result<Option<&'a KeyEntry>> {
    validate(image, table)?;
    let head = swar_head(key);
    match &table.strategy {
        KeyStrategy::Swar => {
            for entry in table_entries(image, table)? {
                if matches_key(image, entry, key, head)? {
                    return Ok(Some(entry));
                }
            }
            Ok(None)
        }
        KeyStrategy::PerfectHash { seed, slots } => {
            let invalid = || malformed(image, 0, "invalid perfect-hash lookup index");
            let capacity = u32::try_from(slots.len()).map_err(|_| invalid())?;
            let mask = u64::from(capacity.checked_sub(1).ok_or_else(invalid)?);
            let position = usize::try_from(key_hash(key, *seed) & mask).map_err(|_| invalid())?;
            let index = *slots.get(position).ok_or_else(invalid)?;
            if index == EMPTY_KEY_SLOT {
                return Ok(None);
            }
            let entry = candidate(image, table, index)?;
            if matches_key(image, entry, key, head)? { Ok(Some(entry)) } else { Ok(None) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};

    fn spec(name: &str, field: u32, required: bool, target: u32) -> KeySpec {
        KeySpec { name: String::from(name), field_id: FieldId(field), required, target: StateId(target) }
    }

    fn options(threshold: u16, attempts: u32) -> CompilerOptions {
        CompilerOptions { swar_threshold: threshold, max_hash_attempts: attempts, ..CompilerOptions::default() }
    }

    fn image_table(built: BuiltKeys) -> (SchemaImage, KeyTable) {
        let key_count = u32::try_from(built.entries.len()).unwrap();
        let image = SchemaImage {
            header: SchemaImageHeader {
                magic: IMAGE_MAGIC, version: IMAGE_VERSION, flags: IMAGE_FLAGS,
                schema_id: 73, key_count, ..SchemaImageHeader::default()
            },
            insns: Vec::new(), keys: built.entries, key_bytes: built.bytes,
        };
        let table = KeyTable { first_key: 0, key_count, strategy: built.strategy };
        (image, table)
    }

    fn error_is(error: Error, code: ErrorCode, schema: u32, field: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, schema);
        assert_eq!(error.field_id, field);
        assert!(!error.detail.is_empty());
    }

    fn invalid_table(image: &SchemaImage, table: &KeyTable) {
        let error = validate(image, table).unwrap_err();
        assert_eq!(error.code, ErrorCode::SchemaImageInvalid);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.schema_id, 73);
        assert_eq!(error.byte_offset, 0);
        for key in [b"".as_slice(), b"a", b"bb", b"absent"] {
            assert_eq!(lookup(image, table, key).unwrap_err(), error);
        }
    }

    #[test]
    fn empty_tables_need_no_resources_and_always_use_swar() {
        let mut opts = options(0, u32::MAX);
        opts.limits.max_keys = 0;
        opts.max_image_bytes = 0;
        let built = build(&[], &opts).unwrap();
        assert!(built.entries.is_empty());
        assert!(built.bytes.is_empty());
        assert_eq!(built.required_count, 0);
        assert_eq!(built.strategy, KeyStrategy::Swar);
        let (image, table) = image_table(built);
        validate(&image, &table).unwrap();
        assert!(lookup(&image, &table, b"").unwrap().is_none());
        assert!(lookup(&image, &table, b"missing").unwrap().is_none());
    }

    #[test]
    fn sorted_utf8_names_have_exact_unpadded_spans_and_dense_required_bits() {
        let specs = [spec("é", 5, true, 99), spec("a\0", 2, false, 5),
            spec("", 0, true, u32::MAX), spec("数字", 6, false, 0),
            spec("a", 1, true, 4), spec("abcdefghX", 4, true, 7),
            spec("abcdefgh", 3, false, 6)];
        let built = build(&specs, &options(7, 20)).unwrap();
        assert_eq!(built.bytes, "aa\0abcdefghabcdefghXé数字".as_bytes());
        assert_eq!(built.required_count, 4);
        assert_eq!(built.strategy, KeyStrategy::Swar);
        let spans = [(0, 0), (0, 1), (1, 2), (3, 8), (11, 9), (20, 2), (22, 6)];
        let bits = [0, 1, NO_REQUIRED_BIT, NO_REQUIRED_BIT, 2, 3, NO_REQUIRED_BIT];
        let targets = [u32::MAX, 4, 5, 6, 7, 99, 0];
        for (i, entry) in built.entries.iter().enumerate() {
            assert_eq!(entry.span, Span32::new(spans[i].0, spans[i].1));
            assert_eq!(entry.field_id, u16::try_from(i).unwrap());
            assert_eq!(entry.required_bit, bits[i]);
            assert_eq!(entry.target_state, targets[i]);
        }
        assert_eq!(built.entries[0].swar_head, 0);
        assert_eq!(built.entries[1].swar_head, 0x61);
        assert_eq!(built.entries[2].swar_head, 0x61);
        assert_eq!(built.entries[3].swar_head, 0x6867_6665_6463_6261);
        assert_eq!(built.entries[4].swar_head, 0x6867_6665_6463_6261);
        assert_eq!(built.entries[5].swar_head, 0xa9c3);
        let (image, table) = image_table(built);
        validate(&image, &table).unwrap();
        for spec in &specs {
            let found = lookup(&image, &table, spec.name.as_bytes()).unwrap().unwrap();
            assert_eq!(u32::from(found.field_id), spec.field_id.0);
            assert_eq!(found.target_state, spec.target.0);
        }
        for missing in [b"\0".as_slice(), b"a\0\0", b"abcdefg", b"abcdefghY", b"abcdefghX\0", b"\xff"] {
            assert!(lookup(&image, &table, missing).unwrap().is_none());
        }
    }

    #[test]
    fn threshold_is_inclusive_and_seed_zero_has_exact_local_slots() {
        let specs = [spec("b", 42, false, 9), spec("a", 99, true, 8)];
        assert_eq!(build(&specs, &options(2, 4)).unwrap().strategy, KeyStrategy::Swar);
        let built = build(&specs, &options(1, 4)).unwrap();
        assert_eq!(built.strategy, KeyStrategy::PerfectHash {
            seed: 0, slots: vec![0, 1, EMPTY_KEY_SLOT, EMPTY_KEY_SLOT],
        });
        let (image, table) = image_table(built);
        validate(&image, &table).unwrap();
        assert_eq!(lookup(&image, &table, b"a").unwrap().unwrap().field_id, 99);
        assert_eq!(lookup(&image, &table, b"b").unwrap().unwrap().field_id, 42);
        // "e" collides with "a"; "c" hashes into an unoccupied slot.
        assert_eq!(key_hash(b"e", 0) & 3, key_hash(b"a", 0) & 3);
        assert!(lookup(&image, &table, b"e").unwrap().is_none());
        assert!(lookup(&image, &table, b"c").unwrap().is_none());
    }

    #[test]
    fn first_successful_seed_wins_and_attempt_limit_is_exclusive() {
        let specs = [spec("d", 1, false, 1), spec("aa", 0, true, 2)];
        assert_eq!(key_hash(b"aa", 0) & 3, 3);
        assert_eq!(key_hash(b"d", 0) & 3, 3);
        for attempts in [0, 1] {
            assert_eq!(build(&specs, &options(0, attempts)).unwrap().strategy, KeyStrategy::Swar);
        }
        for attempts in [2, 20] {
            let built = build(&specs, &options(0, attempts)).unwrap();
            assert_eq!(built.strategy, KeyStrategy::PerfectHash {
                seed: 1, slots: vec![1, EMPTY_KEY_SLOT, 0, EMPTY_KEY_SLOT],
            });
            let (image, table) = image_table(built);
            validate(&image, &table).unwrap();
            assert_eq!(lookup(&image, &table, b"aa").unwrap().unwrap().field_id, 0);
            assert_eq!(lookup(&image, &table, b"d").unwrap().unwrap().field_id, 1);
        }
    }

    #[test]
    fn inseparable_hash_keys_fall_back_and_do_not_charge_unused_slots() {
        let specs = [spec("e", 1, false, 0), spec("a", 0, false, 0)];
        let mut opts = options(0, 16);
        opts.max_image_bytes = 50;
        let built = build(&specs, &opts).unwrap();
        assert_eq!(built.strategy, KeyStrategy::Swar);
        let (image, table) = image_table(built);
        validate(&image, &table).unwrap();
        assert_eq!(lookup(&image, &table, b"e").unwrap().unwrap().field_id, 1);
    }

    #[test]
    fn input_permutations_produce_identical_tables() {
        let mut specs = vec![spec("z", 12, false, 9), spec("a", 1, true, 8),
            spec("é", 3, true, 7), spec("", 0, false, 6)];
        for opts in [options(8, 0), options(0, 16)] {
            let expected = build(&specs, &opts).unwrap();
            for _ in 0..specs.len() {
                specs.rotate_left(1);
                let actual = build(&specs, &opts).unwrap();
                assert_eq!(actual.entries, expected.entries);
                assert_eq!(actual.bytes, expected.bytes);
                assert_eq!(actual.strategy, expected.strategy);
                assert_eq!(actual.required_count, expected.required_count);
            }
            specs.reverse();
            assert_eq!(build(&specs, &opts).unwrap().entries, expected.entries);
        }
    }

    #[test]
    fn duplicates_and_unsupported_field_identifiers_have_context() {
        let opts = CompilerOptions::default();
        error_is(build(&[spec("same", 2, true, 0), spec("same", 9, false, 1)], &opts).unwrap_err(),
            ErrorCode::ConstraintViolation, 0, 9);
        error_is(build(&[spec("", 2, false, 0), spec("", 9, false, 0)], &opts).unwrap_err(),
            ErrorCode::ConstraintViolation, 0, 9);
        error_is(build(&[spec("a", 65535, false, 0), spec("b", 65535, true, 1)], &opts).unwrap_err(),
            ErrorCode::ConstraintViolation, 0, 65535);
        for field in [65536, u32::MAX] {
            error_is(build(&[spec("x", field, false, 0)], &opts).unwrap_err(),
                ErrorCode::UnsupportedSchemaFeature, 0, field);
        }
        let built = build(&[spec("x", 65535, true, u32::MAX)], &opts).unwrap();
        assert_eq!(built.entries[0].field_id, u16::MAX);
        assert_eq!(built.entries[0].required_bit, 0);
        let (image, table) = image_table(built);
        // No state vector exists here, so even this target must be preserved.
        validate(&image, &table).unwrap();
    }

    #[test]
    fn key_and_payload_limits_are_inclusive() {
        let specs = [spec("a", 0, false, 0)];
        let mut opts = options(1, 0);
        opts.limits.max_keys = 1;
        opts.max_image_bytes = 25;
        assert!(build(&specs, &opts).is_ok());
        opts.max_image_bytes = 24;
        error_is(build(&specs, &opts).unwrap_err(), ErrorCode::BudgetExhausted, 0, 0);
        opts.max_image_bytes = 25;
        opts.limits.max_keys = 0;
        error_is(build(&specs, &opts).unwrap_err(), ErrorCode::BudgetExhausted, 0, 0);
        opts.limits.max_keys = 1;
        opts.swar_threshold = 0;
        opts.max_hash_attempts = 1;
        opts.max_image_bytes = 33;
        assert!(matches!(build(&specs, &opts).unwrap().strategy, KeyStrategy::PerfectHash { .. }));
        opts.max_image_bytes = 32;
        error_is(build(&specs, &opts).unwrap_err(), ErrorCode::BudgetExhausted, 0, 0);
    }

    #[test]
    fn arithmetic_boundaries_and_reservation_failures_are_budget_errors() {
        for (keys, capacity) in [(0, 2), (1, 2), (2, 4), (3, 8), (4, 8),
            (5, 16), (65536, 131072), (1 << 30, 1 << 31)]
        {
            assert_eq!(hash_capacity(keys).unwrap(), capacity);
        }
        for keys in [(1 << 30) + 1, 1 << 31, u32::MAX] {
            error_is(hash_capacity(keys).unwrap_err(), ErrorCode::BudgetExhausted, 0, 0);
        }
        assert!(bounded_size(0, u32::MAX, 0, u32::MAX).is_ok());
        for (keys, bytes, slots) in [(1, u32::MAX, 0), (u32::MAX, 0, 0), (0, 0, u32::MAX)] {
            error_is(bounded_size(keys, bytes, slots, u32::MAX).unwrap_err(), ErrorCode::BudgetExhausted, 0, 0);
        }
        let mut values = Vec::<u64>::new();
        error_is(reserve(&mut values, usize::MAX).unwrap_err(), ErrorCode::BudgetExhausted, 0, 0);
        assert!(values.is_empty());
        #[cfg(target_pointer_width = "64")]
        error_is(count(u32::MAX as usize + 1).unwrap_err(), ErrorCode::BudgetExhausted, 0, 0);
    }

    #[test]
    fn required_sentinel_boundary_and_all_field_ids_are_supported() {
        let mut specs: Vec<KeySpec> = (0..65535)
            .map(|field| spec(&format!("p{field:05}"), field, true, 0)).collect();
        let opts = options(u16::MAX, 0);
        let built = build(&specs, &opts).unwrap();
        assert_eq!(built.required_count, u16::MAX);
        assert_eq!(built.entries.last().unwrap().required_bit, u16::MAX - 1);
        let (image, table) = image_table(built);
        validate(&image, &table).unwrap();
        specs.push(spec("p65535", 65535, true, 0));
        error_is(build(&specs, &opts).unwrap_err(), ErrorCode::UnsupportedSchemaFeature, 0, 65535);
        specs.last_mut().unwrap().required = false;
        let built = build(&specs, &opts).unwrap();
        assert_eq!(built.entries.len(), 65536);
        assert_eq!(built.required_count, u16::MAX);
        assert_eq!(built.entries.last().unwrap().required_bit, NO_REQUIRED_BIT);
        let (image, table) = image_table(built);
        validate(&image, &table).unwrap();
        assert_eq!(lookup(&image, &table, b"p65535").unwrap().unwrap().field_id, u16::MAX);
    }

    #[test]
    fn rebased_global_indices_and_byte_spans_work_for_each_strategy() {
        for opts in [options(8, 10), options(0, 10)] {
            let prefix = build(&[spec("prefix", 7, false, 0)], &opts).unwrap();
            let built = build(&[spec("b", 9, false, 2), spec("a", 8, true, 1)], &opts).unwrap();
            let (mut image, mut table) = image_table(built);
            for entry in &mut image.keys { entry.span.offset += 6; }
            image.keys.insert(0, prefix.entries[0]);
            drop(image.key_bytes.splice(0..0, prefix.bytes));
            image.header.key_count += 1;
            table.first_key = 1;
            if let KeyStrategy::PerfectHash { slots, .. } = &mut table.strategy {
                for index in slots { if *index != EMPTY_KEY_SLOT { *index += 1; } }
            }
            validate(&image, &table).unwrap();
            for (key, global) in [(b"a", 1), (b"b", 2)] {
                let found = lookup(&image, &table, key).unwrap().unwrap();
                assert!(core::ptr::eq(found, &image.keys[global]));
            }
            assert!(lookup(&image, &table, b"prefix").unwrap().is_none());
            let empty = KeyTable { first_key: 3, key_count: 0, strategy: KeyStrategy::Swar };
            validate(&image, &empty).unwrap();
            assert!(lookup(&image, &empty, b"").unwrap().is_none());
            if let KeyStrategy::PerfectHash { slots, .. } = &mut table.strategy {
                *slots.iter_mut().find(|slot| **slot != EMPTY_KEY_SLOT).unwrap() = 0;
                invalid_table(&image, &table);
            }
        }
    }

    #[test]
    fn invalid_table_ranges_and_empty_hash_tables_are_rejected() {
        let (image, table) = image_table(build(&[spec("a", 0, false, 0)], &options(1, 0)).unwrap());
        for (first_key, key_count) in [(2, 0), (1, 1), (0, 2), (u32::MAX, 1), (1, u32::MAX)] {
            invalid_table(&image, &KeyTable { first_key, key_count, strategy: KeyStrategy::Swar });
        }
        let mut empty = table;
        empty.key_count = 0;
        empty.strategy = KeyStrategy::PerfectHash { seed: 0, slots: vec![EMPTY_KEY_SLOT; 2] };
        invalid_table(&image, &empty);
    }

    #[test]
    fn malformed_names_heads_fields_and_required_bits_cannot_hide_behind_a_hit() {
        let (base, table) = image_table(build(&[spec("a", 0, true, 0), spec("bb", 1, true, 0)], &options(2, 0)).unwrap());
        for case in 0..10 {
            let mut image = base.clone();
            match case {
                0 => image.keys[1].span = Span32::new(u32::MAX, 1),
                1 => image.keys[1].span = Span32::new(4, 0),
                2 => image.keys[1].span = Span32::new(2, 2),
                3 => image.keys[1].swar_head ^= 1,
                4 => image.keys[1].field_id = 0,
                5 => image.keys[1].required_bit = 0,
                6 => image.keys[1].required_bit = 2,
                7 => image.keys[0].required_bit = NO_REQUIRED_BIT,
                8 => { image.key_bytes[1] = 0xff; image.keys[1].swar_head = swar_head(&image.key_bytes[1..]); }
                _ => { image.keys[1].span = image.keys[0].span; image.keys[1].swar_head = image.keys[0].swar_head; }
            }
            invalid_table(&image, &table);
        }
        let mut unsorted = base.clone();
        unsorted.keys.swap(0, 1);
        unsorted.keys[0].required_bit = 0;
        unsorted.keys[1].required_bit = 1;
        invalid_table(&unsorted, &table);
        let mut bad_head = base;
        bad_head.keys[1].swar_head = 0;
        error_is(validate(&bad_head, &table).unwrap_err(), ErrorCode::SchemaImageInvalid, 73, 1);
    }

    #[test]
    fn malformed_hash_slots_are_rejected_even_when_not_selected() {
        let (image, table) = image_table(build(&[spec("a", 0, false, 0), spec("b", 1, false, 0)], &options(0, 1)).unwrap());
        let bad_slots = [vec![], vec![0], vec![0, 1, EMPTY_KEY_SLOT],
            vec![0, EMPTY_KEY_SLOT, EMPTY_KEY_SLOT, EMPTY_KEY_SLOT],
            vec![0, 1, 0, EMPTY_KEY_SLOT], vec![0, 1, 2, EMPTY_KEY_SLOT],
            vec![0, 1, u32::MAX - 1, EMPTY_KEY_SLOT],
            vec![1, 0, EMPTY_KEY_SLOT, EMPTY_KEY_SLOT], vec![EMPTY_KEY_SLOT; 4]];
        for slots in bad_slots {
            let broken = KeyTable { strategy: KeyStrategy::PerfectHash { seed: 0, slots }, ..table.clone() };
            invalid_table(&image, &broken);
        }
        let mut changed_seed = table;
        if let KeyStrategy::PerfectHash { seed, .. } = &mut changed_seed.strategy { *seed = 1; }
        invalid_table(&image, &changed_seed);
    }

    #[test]
    fn structural_hash_validation_is_separate_from_canonical_options() {
        let (image, mut table) = image_table(build(&[spec("a", 0, false, 0),
            spec("b", 1, false, 0)], &options(0, 1)).unwrap());
        // Capacity two is structurally valid and both keys occupy their
        // computed slots. Canonical capacity is enforced by image validation.
        table.strategy = KeyStrategy::PerfectHash { seed: 0, slots: vec![0, 1] };
        validate(&image, &table).unwrap();
        assert_eq!(lookup(&image, &table, b"a").unwrap().unwrap().field_id, 0);
        assert_eq!(lookup(&image, &table, b"b").unwrap().unwrap().field_id, 1);
        assert!(lookup(&image, &table, b"e").unwrap().is_none());
    }

    #[test]
    fn phf_collision_with_identical_length_and_swar_head_needs_full_bytes() {
        let (image, table) = image_table(build(&[spec("abcdefghX", 11, false, 0)], &options(0, 1)).unwrap());
        assert_eq!(key_hash(b"abcdefghX", 0) & 1, key_hash(b"abcdefghZ", 0) & 1);
        assert_eq!(swar_head(b"abcdefghX"), swar_head(b"abcdefghZ"));
        assert_eq!(lookup(&image, &table, b"abcdefghX").unwrap().unwrap().field_id, 11);
        assert!(lookup(&image, &table, b"abcdefghZ").unwrap().is_none());
        assert!(lookup(&image, &table, b"abcdefgh").unwrap().is_none());
        assert!(lookup(&image, &table, b"abcdefghX\0").unwrap().is_none());
    }

    #[test]
    fn unicode_names_are_not_normalized_and_partial_scalars_are_invalid_images() {
        let specs = [spec("é", 1, false, 0), spec("e\u{301}", 0, true, 1),
            spec("😀", 2, false, 2)];
        let (mut image, table) = image_table(build(&specs, &options(3, 0)).unwrap());
        assert_eq!(lookup(&image, &table, "e\u{301}".as_bytes()).unwrap().unwrap().field_id, 0);
        assert_eq!(lookup(&image, &table, "é".as_bytes()).unwrap().unwrap().field_id, 1);
        assert_eq!(lookup(&image, &table, "😀".as_bytes()).unwrap().unwrap().field_id, 2);
        assert!(lookup(&image, &table, b"e").unwrap().is_none());
        let span = image.keys[2].span;
        image.keys[2].span = Span32::new(span.offset, span.len - 1);
        image.keys[2].swar_head = swar_head(image.keys[2].span.slice(&image.key_bytes).unwrap());
        invalid_table(&image, &table);
    }

    #[test]
    fn single_empty_name_can_use_phf_and_nul_is_distinct() {
        let (image, table) = image_table(build(&[spec("", 4, true, 0)], &options(0, 1)).unwrap());
        assert_eq!(table.strategy, KeyStrategy::PerfectHash { seed: 0, slots: vec![EMPTY_KEY_SLOT, 0] });
        assert_eq!(lookup(&image, &table, b"").unwrap().unwrap().field_id, 4);
        assert!(lookup(&image, &table, b"\0").unwrap().is_none());
        let (image, table) = image_table(build(&[spec("", 0, false, 0), spec("\0", 1, false, 0),
            spec("\0\0", 2, false, 0)], &options(3, 0)).unwrap());
        for (name, field) in [(b"".as_slice(), 0), (b"\0", 1), (b"\0\0", 2)] {
            assert_eq!(lookup(&image, &table, name).unwrap().unwrap().field_id, field);
        }
    }

    #[test]
    fn deterministic_corpus_matches_linear_exact_name_oracle() {
        let mut specs = Vec::new();
        for field in 0..96 {
            specs.push(spec(&format!("common-prefix-{field:03}-é"), field, field % 3 == 0, field + 100));
        }
        specs.reverse();
        for opts in [options(96, 0), options(0, 8)] {
            let (image, table) = image_table(build(&specs, &opts).unwrap());
            validate(&image, &table).unwrap();
            for number in 0..128 {
                let key = format!("common-prefix-{number:03}-é");
                let expected = specs.iter().find(|spec| spec.name.as_bytes() == key.as_bytes());
                let found = lookup(&image, &table, key.as_bytes()).unwrap();
                assert_eq!(found.map(|entry| u32::from(entry.field_id)), expected.map(|spec| spec.field_id.0));
                assert_eq!(found.map(|entry| entry.target_state), expected.map(|spec| spec.target.0));
            }
        }
    }
}
