//! Strict and query-only path selection over owned or borrowed JSON input.

use alloc::{string::String, vec::Vec};

use jmax_cursor::{Event, SkipMode};
use jmax_path::{PathLimits, Segment};

use super::{
    BorrowedQueryResult, CompiledQuery, Error, ErrorCode, ErrorSource, ParseOptions, Payload,
    QueryMode, QueryOptions, QueryResult, Result, Span32,
};

/// Validates and owns a sequence of native dot/escape/index paths.
///
/// Paths are retained byte-for-byte and in caller order. Repeated paths are
/// intentional: each occurrence receives its own result vector.
pub fn compile_paths(paths: &[&str]) -> Result<CompiledQuery> {
    let limits = PathLimits::default();
    if paths.len() > limits.max_paths as usize {
        return Err(path_error(
            ErrorCode::BudgetExhausted,
            0,
            "path count limit exceeded",
        ));
    }

    let mut owned = Vec::new();
    owned
        .try_reserve_exact(paths.len())
        .map_err(|_| allocation_error())?;
    for path in paths {
        // The native parser supplies the canonical grammar and error offsets.
        let _parsed = jmax_path::parse::parse(path, limits)?;
        let mut copy = String::new();
        copy.try_reserve_exact(path.len())
            .map_err(|_| allocation_error())?;
        copy.push_str(path);
        owned.push(copy);
    }
    let selector = SelectorTrie::compile(&owned)?;
    Ok(CompiledQuery {
        paths: owned,
        selector,
    })
}

/// Selects raw values while preserving the original payload owner. Strict mode
/// validates every byte; query mode can structurally skip unselected interiors.
pub fn query_payload(
    input: Payload,
    query: &CompiledQuery,
    options: QueryOptions,
) -> Result<QueryResult> {
    let borrowed = execute_borrowed(query, input.as_slice(), options)?;
    let matches = borrowed.matches;
    let fully_validated = borrowed.fully_validated;
    Ok(QueryResult {
        payload: input,
        matches,
        fully_validated,
    })
}

/// Executes a retained plan while preserving the supplied payload owner.
pub fn execute(
    query: &CompiledQuery,
    input: Payload,
    options: QueryOptions,
) -> Result<QueryResult> {
    query_payload(input, query, options)
}

/// Owned query execution with reusable traversal and recycled result storage.
pub fn execute_with_scratch(
    query: &CompiledQuery,
    input: Payload,
    options: QueryOptions,
    scratch: &mut QueryScratch,
) -> Result<QueryResult> {
    let borrowed = execute_borrowed_with_scratch(query, input.as_slice(), options, scratch)?;
    let matches = borrowed.matches;
    let fully_validated = borrowed.fully_validated;
    Ok(QueryResult {
        payload: input,
        matches,
        fully_validated,
    })
}

/// Executes directly over the original immutable slice. Query-only structural
/// skips never issue a ValidationProof. Selected complete container values still
/// receive strict validation, including when their descendants are also queried.
pub fn execute_borrowed<'a>(
    query: &CompiledQuery,
    input: &'a [u8],
    options: QueryOptions,
) -> Result<BorrowedQueryResult<'a>> {
    execute_borrowed_with_scratch(query, input, options, &mut QueryScratch::default())
}

/// Reusable traversal stack and escaped-key arena. Match vectors belong to the
/// returned result; scratch contains no input references and can serve any plan.
#[derive(Default)]
pub struct QueryScratch {
    frames: Vec<Frame>,
    decoded: Vec<u8>,
    key_tables: Vec<QueryKeyTable>,
    matches: Vec<Vec<Span32>>,
    skip: jmax_syntax::SkipScratch,
}

impl QueryScratch {
    /// Return result storage after consuming all borrowed matches.
    pub fn recycle_borrowed(&mut self, result: BorrowedQueryResult<'_>) {
        self.matches = result.matches;
    }

    /// Return result storage and release the result's retained payload owner.
    pub fn recycle_owned(&mut self, result: QueryResult) {
        self.matches = result.matches;
    }
}

#[derive(Default)]
struct QueryKeyTable {
    entries: Vec<QueryKey>,
    generation: u32,
    used: usize,
}

#[derive(Clone, Copy)]
struct QueryKey {
    hash: u64,
    reference: super::StrRef,
    generation: u32,
}

impl QueryKey {
    const EMPTY: Self = Self {
        hash: 0,
        reference: super::StrRef {
            span: Span32::new(0, 0),
            source: super::SpanSource::RawPayload,
            scalar_len: 0,
        },
        generation: 0,
    };
}

impl QueryKeyTable {
    fn reset(&mut self) {
        self.used = 0;
        self.generation = match self.generation.checked_add(1) {
            Some(next) => next,
            None => {
                self.entries.fill(QueryKey::EMPTY);
                1
            }
        };
    }

    fn insert(&mut self, key: jmax_abi::syntax::KeyMeta, raw: &[u8], decoded: &[u8]) -> Result<()> {
        let bytes = |reference: super::StrRef| {
            reference.span.slice(match reference.source {
                super::SpanSource::RawPayload => raw,
                super::SpanSource::DecodedArena => decoded,
            })
        };
        if self.used >= self.entries.len() / 2 {
            let capacity = self
                .entries
                .len()
                .checked_mul(2)
                .ok_or_else(allocation_error)?
                .max(8);
            let mut entries = Vec::new();
            entries
                .try_reserve_exact(capacity)
                .map_err(|_| allocation_error())?;
            entries.resize(capacity, QueryKey::EMPTY);
            for entry in self
                .entries
                .iter()
                .filter(|entry| entry.generation == self.generation)
            {
                let mut index = entry.hash as usize & (capacity - 1);
                while entries[index].generation == self.generation {
                    index = (index + 1) & (capacity - 1);
                }
                entries[index] = *entry;
            }
            self.entries = entries;
        }
        let hash = key.cooked_hash;
        let mut index = hash as usize & (self.entries.len() - 1);
        loop {
            let previous = &mut self.entries[index];
            if previous.generation != self.generation {
                *previous = QueryKey {
                    hash,
                    reference: key.string.cooked,
                    generation: self.generation,
                };
                self.used += 1;
                return Ok(());
            }
            if previous.hash == hash && bytes(previous.reference) == bytes(key.string.cooked) {
                return Err(path_error(
                    ErrorCode::DuplicateObjectKey,
                    key.string.raw_token.offset,
                    "duplicate query object key",
                ));
            }
            index = (index + 1) & (self.entries.len() - 1);
        }
    }
}

/// Execute a retained selector without parsing paths or building selector nodes.
pub fn execute_borrowed_with_scratch<'a>(
    query: &CompiledQuery,
    input: &'a [u8],
    options: QueryOptions,
    scratch: &mut QueryScratch,
) -> Result<BorrowedQueryResult<'a>> {
    if options.max_matches == 0 {
        return Err(path_error(
            ErrorCode::ConstraintViolation,
            0,
            "query match limit must be positive",
        ));
    }

    let selector = &query.selector;
    scratch.frames.clear();
    scratch.decoded.clear();
    let mut matches = core::mem::take(&mut scratch.matches);
    matches.truncate(query.paths().len());
    for values in &mut matches {
        values.clear();
    }
    matches
        .try_reserve(query.paths().len() - matches.len())
        .map_err(|_| allocation_error())?;
    matches.resize_with(query.paths().len(), Vec::new);

    if !selector.has_overlapping_terminals {
        let mut skip = core::mem::take(&mut scratch.skip);
        let result =
            execute_query_blocks(input, selector, options, scratch, &mut matches, &mut skip);
        scratch.skip = skip;
        result?;
        return Ok(BorrowedQueryResult {
            raw: input,
            matches,
            fully_validated: options.mode == QueryMode::Strict,
        });
    }

    let mode = match options.mode {
        QueryMode::Strict => SkipMode::Strict,
        QueryMode::Query => SkipMode::Query,
    };
    let mut cursor = jmax_cursor::walk::walk_borrowed_with_numeric(
        input,
        options.parse.limits,
        mode,
        Some(options.parse.numeric),
    )?;
    let stack = &mut scratch.frames;
    let mut total_matches = 0u32;

    loop {
        match cursor.next()? {
            jmax_abi::syntax::ScanStep::Emit(event) => match event {
                Event::Key { str_ref } => {
                    let frame = stack.last_mut().ok_or_else(|| {
                        internal_error(
                            str_ref.span.offset,
                            "cursor emitted an object key without a container",
                        )
                    })?;
                    if !frame.is_object {
                        return Err(internal_error(
                            str_ref.span.offset,
                            "cursor emitted an object key inside an array",
                        ));
                    }
                    let key = cursor.resolve_str(str_ref).ok_or_else(|| {
                        internal_error(
                            str_ref.span.offset,
                            "cursor emitted an invalid decoded object key",
                        )
                    })?;
                    frame.pending = frame
                        .selector_node
                        .and_then(|node| selector.child_key(node, key));
                }
                Event::ObjectStart { offset } | Event::ArrayStart { offset } => {
                    let inherited_strict = stack.last().is_some_and(|frame| frame.strict_subtree);
                    let node = take_value_node(stack, selector, offset)?;
                    let selected = node
                        .and_then(|index| selector.nodes.get(index))
                        .is_some_and(|node| !node.slots.is_empty());
                    let strict_subtree =
                        options.mode == QueryMode::Strict || inherited_strict || selected;
                    let no_children = node
                        .and_then(|index| selector.nodes.get(index))
                        .is_none_or(|node| node.edges.is_empty());
                    if no_children {
                        let skip_mode = if strict_subtree {
                            jmax_cursor::ValueSkipMode::StrictValidate
                        } else {
                            jmax_cursor::ValueSkipMode::QueryOnly
                        };
                        let value = cursor.skip_current(skip_mode)?;
                        add_matches(
                            node,
                            value.span,
                            selector,
                            &mut matches,
                            &mut total_matches,
                            options.max_matches,
                        )?;
                    } else {
                        let mut frame = if matches!(event, Event::ObjectStart { .. }) {
                            Frame::object(offset, node)
                        } else {
                            Frame::array(offset, node)
                        };
                        frame.strict_subtree = strict_subtree;
                        push_container(stack, frame)?;
                    }
                }
                Event::ObjectEnd { offset } => {
                    let frame = stack.pop().ok_or_else(|| {
                        internal_error(offset, "cursor closed an object without an open container")
                    })?;
                    if !frame.is_object {
                        return Err(internal_error(
                            offset,
                            "cursor container kinds are unbalanced",
                        ));
                    }
                    let span = completed_span(frame.start, offset)?;
                    add_matches(
                        frame.selector_node,
                        span,
                        selector,
                        &mut matches,
                        &mut total_matches,
                        options.max_matches,
                    )?;
                }
                Event::ArrayEnd { offset } => {
                    let frame = stack.pop().ok_or_else(|| {
                        internal_error(offset, "cursor closed an array without an open container")
                    })?;
                    if frame.is_object {
                        return Err(internal_error(
                            offset,
                            "cursor container kinds are unbalanced",
                        ));
                    }
                    let span = completed_span(frame.start, offset)?;
                    add_matches(
                        frame.selector_node,
                        span,
                        selector,
                        &mut matches,
                        &mut total_matches,
                        options.max_matches,
                    )?;
                }
                Event::Scalar { kind: _, span, .. } => {
                    let node = take_value_node(stack, selector, span.offset)?;
                    add_matches(
                        node,
                        span,
                        selector,
                        &mut matches,
                        &mut total_matches,
                        options.max_matches,
                    )?;
                }
            },
            jmax_abi::syntax::ScanStep::Finished => break,
            jmax_abi::syntax::ScanStep::NeedMore => {
                return Err(internal_error(
                    cursor.state().byte_offset,
                    "complete-input query cursor requested more bytes",
                ));
            }
        }
    }

    if !stack.is_empty() {
        return Err(internal_error(
            cursor.state().byte_offset,
            "query cursor finished with open containers",
        ));
    }

    Ok(BorrowedQueryResult {
        raw: input,
        matches,
        fully_validated: options.mode == QueryMode::Strict && cursor.fully_validated(),
    })
}

/// Copies input, compiles the supplied paths, and delegates to query_payload.
pub fn query_slice(input: &[u8], paths: &[&str], options: QueryOptions) -> Result<QueryResult> {
    let compiled = compile_paths(paths)?;
    query_payload(Payload::from_slice(input), &compiled, options)
}

/// Runs strict selection with the facade default match ceiling of 1,024.
pub fn strict_select(
    input: Payload,
    query: &CompiledQuery,
    options: ParseOptions,
) -> Result<QueryResult> {
    query_payload(
        input,
        query,
        QueryOptions {
            parse: options,
            mode: QueryMode::Strict,
            max_matches: 1024,
        },
    )
}

#[derive(Clone, Debug, Default)]
struct SelectorNode {
    edges: Vec<(Segment, usize)>,
    slots: Vec<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct SelectorTrie {
    nodes: Vec<SelectorNode>,
    has_overlapping_terminals: bool,
}

impl SelectorTrie {
    fn compile(paths: &[String]) -> Result<Self> {
        let limits = PathLimits::default();
        let mut trie = Self {
            nodes: Vec::new(),
            has_overlapping_terminals: false,
        };
        trie.nodes.try_reserve(1).map_err(|_| allocation_error())?;
        trie.nodes.push(SelectorNode::default());

        for (slot, source) in paths.iter().enumerate() {
            let path = jmax_path::parse::parse(source, limits)?;
            let mut node_index = 0usize;
            for segment in path.segments() {
                let existing = trie.nodes[node_index]
                    .edges
                    .iter()
                    .find(|(candidate, _)| candidate == segment)
                    .map(|(_, target)| *target);
                node_index = if let Some(target) = existing {
                    target
                } else {
                    let target = trie.nodes.len();
                    trie.nodes.try_reserve(1).map_err(|_| allocation_error())?;
                    let edge_segment = match segment {
                        Segment::Key(key) => {
                            let mut owned = String::new();
                            owned
                                .try_reserve_exact(key.len())
                                .map_err(|_| allocation_error())?;
                            owned.push_str(key);
                            Segment::Key(owned)
                        }
                        Segment::Index(index) => Segment::Index(*index),
                    };
                    let parent = trie.nodes.get_mut(node_index).ok_or_else(|| {
                        internal_error(0, "selector trie parent is outside its node table")
                    })?;
                    parent
                        .edges
                        .try_reserve(1)
                        .map_err(|_| allocation_error())?;
                    parent.edges.push((edge_segment, target));
                    trie.nodes.push(SelectorNode::default());
                    target
                };
            }
            let terminal = trie.nodes.get_mut(node_index).ok_or_else(|| {
                internal_error(0, "selector trie terminal is outside its node table")
            })?;
            terminal
                .slots
                .try_reserve(1)
                .map_err(|_| allocation_error())?;
            terminal.slots.push(slot);
        }
        trie.has_overlapping_terminals = trie
            .nodes
            .iter()
            .any(|node| !node.slots.is_empty() && !node.edges.is_empty());
        Ok(trie)
    }

    fn child_key(&self, node: usize, key: &str) -> Option<usize> {
        self.nodes
            .get(node)?
            .edges
            .iter()
            .find_map(|(segment, target)| match segment {
                Segment::Key(candidate) if candidate == key => Some(*target),
                _ => None,
            })
    }

    fn child_index(&self, node: usize, index: u32) -> Option<usize> {
        self.nodes
            .get(node)?
            .edges
            .iter()
            .find_map(|(segment, target)| match segment {
                Segment::Index(candidate) if *candidate == index => Some(*target),
                _ => None,
            })
    }
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    start: u32,
    selector_node: Option<usize>,
    is_object: bool,
    next_index: u32,
    pending: Option<usize>,
    strict_subtree: bool,
    phase: jmax_abi::syntax::FramePhase,
}

impl Frame {
    fn object(start: u32, selector_node: Option<usize>) -> Self {
        Self {
            start,
            selector_node,
            is_object: true,
            next_index: 0,
            pending: None,
            strict_subtree: false,
            phase: jmax_abi::syntax::FramePhase::ObjectStart,
        }
    }

    fn array(start: u32, selector_node: Option<usize>) -> Self {
        Self {
            start,
            selector_node,
            is_object: false,
            next_index: 0,
            pending: None,
            strict_subtree: false,
            phase: jmax_abi::syntax::FramePhase::ArrayStart,
        }
    }
}

/// Sparse query traversal owns only the containers on selected paths. Every
/// other value is skipped before an event/frame or decoded string is built.
fn execute_query_blocks(
    input: &[u8],
    selector: &SelectorTrie,
    options: QueryOptions,
    scratch: &mut QueryScratch,
    matches: &mut [Vec<Span32>],
    skip: &mut jmax_syntax::SkipScratch,
) -> Result<()> {
    use jmax_abi::syntax::{DuplicatePolicyOwner, FramePhase};
    let mut cursor = jmax_syntax::BlockCursor::with_scratch(
        input,
        options.parse.limits,
        Some(options.parse.numeric),
        DuplicatePolicyOwner::Syntax,
        skip,
    )?;
    let mut total = 0;
    query_value(
        &mut cursor,
        Some(0),
        selector,
        options,
        scratch,
        matches,
        &mut total,
    )?;
    while let Some(frame) = scratch.frames.last() {
        let phase = frame.phase;
        let byte = cursor.skip_whitespace()?.ok_or_else(|| {
            path_error(
                ErrorCode::UnexpectedEof,
                cursor.offset(),
                "unfinished query container",
            )
        })?;
        match phase {
            FramePhase::ObjectStart | FramePhase::ObjectKeyOrEnd => {
                if phase == FramePhase::ObjectStart && byte == b'}' {
                    cursor.advance(1);
                    scratch.frames.pop();
                    continue;
                }
                let meta = cursor.scan_key_meta(
                    &mut scratch.decoded,
                    options.parse.limits.reject_duplicate_keys,
                )?;
                let key_bytes = meta
                    .string
                    .cooked
                    .span
                    .slice(match meta.string.cooked.source {
                        super::SpanSource::RawPayload => input,
                        super::SpanSource::DecodedArena => &scratch.decoded,
                    })
                    .ok_or_else(|| internal_error(cursor.offset(), "invalid query key span"))?;
                let key = core::str::from_utf8(key_bytes).map_err(|_| {
                    internal_error(meta.string.raw_token.offset, "invalid query key UTF-8")
                })?;
                let depth = scratch.frames.len() - 1;
                let frame = scratch
                    .frames
                    .last_mut()
                    .ok_or_else(|| internal_error(cursor.offset(), "missing query object"))?;
                frame.pending = frame
                    .selector_node
                    .and_then(|node| selector.child_key(node, key));
                if options.parse.limits.reject_duplicate_keys {
                    scratch.key_tables[depth].insert(meta, input, &scratch.decoded)?;
                }
                frame.next_index = frame
                    .next_index
                    .checked_add(1)
                    .ok_or_else(allocation_error)?;
                if frame.next_index > options.parse.limits.max_fields_per_object {
                    return Err(path_error(
                        ErrorCode::BudgetExhausted,
                        cursor.offset(),
                        "query object field limit exceeded",
                    ));
                }
                cursor.skip_whitespace()?;
                cursor.expect_byte(b':', "expected colon after query key")?;
                frame.phase = FramePhase::ObjectValue;
            }
            FramePhase::ObjectValue => {
                let frame = scratch
                    .frames
                    .last_mut()
                    .ok_or_else(|| internal_error(cursor.offset(), "missing query object"))?;
                let node = frame.pending.take();
                frame.phase = FramePhase::ObjectCommaOrEnd;
                query_value(
                    &mut cursor,
                    node,
                    selector,
                    options,
                    scratch,
                    matches,
                    &mut total,
                )?;
            }
            FramePhase::ArrayStart | FramePhase::ArrayValueOrEnd => {
                if phase == FramePhase::ArrayStart && byte == b']' {
                    cursor.advance(1);
                    scratch.frames.pop();
                    continue;
                }
                let frame = scratch
                    .frames
                    .last_mut()
                    .ok_or_else(|| internal_error(cursor.offset(), "missing query array"))?;
                let node = frame
                    .selector_node
                    .and_then(|node| selector.child_index(node, frame.next_index));
                frame.next_index = frame
                    .next_index
                    .checked_add(1)
                    .ok_or_else(allocation_error)?;
                if frame.next_index > options.parse.limits.max_array_items {
                    return Err(path_error(
                        ErrorCode::BudgetExhausted,
                        cursor.offset(),
                        "query array item limit exceeded",
                    ));
                }
                frame.phase = FramePhase::ArrayCommaOrEnd;
                query_value(
                    &mut cursor,
                    node,
                    selector,
                    options,
                    scratch,
                    matches,
                    &mut total,
                )?;
            }
            FramePhase::ObjectCommaOrEnd | FramePhase::ArrayCommaOrEnd => {
                let object = phase == FramePhase::ObjectCommaOrEnd;
                if byte == if object { b'}' } else { b']' } {
                    cursor.advance(1);
                    scratch.frames.pop();
                } else {
                    cursor.expect_byte(b',', "expected query container comma or end")?;
                    let frame = scratch.frames.last_mut().ok_or_else(|| {
                        internal_error(cursor.offset(), "missing query container")
                    })?;
                    frame.phase = if object {
                        FramePhase::ObjectKeyOrEnd
                    } else {
                        FramePhase::ArrayValueOrEnd
                    };
                }
            }
            _ => {
                return Err(internal_error(
                    cursor.offset(),
                    "invalid query traversal phase",
                ));
            }
        }
    }
    cursor.finish_eof()
}

fn query_value(
    cursor: &mut jmax_syntax::BlockCursor<'_>,
    node: Option<usize>,
    selector: &SelectorTrie,
    options: QueryOptions,
    scratch: &mut QueryScratch,
    matches: &mut [Vec<Span32>],
    total: &mut u32,
) -> Result<()> {
    let byte = cursor.skip_whitespace()?.ok_or_else(|| {
        path_error(
            ErrorCode::UnexpectedEof,
            cursor.offset(),
            "missing query value",
        )
    })?;
    let selected = node.and_then(|index| selector.nodes.get(index));
    let depth = u16::try_from(scratch.frames.len()).map_err(|_| allocation_error())?;
    if selected.is_none_or(|node| node.edges.is_empty()) || !matches!(byte, b'{' | b'[') {
        let mode = if options.mode == QueryMode::Strict
            || selected.is_some_and(|node| !node.slots.is_empty() || !node.edges.is_empty())
        {
            jmax_abi::syntax::SkipMode::StrictValidate
        } else {
            jmax_abi::syntax::SkipMode::QueryOnly
        };
        let span = cursor.skip_value(&mut scratch.decoded, depth, mode)?;
        return add_matches(node, span, selector, matches, total, options.max_matches);
    }
    if depth >= options.parse.limits.max_depth {
        return Err(path_error(
            ErrorCode::DepthLimitExceeded,
            cursor.offset(),
            "query depth limit exceeded",
        ));
    }
    if byte == b'{' && options.parse.limits.reject_duplicate_keys {
        let depth = scratch.frames.len();
        if scratch.key_tables.len() <= depth {
            let additional = depth + 1 - scratch.key_tables.len();
            scratch
                .key_tables
                .try_reserve(additional)
                .map_err(|_| allocation_error())?;
            scratch
                .key_tables
                .resize_with(depth + 1, QueryKeyTable::default);
        }
        scratch.key_tables[depth].reset();
    }
    let offset = cursor.offset();
    cursor.advance(1);
    push_container(
        &mut scratch.frames,
        if byte == b'{' {
            Frame::object(offset, node)
        } else {
            Frame::array(offset, node)
        },
    )
}

fn take_value_node(
    stack: &mut [Frame],
    selector: &SelectorTrie,
    offset: u32,
) -> Result<Option<usize>> {
    let Some(parent) = stack.last_mut() else {
        return Ok(Some(0));
    };
    if parent.is_object {
        return Ok(parent.pending.take());
    }
    let index = parent.next_index;
    parent.next_index = parent.next_index.checked_add(1).ok_or_else(|| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Cursor,
            offset,
            "array index exceeds the supported range",
        )
    })?;
    Ok(parent
        .selector_node
        .and_then(|node| selector.child_index(node, index)))
}

fn push_container(stack: &mut Vec<Frame>, frame: Frame) -> Result<()> {
    stack.try_reserve(1).map_err(|_| allocation_error())?;
    stack.push(frame);
    Ok(())
}

fn completed_span(start: u32, closing_offset: u32) -> Result<Span32> {
    let end = closing_offset.checked_add(1).ok_or_else(|| {
        internal_error(
            closing_offset,
            "container end offset exceeds the 32-bit span range",
        )
    })?;
    let length = end.checked_sub(start).ok_or_else(|| {
        internal_error(closing_offset, "container end precedes its opening offset")
    })?;
    Ok(Span32::new(start, length))
}

fn add_matches(
    node: Option<usize>,
    span: Span32,
    selector: &SelectorTrie,
    output: &mut [Vec<Span32>],
    total: &mut u32,
    maximum: u32,
) -> Result<()> {
    let Some(node) = node.and_then(|index| selector.nodes.get(index)) else {
        return Ok(());
    };
    for &slot in &node.slots {
        if *total >= maximum {
            return Err(Error::new(
                ErrorCode::BudgetExhausted,
                ErrorSource::Cursor,
                span.offset,
                "query match limit exceeded",
            ));
        }
        let matches = output.get_mut(slot).ok_or_else(|| {
            internal_error(
                span.offset,
                "selector terminal refers to a missing result slot",
            )
        })?;
        matches.try_reserve(1).map_err(|_| allocation_error())?;
        matches.push(span);
        *total += 1;
    }
    Ok(())
}

fn allocation_error() -> Error {
    Error::new(
        ErrorCode::BudgetExhausted,
        ErrorSource::Cursor,
        0,
        "query allocation failed",
    )
}

fn path_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Cursor, offset, detail)
}

fn internal_error(offset: u32, detail: &'static str) -> Error {
    Error::new(
        ErrorCode::InternalError,
        ErrorSource::Cursor,
        offset,
        detail,
    )
}
