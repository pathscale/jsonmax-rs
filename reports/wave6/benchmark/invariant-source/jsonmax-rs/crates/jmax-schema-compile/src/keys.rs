//! Deterministic object-key tables with bounded perfect-hash construction.
//!
//! Built spans and hash indices are local; lowering rebases them exactly once.
//! Validation belongs to the compile/load boundary. Lookup performs only the
//! chosen candidate probes, with checked indexing and exact byte comparison.

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
    compile_error(
        ErrorCode::BudgetExhausted,
        "key table resource limit exceeded",
    )
}

fn malformed(image: &SchemaImage, field: u32, detail: &'static str) -> Error {
    compile_error(ErrorCode::SchemaImageInvalid, detail).with_schema(image.header.schema_id, field)
}

fn count(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| budget())
}

fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<()> {
    count(values.len().checked_add(additional).ok_or_else(budget)?)?;
    values.try_reserve_exact(additional).map_err(|_| budget())
}

fn hash_capacity(key_count: u32) -> Result<u32> {
    key_count
        .checked_mul(2)
        .ok_or_else(budget)?
        .max(2)
        .checked_next_power_of_two()
        .ok_or_else(budget)
}

fn bounded_size(key_count: u32, byte_count: u32, slots: u32, limit: u32) -> Result<()> {
    // These are wire record sizes, never native struct sizes. This is a lower
    // bound on the final image; lowering enforces its complete encoded length.
    let size = key_count
        .checked_mul(24)
        .and_then(|size| size.checked_add(byte_count))
        .and_then(|size| {
            slots
                .checked_mul(4)
                .and_then(|bytes| size.checked_add(bytes))
        })
        .ok_or_else(budget)?;
    if size > limit { Err(budget()) } else { Ok(()) }
}

/// Absolute bound on the number of candidates examined by SWAR lookup.
pub const MAX_SWAR_KEYS: u32 = 8;

/// Build sorted, unique keys and choose a bounded-probe strategy.
///
/// Resource failures are reported rather than silently changing strategy.
/// Exhausting the single-level seed search uses a bounded two-level layout.
/// Hash construction failure is a resource error, never an unbounded SWAR scan.
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
            compile_error(
                ErrorCode::UnsupportedSchemaFeature,
                "field identifier does not fit u16",
            )
            .with_schema(0, spec.field_id.0)
        })?;
        if !fields.insert(field) {
            return Err(compile_error(
                ErrorCode::ConstraintViolation,
                "duplicate key field identifier",
            )
            .with_schema(0, spec.field_id.0));
        }
        let name_len =
            count(spec.name.len()).map_err(|error| error.with_schema(0, spec.field_id.0))?;
        byte_count = byte_count
            .checked_add(name_len)
            .ok_or_else(|| budget().with_schema(0, spec.field_id.0))?;
        if spec.required {
            required_total = required_total.checked_add(1).ok_or_else(budget)?;
            if required_total > u32::from(NO_REQUIRED_BIT) {
                return Err(compile_error(
                    ErrorCode::UnsupportedSchemaFeature,
                    "too many required object properties",
                )
                .with_schema(0, spec.field_id.0));
            }
        }
    }
    bounded_size(key_count, byte_count, 0, options.max_image_bytes)?;

    let mut sorted = Vec::new();
    reserve(&mut sorted, specs.len())?;
    sorted.extend(specs.iter());
    // Unstable sorting is allocation-free and deterministic for unique names.
    // Field IDs break ties to make duplicate-name diagnostics deterministic.
    sorted.sort_unstable_by(|a, b| {
        a.name
            .as_bytes()
            .cmp(b.name.as_bytes())
            .then(a.field_id.0.cmp(&b.field_id.0))
    });
    for pair in sorted.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(
                compile_error(ErrorCode::ConstraintViolation, "duplicate key name")
                    .with_schema(0, pair[1].field_id.0),
            );
        }
    }

    let mut entries = Vec::new();
    let mut bytes = Vec::new();
    reserve(&mut entries, specs.len())?;
    reserve(
        &mut bytes,
        usize::try_from(byte_count).map_err(|_| budget())?,
    )?;
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

    let strategy = if key_count <= u32::from(options.swar_threshold).min(MAX_SWAR_KEYS)
        || (options.max_hash_attempts == 0 && key_count <= MAX_SWAR_KEYS)
        || key_count == 0
    {
        KeyStrategy::Swar
    } else {
        build_hash(&sorted, key_count, byte_count, options)?
    };
    Ok(BuiltKeys {
        entries,
        bytes,
        strategy,
        required_count,
    })
}

fn build_hash(
    sorted: &[&KeySpec],
    key_count: u32,
    byte_count: u32,
    options: &CompilerOptions,
) -> Result<KeyStrategy> {
    if key_count > 64 {
        return build_bucketed(sorted, key_count, byte_count, options);
    }
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
    build_bucketed(sorted, key_count, byte_count, options)
}

/// FKS layout: a linear-size primary directory and quadratic secondary
/// buckets. A successful construction needs exactly two hashes and one exact
/// candidate comparison, even for adversarial unknown keys.
fn build_bucketed(
    sorted: &[&KeySpec],
    key_count: u32,
    byte_count: u32,
    options: &CompilerOptions,
) -> Result<KeyStrategy> {
    let capacity = hash_capacity(key_count)?;
    let len = usize::try_from(capacity).map_err(|_| budget())?;
    let directory_bytes = capacity.checked_mul(16).ok_or_else(budget)?;
    let limit = options
        .max_image_bytes
        .checked_sub(directory_bytes)
        .ok_or_else(budget)?;
    bounded_size(key_count, byte_count, 0, limit)?;
    let mut counts = Vec::new();
    reserve(&mut counts, len)?;
    counts.resize(len, 0u32);
    let max_slots = key_count.checked_mul(8).ok_or_else(budget)?;
    let mut chosen = None;
    for attempt in 0..options.max_hash_attempts {
        counts.fill(0);
        let seed = u64::from(attempt);
        for spec in sorted {
            let bucket = (key_hash(spec.name.as_bytes(), seed) & u64::from(capacity - 1)) as usize;
            counts[bucket] = counts[bucket].checked_add(1).ok_or_else(budget)?;
        }
        let mut total = 0u32;
        for &count in &counts {
            let Ok(size) = secondary_capacity(count) else {
                total = u32::MAX;
                break;
            };
            let Some(next) = total.checked_add(size) else {
                total = u32::MAX;
                break;
            };
            total = next;
            if total > max_slots {
                break;
            }
        }
        if total <= max_slots {
            chosen = Some((seed, total));
            break;
        }
    }
    let (seed, total) = chosen.ok_or_else(budget)?;
    bounded_size(key_count, byte_count, total, limit)?;
    let mut buckets = Vec::new();
    reserve(&mut buckets, len)?;
    let mut first_slot = 0u32;
    for &count in &counts {
        let slot_count = secondary_capacity(count)?;
        buckets.push(HashBucket {
            seed: 0,
            first_slot,
            slot_count,
        });
        first_slot = first_slot.checked_add(slot_count).ok_or_else(budget)?;
    }
    let mut slots = Vec::new();
    reserve(&mut slots, total as usize)?;
    slots.resize(total as usize, EMPTY_KEY_SLOT);
    // Group key indices once. Secondary seed search touches only its bucket.
    let mut starts = Vec::new();
    reserve(&mut starts, len)?;
    let mut next = 0u32;
    for &count in &counts {
        starts.push(next);
        next = next.checked_add(count).ok_or_else(budget)?;
    }
    counts.fill(0);
    let mut grouped = Vec::new();
    reserve(&mut grouped, sorted.len())?;
    grouped.resize(sorted.len(), 0u32);
    for (index, spec) in sorted.iter().enumerate() {
        let bucket = (key_hash(spec.name.as_bytes(), seed) & u64::from(capacity - 1)) as usize;
        let position = starts[bucket]
            .checked_add(counts[bucket])
            .ok_or_else(budget)?;
        grouped[position as usize] = count(index)?;
        counts[bucket] += 1;
    }
    for (number, bucket) in buckets.iter_mut().enumerate() {
        if bucket.slot_count == 0 {
            continue;
        }
        let start = bucket.first_slot as usize;
        let end = start
            .checked_add(bucket.slot_count as usize)
            .ok_or_else(budget)?;
        let group_start = starts[number] as usize;
        let group_end = group_start
            .checked_add(counts[number] as usize)
            .ok_or_else(budget)?;
        let mut found = false;
        for attempt in 0..options.max_hash_attempts {
            slots[start..end].fill(EMPTY_KEY_SLOT);
            let secondary_seed = u64::from(attempt);
            let mut collision = false;
            for &index in &grouped[group_start..group_end] {
                let key = sorted[index as usize].name.as_bytes();
                let local = key_hash(key, secondary_seed) & u64::from(bucket.slot_count - 1);
                let target = &mut slots[start + local as usize];
                if *target != EMPTY_KEY_SLOT {
                    collision = true;
                    break;
                }
                *target = index;
            }
            if !collision {
                bucket.seed = secondary_seed;
                found = true;
                break;
            }
        }
        if !found {
            return Err(budget());
        }
    }
    Ok(KeyStrategy::BucketedPerfectHash {
        seed,
        buckets,
        slots,
    })
}

fn secondary_capacity(count: u32) -> Result<u32> {
    if count == 0 {
        return Ok(0);
    }
    count
        .checked_mul(count)
        .and_then(u32::checked_next_power_of_two)
        .ok_or_else(budget)
}

fn table_entries<'a>(image: &'a SchemaImage, table: &KeyTable) -> Result<&'a [KeyEntry]> {
    let invalid = || malformed(image, 0, "key table range is outside the image");
    let end = table
        .first_key
        .checked_add(table.key_count)
        .ok_or_else(invalid)?;
    let start = usize::try_from(table.first_key).map_err(|_| invalid())?;
    let end = usize::try_from(end).map_err(|_| invalid())?;
    image.keys.get(start..end).ok_or_else(invalid)
}

fn entry_bytes<'a>(image: &'a SchemaImage, entry: &KeyEntry) -> Result<&'a [u8]> {
    entry.span.slice(&image.key_bytes).ok_or_else(|| {
        malformed(
            image,
            u32::from(entry.field_id),
            "key byte span is outside the image",
        )
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
        return Err(malformed(
            image,
            0,
            "key table exceeds the unique u16 field domain",
        ));
    }
    if matches!(table.strategy, KeyStrategy::Swar) && table.key_count > MAX_SWAR_KEYS {
        return Err(malformed(
            image,
            0,
            "SWAR table exceeds its constant probe bound",
        ));
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
            required_count = required_count
                .checked_add(1)
                .ok_or_else(|| invalid("required key count overflows u16"))?;
        }
        previous = Some(name);
    }

    if let KeyStrategy::PerfectHash { seed, slots } = &table.strategy {
        let invalid = |detail| malformed(image, 0, detail);
        if entries.is_empty() {
            return Err(invalid("empty key tables must use SWAR"));
        }
        let capacity =
            u32::try_from(slots.len()).map_err(|_| invalid("hash capacity does not fit u32"))?;
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
            let position =
                u64::try_from(position).map_err(|_| invalid("hash position does not fit u64"))?;
            if expected != position {
                return Err(malformed(
                    image,
                    u32::from(entry.field_id),
                    "key occupies the wrong hash slot",
                ));
            }
            occupied = occupied
                .checked_add(1)
                .ok_or_else(|| invalid("hash occupancy overflows u32"))?;
        }
        // A key can only occur at its single computed position. Thus correct
        // placements plus equal cardinality prove every entry occurs once,
        // with all remaining slots equal to EMPTY_KEY_SLOT.
        if occupied != table.key_count {
            return Err(invalid("perfect-hash slots do not cover every key"));
        }
    }
    if let KeyStrategy::BucketedPerfectHash {
        seed,
        buckets,
        slots,
    } = &table.strategy
    {
        if buckets.len() < 2 || !buckets.len().is_power_of_two() || entries.is_empty() {
            return Err(malformed(image, 0, "invalid primary hash directory"));
        }
        let mut end = 0u32;
        let mut occupied = 0u32;
        for (number, bucket) in buckets.iter().enumerate() {
            if bucket.first_slot != end
                || (bucket.slot_count != 0 && !bucket.slot_count.is_power_of_two())
            {
                return Err(malformed(image, 0, "invalid secondary hash range"));
            }
            end = end.checked_add(bucket.slot_count).ok_or_else(budget)?;
            let range = slots
                .get(bucket.first_slot as usize..end as usize)
                .ok_or_else(|| malformed(image, 0, "secondary range exceeds hash slots"))?;
            for (position, &index) in range.iter().enumerate() {
                if index == EMPTY_KEY_SLOT {
                    continue;
                }
                let entry = candidate(image, table, index)?;
                let key = entry_bytes(image, entry)?;
                if (key_hash(key, *seed) & (buckets.len() as u64 - 1)) != number as u64
                    || (key_hash(key, bucket.seed) & u64::from(bucket.slot_count - 1))
                        != position as u64
                {
                    return Err(malformed(
                        image,
                        u32::from(entry.field_id),
                        "key occupies the wrong secondary slot",
                    ));
                }
                occupied = occupied.checked_add(1).ok_or_else(budget)?;
            }
        }
        if end as usize != slots.len() || occupied != table.key_count {
            return Err(malformed(
                image,
                0,
                "secondary slots do not cover every key",
            ));
        }
    }
    Ok(())
}

fn matches_key(image: &SchemaImage, entry: &KeyEntry, key: &[u8], head: u64) -> Result<bool> {
    let name = entry_bytes(image, entry)?;
    Ok(name.len() == key.len() && entry.swar_head == head && name == key)
}

/// Find an exact decoded key, computing its SWAR head for callers without
/// token metadata. Direct consumers should pass KeyMeta's head instead.
pub fn lookup(
    program: &CompiledProgram,
    table_index: u32,
    key_bytes: &[u8],
) -> Result<Option<KeyMatch>> {
    lookup_with_swar(program, table_index, key_bytes, swar_head(key_bytes))
}

/// Compatibility entry point for standalone ABI image/table callers.
pub fn lookup_image<'a>(
    image: &'a SchemaImage,
    table: &KeyTable,
    key: &[u8],
) -> Result<Option<&'a KeyEntry>> {
    lookup_unchecked_table(image, table, key)
}

/// "Unchecked" concerns table structure only. This safe API never performs
/// unchecked memory access; callers needing an image witness must use validate.
pub fn lookup_unchecked_table<'a>(
    image: &'a SchemaImage,
    table: &KeyTable,
    key: &[u8],
) -> Result<Option<&'a KeyEntry>> {
    Ok(lookup_index(image, table, key)?.and_then(|index| image.keys.get(index as usize)))
}

/// Return the global key index directly, avoiding a second entry search.
pub fn lookup_index(image: &SchemaImage, table: &KeyTable, key: &[u8]) -> Result<Option<u32>> {
    lookup_index_with_head(image, table, key, swar_head(key))
}

fn lookup_index_with_head(
    image: &SchemaImage,
    table: &KeyTable,
    key: &[u8],
    head: u64,
) -> Result<Option<u32>> {
    let invalid = || malformed(image, 0, "invalid perfect-hash lookup index");
    let index = match &table.strategy {
        KeyStrategy::Swar => {
            // SWAR is always bounded, including when hash construction fails.
            if table.key_count > MAX_SWAR_KEYS {
                return Err(invalid());
            }
            for (local, entry) in table_entries(image, table)?.iter().enumerate() {
                if matches_key(image, entry, key, head)? {
                    return Ok(Some(
                        table
                            .first_key
                            .checked_add(count(local)?)
                            .ok_or_else(invalid)?,
                    ));
                }
            }
            return Ok(None);
        }
        KeyStrategy::PerfectHash { seed, slots } => {
            if slots.len() < 2 || !slots.len().is_power_of_two() {
                return Err(invalid());
            }
            let position = (key_hash(key, *seed) & (slots.len() as u64 - 1)) as usize;
            *slots.get(position).ok_or_else(invalid)?
        }
        KeyStrategy::BucketedPerfectHash {
            seed,
            buckets,
            slots,
        } => {
            if buckets.len() < 2 || !buckets.len().is_power_of_two() {
                return Err(invalid());
            }
            let primary = (key_hash(key, *seed) & (buckets.len() as u64 - 1)) as usize;
            let bucket = buckets.get(primary).ok_or_else(invalid)?;
            if bucket.slot_count == 0 {
                return Ok(None);
            }
            if !bucket.slot_count.is_power_of_two() {
                return Err(invalid());
            }
            let local = key_hash(key, bucket.seed) & u64::from(bucket.slot_count - 1);
            let position = bucket
                .first_slot
                .checked_add(u32::try_from(local).map_err(|_| invalid())?)
                .ok_or_else(invalid)?;
            *slots.get(position as usize).ok_or_else(invalid)?
        }
    };
    if index == EMPTY_KEY_SLOT {
        return Ok(None);
    }
    let entry = candidate(image, table, index)?;
    if matches_key(image, entry, key, head)? {
        Ok(Some(index))
    } else {
        Ok(None)
    }
}

/// A collision-checked match with both global and table-local seen-bit indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyMatch {
    pub index: u32,
    pub local_index: u32,
    pub entry: KeyEntry,
}

/// Consume the validated token's SWAR head without repacking key bytes.
/// Table structure is validated once when constructing `ValidatedProgram`;
/// checked indexing and exact candidate comparison remain safe for raw programs.
pub fn lookup_with_swar(
    program: &CompiledProgram,
    table_index: u32,
    key_bytes: &[u8],
    swar_head: u64,
) -> Result<Option<KeyMatch>> {
    let image = &program.image;
    let invalid = || malformed(image, 0, "invalid compiled key lookup");
    let table = program
        .key_tables
        .get(table_index as usize)
        .ok_or_else(invalid)?;
    let Some(index) = lookup_index_with_head(image, table, key_bytes, swar_head)? else {
        return Ok(None);
    };
    let entry = *image.keys.get(index as usize).ok_or_else(invalid)?;
    let local_index = index.checked_sub(table.first_key).ok_or_else(invalid)?;
    Ok(Some(KeyMatch {
        index,
        local_index,
        entry,
    }))
}
