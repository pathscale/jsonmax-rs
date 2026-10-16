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
/// The returned owner retains the entire supplied payload, including an
/// unconsumed suffix. Query-only skipped interiors keep `fully_validated` false
/// even when the grammar reaches EOF. Errors return no partial result.
pub fn extract(
    payload: Payload,
    plan: &SelectorPlan,
    mode: SkipMode,
    limits: Limits,
) -> Result<SelectedValues> {
    let mut cursor = walk::from_payload(payload, limits, mode)?;
    let slots = extract_slots(&mut cursor, plan, mode)?;
    let fully_validated = cursor.syntax.fully_validated();
    let output = cursor.syntax.into_storage()?;
    Ok(SelectedValues {
        payload: output.payload,
        decoded: output.decoded,
        slots,
        fully_validated,
    })
}

/// Borrows the exact complete input allocation for raw selection spans.
pub fn select_borrowed<'a>(
    input: &'a [u8],
    plan: &SelectorPlan,
    mode: SkipMode,
    limits: Limits,
) -> Result<BorrowedSelectedValues<'a>> {
    select_borrowed_with_numeric(input, plan, mode, limits, None)
}
/// Borrowed extraction with optional number conversion policy.
pub fn select_borrowed_with_numeric<'a>(
    input: &'a [u8],
    plan: &SelectorPlan,
    mode: SkipMode,
    limits: Limits,
    numeric: Option<jmax_abi::number::NumericPolicy>,
) -> Result<BorrowedSelectedValues<'a>> {
    let mut cursor = walk::walk_borrowed_with_numeric(input, limits, mode, numeric)?;
    let slots = extract_slots(&mut cursor, plan, mode)?;
    let fully_validated = cursor.fully_validated();
    Ok(BorrowedSelectedValues {
        input,
        decoded: cursor.decoded,
        slots,
        fully_validated,
    })
}

trait SelectionCursor {
    fn next_step(&mut self) -> Result<ScanStep>;
    fn state(&self) -> CursorState;
    fn raw_bytes(&self) -> &[u8];
    fn resolve_str(&self, reference: StrRef) -> Option<&str>;
    fn completed(&self) -> bool;
    fn skip_container(&mut self, mode: SkipMode) -> Result<SelectedValue>;
}
impl SelectionCursor for Cursor {
    fn next_step(&mut self) -> Result<ScanStep> {
        walk::next(self)
    }
    fn state(&self) -> CursorState {
        Cursor::state(self)
    }
    fn raw_bytes(&self) -> &[u8] {
        Cursor::raw_bytes(self)
    }
    fn resolve_str(&self, reference: StrRef) -> Option<&str> {
        Cursor::resolve_str(self, reference)
    }
    fn completed(&self) -> bool {
        self.syntax.is_finished()
    }
    fn skip_container(&mut self, mode: SkipMode) -> Result<SelectedValue> {
        match skip::subtree(self, mode)? {
            SkipStep::Complete { value, .. } => Ok(value),
            SkipStep::NeedMore => Err(cursor_error(
                ErrorCode::UnexpectedEof,
                self.state().byte_offset,
                "selection requires complete input",
            )),
        }
    }
}
impl SelectionCursor for BorrowedCursor<'_> {
    fn next_step(&mut self) -> Result<ScanStep> {
        self.next()
    }
    fn state(&self) -> CursorState {
        BorrowedCursor::state(self)
    }
    fn raw_bytes(&self) -> &[u8] {
        BorrowedCursor::raw_bytes(self)
    }
    fn resolve_str(&self, reference: StrRef) -> Option<&str> {
        BorrowedCursor::resolve_str(self, reference)
    }
    fn completed(&self) -> bool {
        self.scanner.is_finished()
    }
    fn skip_container(&mut self, mode: SkipMode) -> Result<SelectedValue> {
        self.skip_current(match mode {
            SkipMode::Query => ValueSkipMode::QueryOnly,
            SkipMode::Strict => ValueSkipMode::StrictValidate,
        })
    }
}

fn extract_slots(
    cursor: &mut impl SelectionCursor,
    plan: &SelectorPlan,
    mode: SkipMode,
) -> Result<Vec<Option<SelectedValue>>> {
    // Public construction already validates the graph. Keep impossible internal
    // state recoverable instead of indexing blindly or silently losing slots.
    if plan.nodes.is_empty()
        || plan.nodes.len() > MAX_SELECTOR_NODES
        || plan.slot_count > MAX_SELECTION_SLOTS
    {
        return Err(internal(0, "selector graph has invalid dimensions"));
    }
    let count = usize::try_from(plan.slot_count).map_err(|_| {
        cursor_error(
            ErrorCode::BudgetExhausted,
            0,
            "selector slots exceed addressable storage",
        )
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
        let event = match cursor.next_step()? {
            ScanStep::Emit(event) => event,
            ScanStep::Finished => {
                if !root_started || !frames.is_empty() || !cursor.completed() {
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
                let key = cursor
                    .resolve_str(str_ref)
                    .ok_or_else(|| internal(at, "object key has an invalid decoded reference"))?;
                let frame = frames
                    .last_mut()
                    .ok_or_else(|| internal(at, "object key has no containing object"))?;
                match &mut frame.state {
                    ContainerState::Object { pending } if pending.is_none() => {
                        *pending = Some(matching_child(
                            plan,
                            frame.node,
                            at,
                            |segment| matches!(segment, Segment::Key(expected) if expected.as_bytes() == key.as_bytes()),
                        )?);
                    }
                    _ => return Err(internal(at, "object key is out of selection order")),
                }
            }
            Event::ObjectEnd { offset } | Event::ArrayEnd { offset } => {
                let frame = frames
                    .pop()
                    .ok_or_else(|| internal(at, "container end has no selection frame"))?;
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
                    plan,
                    frame.node,
                    SelectedValue {
                        kind,
                        span,
                        decoded: None,
                    },
                    &mut slots,
                    &mut remaining,
                    at,
                )?;
            }
            Event::ObjectStart { offset } | Event::ArrayStart { offset } => {
                let node = value_node(plan, &mut frames, &mut root_started, at)?;
                let no_children = match node {
                    Some(node) => selector_node(plan, node, at)?.edges.is_empty(),
                    None => true,
                };
                if no_children {
                    let value = cursor.skip_container(mode)?;
                    capture(plan, node, value, &mut slots, &mut remaining, at)?;
                    continue;
                }
                let state = match event {
                    Event::ObjectStart { .. } => ContainerState::Object { pending: None },
                    _ => ContainerState::Array { next_index: 0 },
                };
                frames.try_reserve(1).map_err(|_| {
                    cursor_error(
                        ErrorCode::BudgetExhausted,
                        at,
                        "selection frame allocation failed",
                    )
                })?;
                frames.push(SelectionFrame {
                    node,
                    start: offset,
                    state,
                });
            }
            Event::Scalar {
                kind,
                span,
                decoded,
            } => {
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
                    && decoded
                        .and_then(|reference| cursor.resolve_str(reference))
                        .is_none()
                {
                    return Err(internal(
                        at,
                        "selected string has an invalid decoded reference",
                    ));
                }
                capture(
                    plan,
                    node,
                    SelectedValue {
                        kind,
                        span,
                        decoded,
                    },
                    &mut slots,
                    &mut remaining,
                    at,
                )?;
            }
        }
    }

    Ok(slots)
}

fn reserve_exact<T>(
    storage: &mut Vec<T>,
    additional: usize,
    at: u32,
    detail: &'static str,
) -> Result<()> {
    storage
        .try_reserve_exact(additional)
        .map_err(|_| cursor_error(ErrorCode::BudgetExhausted, at, detail))
}

fn selector_node(plan: &SelectorPlan, index: u32, at: u32) -> Result<&SelectorNode> {
    let index = usize::try_from(index)
        .map_err(|_| internal(at, "selector node exceeds addressable storage"))?;
    plan.nodes
        .get(index)
        .ok_or_else(|| internal(at, "selector node is out of bounds"))
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
                return Err(internal(
                    at,
                    "selector edge does not advance through the graph",
                ));
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
        ContainerState::Object { pending } => pending
            .take()
            .ok_or_else(|| internal(at, "object value has no preceding selection key")),
        ContainerState::Array { next_index } => {
            let index = *next_index;
            *next_index = index
                .checked_add(1)
                .ok_or_else(|| internal(at, "selection array item count overflow"))?;
            matching_child(
                plan,
                frame.node,
                at,
                |segment| matches!(segment, Segment::Index(expected) if *expected == index),
            )
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
            let index = usize::try_from(slot)
                .map_err(|_| internal(at, "selection slot exceeds addressable storage"))?;
            let destination = slots
                .get_mut(index)
                .ok_or_else(|| internal(at, "selection terminal slot is out of bounds"))?;
            if destination.is_none() {
                *remaining = remaining
                    .checked_sub(1)
                    .ok_or_else(|| internal(at, "selection remaining slot count underflow"))?;
                *destination = Some(value);
            }
        }
    }
    Ok(())
}

fn container_span(start: u32, close: u32, at: u32) -> Result<Span32> {
    let end = close.checked_add(1).ok_or_else(|| {
        cursor_error(
            ErrorCode::PayloadTooLarge,
            at,
            "selected container end exceeds span capacity",
        )
    })?;
    let len = end
        .checked_sub(start)
        .filter(|&len| len >= 2)
        .ok_or_else(|| internal(at, "selected container delimiters are out of order"))?;
    Ok(Span32::new(start, len))
}

const fn internal(at: u32, detail: &'static str) -> Error {
    cursor_error(ErrorCode::InternalError, at, detail)
}
