use jmax_abi::syntax::{DuplicatePolicyOwner, SkipMode};
use jmax_syntax::{
    BlockCursor, ErrorCode, Limits, Payload, Result, ScanStep, SkipScratch, SyntaxState, feed,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;

thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn record_allocation() {
    let _ = TRACKING.try_with(|tracking| {
        if tracking.get() {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        }
    });
}

// SAFETY: every allocation and deallocation is forwarded unchanged to System.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: the caller supplied the layout required by GlobalAlloc.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: the caller supplied the layout required by GlobalAlloc.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation();
        // SAFETY: pointer, layout, and size satisfy the caller's contract.
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the allocation came from this same System allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn skip_siblings(input: &[u8], scratch: &mut SkipScratch, decoded: &mut Vec<u8>) -> Result<()> {
    let mut cursor = BlockCursor::with_scratch(
        input,
        Limits::strict_default(),
        None,
        DuplicatePolicyOwner::Syntax,
        scratch,
    )?;
    cursor.expect_byte(b'[', "array")?;
    cursor.skip_value(decoded, 1, SkipMode::StrictValidate)?;
    cursor.expect_byte(b',', "separator")?;
    cursor.skip_value(decoded, 1, SkipMode::StrictValidate)?;
    cursor.expect_byte(b']', "end")?;
    cursor.finish_eof()
}

#[test]
fn strict_skip_reuses_scratch_with_zero_warm_allocations_across_documents() -> Result<()> {
    let input = br#"[{"\u0061":[{"\u0062":"discard\n\uD83E\uDD80"},12]}, {"\u0061":null}]"#;
    let mut scratch = SkipScratch::default();
    let mut decoded = b"retained:".to_vec();
    skip_siblings(input, &mut scratch, &mut decoded)?;
    ALLOCATIONS.with(|count| count.set(0));
    TRACKING.with(|tracking| tracking.set(true));
    let result: Result<()> = (|| {
        for _ in 0..32 {
            skip_siblings(input, &mut scratch, &mut decoded)?;
        }
        Ok(())
    })();
    TRACKING.with(|tracking| tracking.set(false));
    result?;
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(decoded, b"retained:");

    // An error must return the capacities as well, rather than dropping them.
    {
        let mut cursor = BlockCursor::with_scratch(
            br#"{"\u0061":[{"\u0062":"bad\q"}]}"#,
            Limits::strict_default(),
            None,
            DuplicatePolicyOwner::Syntax,
            &mut scratch,
        )?;
        assert!(
            cursor
                .skip_value(&mut decoded, 0, SkipMode::StrictValidate)
                .is_err()
        );
    }
    ALLOCATIONS.with(|count| count.set(0));
    TRACKING.with(|tracking| tracking.set(true));
    let result = skip_siblings(input, &mut scratch, &mut decoded);
    TRACKING.with(|tracking| tracking.set(false));
    result?;
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(decoded, b"retained:");
    Ok(())
}

#[test]
fn complete_syntax_state_retains_skip_storage_across_resets() -> Result<()> {
    let payload = Payload::from_slice(br#"[{"\u0061":[{"\u0062":"discard\n"}]}]"#);
    let limits = Limits::strict_default();
    let mut state = SyntaxState::from_payload(payload.clone(), limits)?;
    let consume = |state: &mut SyntaxState| -> Result<()> {
        assert!(matches!(feed::next(state)?, ScanStep::Emit(_)));
        state.skip_open_container(SkipMode::StrictValidate)?;
        assert_eq!(feed::next(state)?, ScanStep::Finished);
        assert!(state.decoded_bytes().is_empty());
        Ok(())
    };
    consume(&mut state)?;
    ALLOCATIONS.with(|count| count.set(0));
    TRACKING.with(|tracking| tracking.set(true));
    let result: Result<()> = (|| {
        for _ in 0..32 {
            state.reset_with_payload(payload.clone(), limits, None)?;
            consume(&mut state)?;
        }
        Ok(())
    })();
    TRACKING.with(|tracking| tracking.set(false));
    result?;
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    Ok(())
}

#[test]
fn discard_never_allocates_decoded_storage_when_keys_are_not_needed() -> Result<()> {
    for input in [
        r#"["\n\uD83E\uDD80", "ordinary é"]"#.as_bytes(),
        br#"{"\u0061":"\n"}"#,
    ] {
        let mut limits = Limits::strict_default();
        limits.reject_duplicate_keys = false;
        let mut cursor = BlockCursor::new(input, limits, None, DuplicatePolicyOwner::Disabled)?;
        let mut decoded = Vec::new();
        cursor.skip_value(&mut decoded, 0, SkipMode::StrictValidate)?;
        assert!(decoded.is_empty());
        assert_eq!(decoded.capacity(), 0);
    }
    Ok(())
}

#[test]
fn closed_object_keys_are_reclaimed_without_losing_ancestor_identity_or_budget() -> Result<()> {
    let key = r"\u0061".repeat(96);
    let object = format!(r#"{{"{key}":"discard\n"}}"#);
    let input = format!("[{}]", vec![object; 128].join(","));
    let mut cursor = BlockCursor::new(
        input.as_bytes(),
        Limits::strict_default(),
        None,
        DuplicatePolicyOwner::Syntax,
    )?;
    let mut decoded = Vec::new();
    cursor.skip_value(&mut decoded, 0, SkipMode::StrictValidate)?;
    assert!(decoded.is_empty());
    assert!(
        decoded.capacity() <= 256,
        "closed siblings retained their cooked keys"
    );

    let mut limits = Limits::strict_default();
    limits.max_decoded_bytes = 1;
    let mut cursor = BlockCursor::new(
        br#"[{"\u0061":0},{"\u0062":0}]"#,
        limits,
        None,
        DuplicatePolicyOwner::Syntax,
    )?;
    assert_eq!(
        cursor
            .skip_value(&mut decoded, 0, SkipMode::StrictValidate)
            .unwrap_err()
            .code,
        ErrorCode::DecodedArenaOverflow,
    );
    assert!(decoded.is_empty());

    let mut cursor = BlockCursor::new(
        br#"{"\u0061":{"\u0062":0},"\u0061":1}"#,
        Limits::strict_default(),
        None,
        DuplicatePolicyOwner::Syntax,
    )?;
    assert_eq!(
        cursor
            .skip_value(&mut decoded, 0, SkipMode::StrictValidate)
            .unwrap_err()
            .code,
        ErrorCode::DuplicateObjectKey,
    );
    assert!(decoded.is_empty());
    Ok(())
}
