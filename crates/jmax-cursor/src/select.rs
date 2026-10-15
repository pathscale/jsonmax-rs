//! Shared selector execution over one forward syntax event stream.
//!
//! Each JSON value has at most one active selector node: sibling segments are
//! unique, even when several graph paths share a target. Container frames keep
//! that node until its closing event so prefix and descendant terminals can
//! both be captured without a node tape, recursion, or reparsing.

use super::*;

#[derive(Debug)]
struct SelectionFrame {
    node: Option<u32>,
    start: u32,
    state: ContainerState,
}

#[derive(Debug)]
enum ContainerState {
    // The outer option distinguishes a missing key from a key with no edge.
    Object { pending: Option<Option<u32>> },
    Array { next_index: u32 },
}

/// Extracts all slots in a shared plan from one owned payload.
///
/// Keys match their decoded UTF-8 bytes exactly. Indices count immediate array
/// children, including children of the wrong type for subsequent segments.
/// Missing paths remain `None`; null is a present value. A slot is written only
/// upon its first complete match, including when duplicate keys are permitted.
///
/// Strict mode validates through EOF. Query mode stops as soon as every slot
/// has a complete value, or validates through EOF to establish missing paths.
/// The returned owner always retains the entire supplied payload, including an
/// unconsumed suffix. Only actual syntax completion sets `fully_validated`.
/// Allocation and syntax errors return no partial result.
pub fn extract(
    payload: Payload,
    plan: &SelectorPlan,
    mode: SkipMode,
    limits: Limits,
) -> Result<SelectedValues> {
    let mut cursor = Cursor::new(limits, mode);
    walk::push(&mut cursor, payload.as_slice())?;
    walk::end_input(&mut cursor)?;

    // Public construction already validates the graph. Keep impossible internal
    // state recoverable instead of indexing blindly or silently losing slots.
    if plan.nodes.is_empty() || plan.nodes.len() > MAX_SELECTOR_NODES
        || plan.slot_count > MAX_SELECTION_SLOTS
    {
        return Err(internal(0, "selector graph has invalid dimensions"));
    }
    let count = usize::try_from(plan.slot_count).map_err(|_| {
        cursor_error(ErrorCode::BudgetExhausted, 0, "selector slots exceed addressable storage")
    })?;
    let mut slots = Vec::new();
    reserve_exact(&mut slots, count, 0, "selection slot allocation failed")?;
    slots.resize(count, None);
    let mut remaining = count;
    let mut frames: Vec<SelectionFrame> = Vec::new();
    let mut root_started = false;

    loop {
        if mode == SkipMode::Query && remaining == 0 {
            break;
        }
        let event = match walk::next(&mut cursor)? {
            ScanStep::Emit(event) => event,
            ScanStep::Finished => {
                if !root_started || !frames.is_empty() || !cursor.syntax.is_finished() {
                    return Err(internal(
                        cursor.state().byte_offset,
                        "syntax completed with unfinished selection traversal",
                    ));
                }
                break;
            }
            ScanStep::NeedMore => {
                return Err(cursor_error(
                    ErrorCode::UnexpectedEof,
                    cursor.state().byte_offset,
                    "selection requires a complete input value",
                ));
            }
        };
        let at = cursor.state().byte_offset;
        match event {
            Event::Key { str_ref } => {
                let key = cursor.resolve_str(str_ref).ok_or_else(|| {
                    internal(at, "object key has an invalid decoded reference")
                })?;
                let frame = frames.last_mut().ok_or_else(|| {
                    internal(at, "object key has no containing object")
                })?;
                match &mut frame.state {
                    ContainerState::Object { pending } if pending.is_none() => {
                        *pending = Some(matching_child(plan, frame.node, at, |segment| {
                            matches!(segment, Segment::Key(expected) if expected.as_bytes() == key.as_bytes())
                        })?);
                    }
                    _ => return Err(internal(at, "object key is out of selection order")),
                }
            }
            Event::ObjectEnd { offset } | Event::ArrayEnd { offset } => {
                let frame = frames.pop().ok_or_else(|| {
                    internal(at, "container end has no selection frame")
                })?;
                let kind = match (&frame.state, event) {
                    (ContainerState::Object { pending: None }, Event::ObjectEnd { .. }) => {
                        ValueKind::Object
                    }
                    (ContainerState::Array { .. }, Event::ArrayEnd { .. }) => ValueKind::Array,
                    _ => return Err(internal(at, "container end disagrees with selection frame")),
                };
                let span = container_span(frame.start, offset, at)?;
                if span.slice(cursor.raw_bytes()).is_none() {
                    return Err(internal(at, "selected container exceeds retained input"));
                }
                capture(
                    plan, frame.node, SelectedValue { kind, span, decoded: None },
                    &mut slots, &mut remaining, at,
                )?;
            }
            Event::ObjectStart { offset } | Event::ArrayStart { offset } => {
                let node = value_node(plan, &mut frames, &mut root_started, at)?;
                let state = match event {
                    Event::ObjectStart { .. } => ContainerState::Object { pending: None },
                    _ => ContainerState::Array { next_index: 0 },
                };
                frames.try_reserve(1).map_err(|_| {
                    cursor_error(ErrorCode::BudgetExhausted, at, "selection frame allocation failed")
                })?;
                frames.push(SelectionFrame { node, start: offset, state });
            }
            Event::Scalar { kind, span, decoded } => {
                let node = value_node(plan, &mut frames, &mut root_started, at)?;
                if span.is_empty() || span.slice(cursor.raw_bytes()).is_none() {
                    return Err(internal(at, "selected scalar has an invalid raw span"));
                }
                let kind = match kind {
                    ScalarKind::String => ValueKind::String,
                    ScalarKind::Number => ValueKind::Number,
                    ScalarKind::True | ScalarKind::False => ValueKind::Bool,
                    ScalarKind::Null => ValueKind::Null,
                };
                if kind == ValueKind::String
                    && decoded.and_then(|reference| cursor.resolve_str(reference)).is_none()
                {
                    return Err(internal(at, "selected string has an invalid decoded reference"));
                }
                capture(
                    plan, node, SelectedValue { kind, span, decoded },
                    &mut slots, &mut remaining, at,
                )?;
            }
        }
    }

    // Retain the original payload owner. Syntax references use the same absolute
    // offsets in its byte-for-byte input copy. Copy the whole cooked arena so
    // references remain unchanged even when earlier keys used arena storage.
    let mut decoded = Vec::new();
    reserve_exact(
        &mut decoded,
        cursor.decoded_bytes().len(),
        cursor.state().byte_offset,
        "selection decoded arena allocation failed",
    )?;
    decoded.extend_from_slice(cursor.decoded_bytes());
    Ok(SelectedValues {
        payload,
        decoded,
        slots,
        fully_validated: cursor.syntax.is_finished(),
    })
}

fn reserve_exact<T>(
    storage: &mut Vec<T>,
    additional: usize,
    at: u32,
    detail: &'static str,
) -> Result<()> {
    storage.try_reserve_exact(additional).map_err(|_| {
        cursor_error(ErrorCode::BudgetExhausted, at, detail)
    })
}

fn selector_node(plan: &SelectorPlan, index: u32, at: u32) -> Result<&SelectorNode> {
    let index = usize::try_from(index).map_err(|_| {
        internal(at, "selector node exceeds addressable storage")
    })?;
    plan.nodes.get(index).ok_or_else(|| internal(at, "selector node is out of bounds"))
}

fn matching_child(
    plan: &SelectorPlan,
    parent: Option<u32>,
    at: u32,
    matches_segment: impl Fn(&Segment) -> bool,
) -> Result<Option<u32>> {
    let parent = match parent {
        Some(parent) => parent,
        None => return Ok(None),
    };
    let node = selector_node(plan, parent, at)?;
    for edge in &node.edges {
        if matches_segment(&edge.segment) {
            if edge.target <= parent {
                return Err(internal(at, "selector edge does not advance through the graph"));
            }
            selector_node(plan, edge.target, at)?;
            return Ok(Some(edge.target));
        }
    }
    Ok(None)
}

fn value_node(
    plan: &SelectorPlan,
    frames: &mut [SelectionFrame],
    root_started: &mut bool,
    at: u32,
) -> Result<Option<u32>> {
    let frame = match frames.last_mut() {
        Some(frame) => frame,
        None => {
            if *root_started {
                return Err(internal(at, "selection encountered a second root value"));
            }
            selector_node(plan, 0, at)?;
            *root_started = true;
            return Ok(Some(0));
        }
    };
    match &mut frame.state {
        ContainerState::Object { pending } => pending.take().ok_or_else(|| {
            internal(at, "object value has no preceding selection key")
        }),
        ContainerState::Array { next_index } => {
            let index = *next_index;
            *next_index = index.checked_add(1).ok_or_else(|| {
                internal(at, "selection array item count overflow")
            })?;
            matching_child(plan, frame.node, at, |segment| {
                matches!(segment, Segment::Index(expected) if *expected == index)
            })
        }
    }
}

fn capture(
    plan: &SelectorPlan,
    node: Option<u32>,
    value: SelectedValue,
    slots: &mut [Option<SelectedValue>],
    remaining: &mut usize,
    at: u32,
) -> Result<()> {
    if let Some(node) = node {
        for &slot in &selector_node(plan, node, at)?.slots {
            let index = usize::try_from(slot).map_err(|_| {
                internal(at, "selection slot exceeds addressable storage")
            })?;
            let destination = slots.get_mut(index).ok_or_else(|| {
                internal(at, "selection terminal slot is out of bounds")
            })?;
            if destination.is_none() {
                *remaining = remaining.checked_sub(1).ok_or_else(|| {
                    internal(at, "selection remaining slot count underflow")
                })?;
                *destination = Some(value);
            }
        }
    }
    Ok(())
}

fn container_span(start: u32, close: u32, at: u32) -> Result<Span32> {
    let end = close.checked_add(1).ok_or_else(|| {
        cursor_error(ErrorCode::PayloadTooLarge, at, "selected container end exceeds span capacity")
    })?;
    let len = end.checked_sub(start).filter(|&len| len >= 2).ok_or_else(|| {
        internal(at, "selected container delimiters are out of order")
    })?;
    Ok(Span32::new(start, len))
}

const fn internal(at: u32, detail: &'static str) -> Error {
    cursor_error(ErrorCode::InternalError, at, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn key(text: &str, target: u32) -> SelectorEdge {
        SelectorEdge { segment: Segment::Key(String::from(text)), target }
    }

    fn index(value: u32, target: u32) -> SelectorEdge {
        SelectorEdge { segment: Segment::Index(value), target }
    }

    fn node(edges: Vec<SelectorEdge>, slots: &[u32]) -> SelectorNode {
        SelectorNode { edges, slots: slots.to_vec() }
    }

    fn root_plan() -> SelectorPlan {
        SelectorPlan::new(vec![node(vec![], &[0])], 1).unwrap()
    }

    fn missing_plan() -> SelectorPlan {
        SelectorPlan::new(vec![node(vec![key("missing", 1)], &[]), node(vec![], &[0])], 1).unwrap()
    }

    fn run(bytes: &[u8], plan: &SelectorPlan, mode: SkipMode) -> SelectedValues {
        extract(Payload::from_slice(bytes), plan, mode, Limits::strict_default()).unwrap()
    }

    fn assert_raw(result: &SelectedValues, slot: u32, expected: &[u8], kind: ValueKind) {
        assert_eq!(result.raw(slot), Some(expected));
        assert_eq!(result.slots()[slot as usize].unwrap().kind, kind);
    }

    fn syntax_error_for(bytes: &[u8], limits: Limits) -> Error {
        let mut cursor = match walk::from_payload(Payload::from_slice(bytes), limits, SkipMode::Strict) {
            Ok(cursor) => cursor,
            Err(error) => return error,
        };
        for _ in 0..=bytes.len() + 1 {
            match walk::next(&mut cursor) {
                Err(error) => return error,
                Ok(ScanStep::Emit(_)) => {}
                _ => panic!("invalid test input did not fail syntax traversal"),
            }
        }
        panic!("syntax traversal failed to make progress")
    }

    #[test]
    fn root_terminals_cover_all_kinds_and_exclude_surrounding_whitespace() {
        let cases: &[(&[u8], ValueKind)] = &[
            (b"{}", ValueKind::Object), (b"[]", ValueKind::Array),
            (br#""plain""#, ValueKind::String), (br#""""#, ValueKind::String),
            (b"-12.50e+2", ValueKind::Number), (b"true", ValueKind::Bool),
            (b"false", ValueKind::Bool), (b"null", ValueKind::Null),
            (br#"{"a":[{},[],null]}"#, ValueKind::Object),
        ];
        for &(token, kind) in cases {
            let mut bytes = b" \t\r\n".to_vec();
            bytes.extend_from_slice(token);
            bytes.extend_from_slice(b" \n");
            for mode in [SkipMode::Strict, SkipMode::Query] {
                let result = run(&bytes, &root_plan(), mode);
                assert_raw(&result, 0, token, kind);
                assert_eq!(result.slots()[0].unwrap().span, Span32::new(4, token.len() as u32));
                assert_eq!(result.payload().as_slice(), bytes.as_slice());
                assert_eq!(result.fully_validated(), mode == SkipMode::Strict);
                assert_eq!(result.slots().len(), 1);
                assert_eq!(result.raw(1), None);
                assert_eq!(result.raw(u32::MAX), None);
                assert_eq!(result.string(u32::MAX), None);
                if kind != ValueKind::String {
                    assert_eq!(result.string(0), None);
                    assert_eq!(result.slots()[0].unwrap().decoded, None);
                }
            }
        }
    }

    #[test]
    fn shared_prefixes_capture_ancestors_descendants_and_multiple_terminal_slots() {
        let plan = SelectorPlan::new(vec![
            node(vec![key("items", 1), key("flag", 5)], &[0]),
            node(vec![index(1, 2), index(0, 4)], &[1]),
            node(vec![key("name", 3)], &[2]),
            node(vec![], &[3, 4]),
            node(vec![], &[5]),
            node(vec![], &[6]),
        ], 7).unwrap();
        let bytes = br#"{"flag":false,"items":[null,{"name":"x\ny","ignored":[0]}]}"#;
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let result = run(bytes, &plan, mode);
            assert_raw(&result, 0, bytes, ValueKind::Object);
            assert_raw(&result, 1, br#"[null,{"name":"x\ny","ignored":[0]}]"#, ValueKind::Array);
            assert_raw(&result, 2, br#"{"name":"x\ny","ignored":[0]}"#, ValueKind::Object);
            assert_raw(&result, 3, br#""x\ny""#, ValueKind::String);
            assert_eq!(result.slots()[3], result.slots()[4]);
            assert_eq!(result.string(3), Some("x\ny"));
            assert_eq!(result.string(4), Some("x\ny"));
            assert_raw(&result, 5, b"null", ValueKind::Null);
            assert_raw(&result, 6, b"false", ValueKind::Bool);
            assert_eq!(result.slots().len(), 7);
        }
    }

    #[test]
    fn indices_count_immediate_children_and_missing_is_distinct_from_null() {
        let plan = SelectorPlan::new(vec![
            node(vec![index(3, 1), index(0, 2), index(2, 3), index(4, 4),
                index(u32::MAX, 5), key("0", 6)], &[]),
            node(vec![], &[0]), node(vec![], &[1]), node(vec![], &[2]),
            node(vec![], &[3]), node(vec![], &[4]), node(vec![], &[5]),
        ], 6).unwrap();
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let result = run(br#"[[0,[1,2]],{"nested":[3,4]},null,5]"#, &plan, mode);
            assert_raw(&result, 0, b"5", ValueKind::Number);
            assert_raw(&result, 1, b"[0,[1,2]]", ValueKind::Array);
            assert_raw(&result, 2, b"null", ValueKind::Null);
            assert_eq!(&result.slots()[3..], &[None, None, None]);
            assert!(result.fully_validated());
        }
    }

    #[test]
    fn segment_type_mismatches_are_missing_paths() {
        let plan = SelectorPlan::new(vec![
            node(vec![key("a", 1), index(0, 2)], &[]),
            node(vec![index(0, 3)], &[]),
            node(vec![key("a", 4)], &[]),
            node(vec![], &[0]), node(vec![], &[1]),
        ], 2).unwrap();
        for bytes in [&b"null"[..], b"true", b"42", br#""a""#, br#"{"a":{}}"#,
            br#"{"a":"text"}"#, b"[[]]", b"[]", b"{}"]
        {
            for mode in [SkipMode::Strict, SkipMode::Query] {
                let result = run(bytes, &plan, mode);
                assert_eq!(result.slots(), &[None, None]);
                assert!(result.fully_validated());
            }
        }
    }

    #[test]
    fn decoded_keys_match_exactly_without_normalization_or_string_coercion() {
        let plan = SelectorPlan::new(vec![
            node(vec![key("a", 1), key("é", 2), key("e\u{301}", 3), key("", 4),
                key("\0", 5), key("😀", 6), key("0", 7), key("A", 8)], &[]),
            node(vec![], &[0]), node(vec![], &[1]), node(vec![], &[2]),
            node(vec![], &[3]), node(vec![], &[4]), node(vec![], &[5]),
            node(vec![], &[6]), node(vec![], &[7]),
        ], 8).unwrap();
        let bytes = br#"{"\u0061":1,"\u00e9":2,"e\u0301":3,"":4,"\u0000":5,"\ud83d\ude00":6,"0":7}"#;
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let result = run(bytes, &plan, mode);
            for slot in 0..7 {
                assert_eq!(result.raw(slot), Some(&[b'1' + slot as u8][..]));
            }
            assert_eq!(result.slots()[7], None);
            assert!(result.fully_validated());
        }
    }

    #[test]
    fn string_references_are_preserved_and_survive_all_input_and_plan_owners() {
        let bytes = r#"{"\u006b":["é","\uD83D\uDE00\n","","\u0000","\\\"}"]}"#.as_bytes();
        let mut cursor = walk::from_payload(
            Payload::from_slice(bytes), Limits::strict_default(), SkipMode::Strict,
        ).unwrap();
        let mut expected = Vec::new();
        for _ in 0..=bytes.len() + 1 {
            match walk::next(&mut cursor).unwrap() {
                ScanStep::Emit(Event::Scalar { kind: ScalarKind::String, span, decoded }) => {
                    expected.push(SelectedValue { kind: ValueKind::String, span, decoded });
                }
                ScanStep::Emit(_) => {}
                ScanStep::Finished => break,
                ScanStep::NeedMore => panic!("complete input requested another chunk"),
            }
        }
        assert!(cursor.syntax.is_finished());
        assert_eq!(expected.len(), 5);
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let result = {
                let mut input = bytes.to_vec();
                let payload = Payload::from_slice(&input);
                input.fill(b'!');
                let mut nodes = vec![node(vec![key("k", 1)], &[])];
                nodes.push(node((0..5).map(|i| index(i, i + 2)).collect(), &[]));
                for slot in 0..5 {
                    nodes.push(node(vec![], &[slot]));
                }
                let plan = SelectorPlan::new(nodes, 5).unwrap();
                extract(payload, &plan, mode, Limits::strict_default()).unwrap()
            };
            for (slot, value) in expected.iter().enumerate() {
                assert_eq!(result.slots()[slot], Some(*value));
            }
            assert_eq!(result.string(0), Some("é"));
            assert_eq!(result.string(1), Some("😀\n"));
            assert_eq!(result.string(2), Some(""));
            assert_eq!(result.string(3), Some("\0"));
            assert_eq!(result.string(4), Some("\\\"}"));
            assert_eq!(result.payload().as_slice(), bytes);
            assert_eq!(result.decoded_bytes(), cursor.decoded_bytes());
            assert_eq!(result.slots()[0].unwrap().decoded.unwrap().source, SpanSource::RawPayload);
            assert_eq!(result.slots()[1].unwrap().decoded.unwrap().source, SpanSource::DecodedArena);
            let clone = result.clone();
            drop(result);
            assert_eq!(clone.string(1), Some("😀\n"));
        }
    }

    #[test]
    fn duplicate_members_fill_each_slot_at_its_first_complete_match() {
        let plan = SelectorPlan::new(vec![
            node(vec![key("a", 1)], &[]),
            node(vec![key("x", 2), key("y", 3)], &[0]),
            node(vec![], &[1]), node(vec![], &[2]),
        ], 3).unwrap();
        let bytes = br#"{"a":{},"\u0061":{"x":null},"a":{"x":9,"y":[]},"a":{"y":[1]}}"#;
        let limits = Limits { reject_duplicate_keys: false, ..Limits::strict_default() };
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let result = extract(Payload::from_slice(bytes), &plan, mode, limits).unwrap();
            assert_raw(&result, 0, b"{}", ValueKind::Object);
            assert_raw(&result, 1, b"null", ValueKind::Null);
            assert_raw(&result, 2, b"[]", ValueKind::Array);
            assert_eq!(result.fully_validated(), mode == SkipMode::Strict);
        }
    }

    #[test]
    fn shared_dag_targets_follow_document_order_not_graph_order() {
        let plan = SelectorPlan::new(vec![
            node(vec![key("a", 1), key("b", 2), key("c", 3)], &[]),
            node(vec![key("v", 4)], &[]),
            node(vec![index(0, 4)], &[]),
            node(vec![key("v", 4)], &[]),
            node(vec![key("leaf", 5)], &[0, 1]),
            node(vec![], &[2]),
        ], 3).unwrap();
        let cases: &[(&[u8], &[u8], &[u8])] = &[
            (br#"{"b":[{}],"a":{"v":{"leaf":2}},"c":{"v":{"leaf":3}}}"#, b"{}", b"2"),
            (br#"{"c":{"v":{"leaf":3}},"a":{"v":{}},"b":[{}]}"#, br#"{"leaf":3}"#, b"3"),
        ];
        for &(bytes, first, leaf) in cases {
            for mode in [SkipMode::Strict, SkipMode::Query] {
                let result = run(bytes, &plan, mode);
                assert_raw(&result, 0, first, ValueKind::Object);
                assert_eq!(result.slots()[0], result.slots()[1]);
                assert_raw(&result, 2, leaf, ValueKind::Number);
            }
        }
    }

    #[test]
    fn query_stops_only_after_a_complete_match_and_keeps_the_entire_payload() {
        let plan = SelectorPlan::new(vec![
            node(vec![key("pick", 1)], &[]), node(vec![], &[0]),
        ], 1).unwrap();
        let cases: &[(&[u8], &[u8])] = &[
            (br#"{"pick":[],"bad":!}"#, b"[]"),
            (br#"{"pick":{"x":[0]},"bad":"\q"}"#, br#"{"x":[0]}"#),
            (br#"{"pick":"\n""#, br#""\n""#),
            (br#"{"pick":null,"pick":true}"#, b"null"),
        ];
        for &(bytes, selected) in cases {
            let payload = Payload::from_vec(bytes.to_vec());
            let original = payload.as_slice().as_ptr();
            let result = extract(payload, &plan, SkipMode::Query, Limits::strict_default()).unwrap();
            assert_eq!(result.raw(0), Some(selected));
            assert_eq!(result.payload().as_slice(), bytes);
            assert_eq!(result.payload().as_slice().as_ptr(), original);
            assert!(!result.fully_validated());
            let expected = syntax_error_for(bytes, Limits::strict_default());
            assert_eq!(extract(Payload::from_slice(bytes), &plan, SkipMode::Strict,
                Limits::strict_default()).unwrap_err(), expected);
        }
        for bytes in [&br#"{"pick":[0,]}"#[..], br#"{"pick":{"x":1"#,
            br#"{"pick":"\q"}"#, br#"{"pick":01}"#]
        {
            assert!(extract(Payload::from_slice(bytes), &plan, SkipMode::Query,
                Limits::strict_default()).is_err());
        }
    }

    #[test]
    fn zero_slots_query_is_unvalidated_but_still_enforces_payload_limits() {
        let plan = SelectorPlan::new(vec![SelectorNode::default()], 0).unwrap();
        for bytes in [&b""[..], b"!", b"\xff", b"[", b"null", b"{} trailing"] {
            let result = run(bytes, &plan, SkipMode::Query);
            assert!(result.slots().is_empty());
            assert!(result.decoded_bytes().is_empty());
            assert_eq!(result.payload().as_slice(), bytes);
            assert!(!result.fully_validated());
            if bytes == b"null" {
                assert!(run(bytes, &plan, SkipMode::Strict).fully_validated());
            } else {
                assert!(extract(Payload::from_slice(bytes), &plan, SkipMode::Strict,
                    Limits::strict_default()).is_err());
            }
        }
        let limits = Limits { max_payload_bytes: 0, ..Limits::strict_default() };
        let result = extract(Payload::from_slice(b""), &plan, SkipMode::Query, limits).unwrap();
        assert!(!result.fully_validated());
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let error = extract(Payload::from_slice(b"0"), &plan, mode, limits).unwrap_err();
            assert_eq!(error.code, ErrorCode::PayloadTooLarge);
            assert_eq!(error, syntax_error_for(b"0", limits));
        }
    }

    #[test]
    fn missing_slots_force_query_to_validate_the_suffix() {
        let plan = missing_plan();
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let result = run(br#"{"present":[1,2]}  "#, &plan, mode);
            assert_eq!(result.slots(), &[None]);
            assert!(result.fully_validated());
            for bytes in [&b"{} false"[..], br#"{"present":[1,]}"#, b"{}\xff"] {
                assert_eq!(extract(Payload::from_slice(bytes), &plan, mode,
                    Limits::strict_default()).unwrap_err(), syntax_error_for(bytes, Limits::strict_default()));
            }
        }
    }

    #[test]
    fn syntax_errors_preserve_exact_source_offset_and_detail() {
        let cases: &[&[u8]] = &[
            b"", b" \t\n", b"[", b"{\"a\":", b"[[0]", b"[tru", b"[1e",
            b"[0,]", b"{\"a\":0,}", b"{0:1}", b"[}", b"[truex]", b"[01]",
            b"[+1]", b"[\"unterminated", b"[\"\\q\"]", b"[\"\\uD800\"]",
            b"[\"\\uDC00\"]", b"[\"\x01\"]", b"[\"\xff\"]",
            br#"{"a":0,"\u0061":1}"#, br#"[{"ok":0},{"x":1,"x":2}]"#,
            b"null true", b"{}\xff",
        ];
        for &bytes in cases {
            let expected = syntax_error_for(bytes, Limits::strict_default());
            for mode in [SkipMode::Strict, SkipMode::Query] {
                let error = extract(Payload::from_slice(bytes), &missing_plan(), mode,
                    Limits::strict_default()).unwrap_err();
                assert_eq!(error, expected);
            }
        }
    }

    #[test]
    fn inclusive_limits_apply_even_to_unselected_values() {
        let defaults = Limits::strict_default();
        let cases: &[(Limits, &[u8], &[u8], ErrorCode)] = &[
            (Limits { max_payload_bytes: 2, ..defaults }, b"[]", b"[] ", ErrorCode::PayloadTooLarge),
            (Limits { max_depth: 0, ..defaults }, b"0", b"[]", ErrorCode::DepthLimitExceeded),
            (Limits { max_depth: 2, ..defaults }, b"[[]]", b"[[[]]]", ErrorCode::DepthLimitExceeded),
            (Limits { max_array_items: 0, ..defaults }, b"[]", b"[0]", ErrorCode::ConstraintViolation),
            (Limits { max_array_items: 1, ..defaults }, b"[0]", b"[0,1]", ErrorCode::ConstraintViolation),
            (Limits { max_fields_per_object: 0, ..defaults }, b"{}", br#"{"a":0}"#, ErrorCode::ConstraintViolation),
            (Limits { max_fields_per_object: 1, ..defaults }, br#"{"a":0}"#, br#"{"a":0,"b":1}"#, ErrorCode::ConstraintViolation),
            (Limits { max_string_bytes: 1, ..defaults }, br#"["a"]"#, br#"["ab"]"#, ErrorCode::ConstraintViolation),
            (Limits { max_string_bytes: 0, ..defaults }, br#"[""]"#, br#"["a"]"#, ErrorCode::ConstraintViolation),
            (Limits { max_number_digits: 2, ..defaults }, b"[1e2]", b"[1e23]", ErrorCode::ConstraintViolation),
            (Limits { max_decoded_bytes: 1, ..defaults }, br#"["\n"]"#, br#"["\n","\t"]"#, ErrorCode::DecodedArenaOverflow),
        ];
        for &(limits, valid, invalid, code) in cases {
            for mode in [SkipMode::Strict, SkipMode::Query] {
                let result = extract(Payload::from_slice(valid), &missing_plan(), mode, limits).unwrap();
                assert!(result.fully_validated());
                assert_eq!(result.slots(), &[None]);
                let error = extract(Payload::from_slice(invalid), &missing_plan(), mode, limits).unwrap_err();
                assert_eq!(error.code, code);
                assert_eq!(error, syntax_error_for(invalid, limits));
            }
        }
    }

    #[test]
    fn deep_selector_and_container_paths_use_an_iterative_stack() {
        let depth = 1024usize;
        let mut bytes = vec![b'['; depth];
        bytes.push(b'0');
        bytes.resize(depth * 2 + 1, b']');
        let mut nodes = Vec::new();
        for i in 0..depth {
            nodes.push(node(vec![index(0, (i + 1) as u32)], &[i as u32]));
        }
        nodes.push(node(vec![], &[depth as u32]));
        let plan = SelectorPlan::new(nodes, depth as u32 + 1).unwrap();
        let limits = Limits { max_depth: depth as u16, ..Limits::strict_default() };
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let result = extract(Payload::from_slice(&bytes), &plan, mode, limits).unwrap();
            for i in 0..depth {
                assert_eq!(result.slots()[i].unwrap().span,
                    Span32::new(i as u32, (bytes.len() - 2 * i) as u32));
                assert_eq!(result.slots()[i].unwrap().kind, ValueKind::Array);
            }
            assert_raw(&result, depth as u32, b"0", ValueKind::Number);
        }
    }

    #[test]
    fn maximum_slot_count_and_many_alias_slots_capture_the_same_value() {
        let slots: Vec<u32> = (0..MAX_SELECTION_SLOTS).collect();
        let plan = SelectorPlan::new(vec![node(vec![], &slots)], MAX_SELECTION_SLOTS).unwrap();
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let result = run(b"null", &plan, mode);
            assert_eq!(result.slots().len(), MAX_SELECTION_SLOTS as usize);
            let expected = Some(SelectedValue {
                kind: ValueKind::Null, span: Span32::new(0, 4), decoded: None,
            });
            assert!(result.slots().iter().all(|value| *value == expected));
            assert_eq!(result.raw(MAX_SELECTION_SLOTS - 1), Some(&b"null"[..]));
            assert_eq!(result.raw(MAX_SELECTION_SLOTS), None);
        }
    }

    #[test]
    fn container_span_arithmetic_checks_representation_boundaries() {
        assert_eq!(container_span(0, 1, 8).unwrap(), Span32::new(0, 2));
        assert_eq!(container_span(0, u32::MAX - 1, 8).unwrap(), Span32::new(0, u32::MAX));
        assert_eq!(container_span(u32::MAX - 2, u32::MAX - 1, 8).unwrap(),
            Span32::new(u32::MAX - 2, 2));
        for (start, close) in [(0, 0), (3, 1), (u32::MAX, u32::MAX - 1)] {
            let error = container_span(start, close, 17).unwrap_err();
            assert_eq!((error.code, error.source, error.byte_offset),
                (ErrorCode::InternalError, ErrorSource::Cursor, 17));
        }
        let error = container_span(0, u32::MAX, 19).unwrap_err();
        assert_eq!((error.code, error.source, error.byte_offset),
            (ErrorCode::PayloadTooLarge, ErrorSource::Cursor, 19));
    }

    #[test]
    fn allocation_capacity_failure_preserves_storage_and_reports_current_offset() {
        let mut storage = vec![7u8];
        // This exceeds Vec's addressable byte capacity without asking the
        // allocator for a large block or depending on available memory.
        let error = reserve_exact(&mut storage, usize::MAX, 29,
            "selection slot allocation failed").unwrap_err();
        assert_eq!((error.code, error.source, error.byte_offset),
            (ErrorCode::BudgetExhausted, ErrorSource::Cursor, 29));
        assert_eq!(error.detail, "selection slot allocation failed");
        assert_eq!(storage, [7]);
        reserve_exact(&mut storage, 0, 30, "selection slot allocation failed").unwrap();
        assert_eq!(storage, [7]);
    }

    #[test]
    fn impossible_graph_and_capture_states_return_local_errors() {
        for plan in [
            SelectorPlan { nodes: vec![], slot_count: 0 },
            SelectorPlan { nodes: vec![SelectorNode::default()], slot_count: MAX_SELECTION_SLOTS + 1 },
        ] {
            let error = extract(Payload::from_slice(b"0"), &plan, SkipMode::Strict,
                Limits::strict_default()).unwrap_err();
            assert_eq!((error.code, error.source, error.byte_offset),
                (ErrorCode::InternalError, ErrorSource::Cursor, 0));
        }
        for target in [0, 1, u32::MAX] {
            let plan = SelectorPlan { nodes: vec![node(vec![key("a", target)], &[])], slot_count: 0 };
            let error = matching_child(&plan, Some(0), 7, |_| true).unwrap_err();
            assert_eq!((error.code, error.source, error.byte_offset),
                (ErrorCode::InternalError, ErrorSource::Cursor, 7));
        }
        let plan = root_plan();
        assert_eq!(matching_child(&plan, None, 3, |_| true).unwrap(), None);
        assert_eq!(selector_node(&plan, u32::MAX, 5).unwrap_err().byte_offset, 5);
        let value = SelectedValue { kind: ValueKind::Null, span: Span32::new(0, 4), decoded: None };
        let mut remaining = 0;
        let mut slots = [None];
        let error = capture(&plan, Some(0), value, &mut slots, &mut remaining, 11).unwrap_err();
        assert_eq!((error.code, error.source, error.byte_offset),
            (ErrorCode::InternalError, ErrorSource::Cursor, 11));
        assert_eq!(slots, [None]);
        let mut remaining = 1;
        assert_eq!(capture(&plan, Some(0), value, &mut [], &mut remaining, 13)
            .unwrap_err().byte_offset, 13);
        capture(&plan, Some(0), value, &mut slots, &mut remaining, 0).unwrap();
        assert_eq!(remaining, 0);
        capture(&plan, Some(0), SelectedValue { kind: ValueKind::Number, ..value },
            &mut slots, &mut remaining, 0).unwrap();
        assert_eq!(slots, [Some(value)]);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn impossible_value_order_and_array_counter_overflow_are_errors() {
        let plan = root_plan();
        let mut root_started = false;
        assert_eq!(value_node(&plan, &mut [], &mut root_started, 0).unwrap(), Some(0));
        assert!(root_started);
        let error = value_node(&plan, &mut [], &mut root_started, 9).unwrap_err();
        assert_eq!((error.code, error.source, error.byte_offset),
            (ErrorCode::InternalError, ErrorSource::Cursor, 9));
        for state in [ContainerState::Object { pending: None },
            ContainerState::Array { next_index: u32::MAX }]
        {
            let mut frames = [SelectionFrame { node: None, start: 0, state }];
            let error = value_node(&plan, &mut frames, &mut root_started, 21).unwrap_err();
            assert_eq!((error.code, error.source, error.byte_offset),
                (ErrorCode::InternalError, ErrorSource::Cursor, 21));
        }
        let mut frames = [SelectionFrame {
            node: None, start: 0, state: ContainerState::Object { pending: Some(None) },
        }];
        assert_eq!(value_node(&plan, &mut frames, &mut root_started, 0).unwrap(), None);
        assert!(matches!(frames[0].state, ContainerState::Object { pending: None }));
    }
}
