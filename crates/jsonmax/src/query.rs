//! Strict and query-mode path selection over owner-bearing JSON payloads.

use alloc::{string::String, vec::Vec};

use jmax_cursor::{Event, SkipMode};
use jmax_path::{PathLimits, Segment};

use super::{
    CompiledQuery, Error, ErrorCode, ErrorSource, ParseOptions, Payload, QueryMode,
    QueryOptions, QueryResult, Result, Span32,
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
    owned.try_reserve_exact(paths.len()).map_err(|_| allocation_error())?;
    for path in paths {
        // The native parser supplies the canonical grammar and error offsets.
        let _parsed = jmax_path::parse::parse(path, limits)?;
        let mut copy = String::new();
        copy.try_reserve_exact(path.len()).map_err(|_| allocation_error())?;
        copy.push_str(path);
        owned.push(copy);
    }
    Ok(CompiledQuery { paths: owned })
}

/// Selects complete raw JSON values while retaining the input payload.
///
/// The syntax and numeric policy are checked over the complete payload before
/// selection. This gives strict mode its complete-document guarantee and makes
/// query mode conservative when malformed input occurs outside a selected
/// value; query mode never advertises that scan as strict validation.
pub fn query_payload(
    input: Payload,
    query: &CompiledQuery,
    options: QueryOptions,
) -> Result<QueryResult> {
    if options.max_matches == 0 {
        return Err(path_error(
            ErrorCode::ConstraintViolation,
            0,
            "query match limit must be positive",
        ));
    }

    // This performs whole-input syntax, UTF-8, duplicate-key, numeric, and
    // configured resource validation before any result can be returned.
    super::parse::parse_payload(input.clone(), options.parse)?;

    let selector = SelectorTrie::compile(query.paths())?;
    let mut matches = Vec::new();
    matches
        .try_reserve_exact(query.paths().len())
        .map_err(|_| allocation_error())?;
    for _ in query.paths() {
        matches.push(Vec::new());
    }

    let mode = match options.mode {
        QueryMode::Strict => SkipMode::Strict,
        QueryMode::Query => SkipMode::Query,
    };
    let mut cursor = jmax_cursor::walk::from_payload(
        input.clone(),
        options.parse.limits,
        mode,
    )?;
    let mut stack: Vec<Frame> = Vec::new();
    let mut total_matches = 0u32;

    loop {
        match jmax_cursor::walk::next(&mut cursor)? {
            jmax_abi::syntax::ScanStep::Emit(event) => match event {
                Event::Key { str_ref } => {
                    let frame = stack.last_mut().ok_or_else(|| internal_error(
                        str_ref.span.offset,
                        "cursor emitted an object key without a container",
                    ))?;
                    if !frame.is_object {
                        return Err(internal_error(
                            str_ref.span.offset,
                            "cursor emitted an object key inside an array",
                        ));
                    }
                    let key = cursor.resolve_str(str_ref).ok_or_else(|| internal_error(
                        str_ref.span.offset,
                        "cursor emitted an invalid decoded object key",
                    ))?;
                    frame.pending = frame
                        .selector_node
                        .and_then(|node| selector.child_key(node, key));
                }
                Event::ObjectStart { offset } => {
                    let node = take_value_node(&mut stack, &selector, offset)?;
                    push_container(&mut stack, Frame::object(offset, node))?;
                }
                Event::ArrayStart { offset } => {
                    let node = take_value_node(&mut stack, &selector, offset)?;
                    push_container(&mut stack, Frame::array(offset, node))?;
                }
                Event::ObjectEnd { offset } => {
                    let frame = stack.pop().ok_or_else(|| internal_error(
                        offset,
                        "cursor closed an object without an open container",
                    ))?;
                    if !frame.is_object {
                        return Err(internal_error(offset, "cursor container kinds are unbalanced"));
                    }
                    let span = completed_span(frame.start, offset)?;
                    add_matches(
                        frame.selector_node,
                        span,
                        &selector,
                        &mut matches,
                        &mut total_matches,
                        options.max_matches,
                    )?;
                }
                Event::ArrayEnd { offset } => {
                    let frame = stack.pop().ok_or_else(|| internal_error(
                        offset,
                        "cursor closed an array without an open container",
                    ))?;
                    if frame.is_object {
                        return Err(internal_error(offset, "cursor container kinds are unbalanced"));
                    }
                    let span = completed_span(frame.start, offset)?;
                    add_matches(
                        frame.selector_node,
                        span,
                        &selector,
                        &mut matches,
                        &mut total_matches,
                        options.max_matches,
                    )?;
                }
                Event::Scalar { kind: _, span, .. } => {
                    let node = take_value_node(&mut stack, &selector, span.offset)?;
                    add_matches(
                        node,
                        span,
                        &selector,
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

    Ok(QueryResult {
        payload: input,
        matches,
        fully_validated: options.mode == QueryMode::Strict,
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
struct SelectorTrie {
    nodes: Vec<SelectorNode>,
}

impl SelectorTrie {
    fn compile(paths: &[String]) -> Result<Self> {
        let limits = PathLimits::default();
        let mut trie = Self { nodes: Vec::new() };
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
                            owned.try_reserve_exact(key.len()).map_err(|_| allocation_error())?;
                            owned.push_str(key);
                            Segment::Key(owned)
                        }
                        Segment::Index(index) => Segment::Index(*index),
                    };
                    let parent = trie.nodes.get_mut(node_index).ok_or_else(|| internal_error(
                        0,
                        "selector trie parent is outside its node table",
                    ))?;
                    parent.edges.try_reserve(1).map_err(|_| allocation_error())?;
                    parent.edges.push((edge_segment, target));
                    trie.nodes.push(SelectorNode::default());
                    target
                };
            }
            let terminal = trie.nodes.get_mut(node_index).ok_or_else(|| internal_error(
                0,
                "selector trie terminal is outside its node table",
            ))?;
            terminal.slots.try_reserve(1).map_err(|_| allocation_error())?;
            terminal.slots.push(slot);
        }
        Ok(trie)
    }

    fn child_key(&self, node: usize, key: &str) -> Option<usize> {
        self.nodes.get(node)?.edges.iter().find_map(|(segment, target)| {
            match segment {
                Segment::Key(candidate) if candidate == key => Some(*target),
                _ => None,
            }
        })
    }

    fn child_index(&self, node: usize, index: u32) -> Option<usize> {
        self.nodes.get(node)?.edges.iter().find_map(|(segment, target)| {
            match segment {
                Segment::Index(candidate) if *candidate == index => Some(*target),
                _ => None,
            }
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
}

impl Frame {
    fn object(start: u32, selector_node: Option<usize>) -> Self {
        Self { start, selector_node, is_object: true, next_index: 0, pending: None }
    }

    fn array(start: u32, selector_node: Option<usize>) -> Self {
        Self { start, selector_node, is_object: false, next_index: 0, pending: None }
    }
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
    parent.next_index = parent.next_index.checked_add(1).ok_or_else(|| Error::new(
        ErrorCode::BudgetExhausted,
        ErrorSource::Cursor,
        offset,
        "array index exceeds the supported range",
    ))?;
    Ok(parent.selector_node.and_then(|node| selector.child_index(node, index)))
}

fn push_container(stack: &mut Vec<Frame>, frame: Frame) -> Result<()> {
    stack.try_reserve(1).map_err(|_| allocation_error())?;
    stack.push(frame);
    Ok(())
}

fn completed_span(start: u32, closing_offset: u32) -> Result<Span32> {
    let end = closing_offset.checked_add(1).ok_or_else(|| internal_error(
        closing_offset,
        "container end offset exceeds the 32-bit span range",
    ))?;
    let length = end.checked_sub(start).ok_or_else(|| internal_error(
        closing_offset,
        "container end precedes its opening offset",
    ))?;
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
        let matches = output.get_mut(slot).ok_or_else(|| internal_error(
            span.offset,
            "selector terminal refers to a missing result slot",
        ))?;
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
    Error::new(ErrorCode::InternalError, ErrorSource::Cursor, offset, detail)
}
