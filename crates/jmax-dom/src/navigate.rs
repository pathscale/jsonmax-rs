//! Navigation over variable-width tape records. Sibling jumps never walk a subtree.
use super::*;

#[inline]
pub fn root(document: &(impl TapeSource + ?Sized)) -> Node<'_> {
    Node {
        document: document.as_tape(),
        id: NodeId(0),
    }
}

/// Resolves an arbitrary public integer ID, rejecting extension words and keys.
/// Membership checking walks record headers; handle-based navigation is O(1).
/// The frozen tape has no per-word header bitmap, so arbitrary-ID membership
/// cannot in general be decided from one candidate word alone.
#[inline]
pub fn node(document: &(impl TapeSource + ?Sized), id: NodeId) -> Option<Node<'_>> {
    node_in_view(document.as_tape(), id)
}
#[inline]
pub(crate) fn node_in_view(document: TapeView<'_>, id: NodeId) -> Option<Node<'_>> {
    let mut cursor = 0usize;
    let target = id.0 as usize;
    if target >= document.tape().len() {
        return None;
    }
    while cursor <= target {
        let header = document.tape()[cursor];
        if cursor == target {
            return (header.tag() <= TAG_OBJECT).then_some(Node { document, id });
        }
        cursor += header.record_words();
    }
    None
}

#[inline]
pub fn kind(node: Node<'_>) -> NodeKind {
    // Genuine handles are only created at value record boundaries.
    match node.document.tape()[node.id.0 as usize].tag() {
        TAG_NULL => NodeKind::Null,
        TAG_FALSE | TAG_TRUE => NodeKind::Bool,
        TAG_NUMBER => NodeKind::Number,
        TAG_STRING => NodeKind::String,
        TAG_ARRAY => NodeKind::Array,
        _ => NodeKind::Object,
    }
}

#[inline]
pub fn raw_span(node: Node<'_>) -> Span32 {
    record_span(node.document, node.id.0 as usize)
}
#[inline]
pub(crate) fn record_span(view: TapeView<'_>, index: usize) -> Span32 {
    let word = view.tape()[index];
    let offset = word.raw_offset();
    let len = match word.tag() {
        TAG_ARRAY | TAG_OBJECT => view.tape()[index + 2].high32() - offset,
        TAG_STRING | TAPE_TAG_KEY if word.has_flag(TAPE_FLAG_EXTENDED) => {
            let extension = if word.tag() == TAPE_TAG_KEY { 2 } else { 3 };
            view.tape()[index + extension].high32()
        }
        TAG_STRING | TAPE_TAG_KEY => word.compact_len() + 2,
        TAG_NUMBER if word.has_flag(TAPE_FLAG_EXTENDED) => view.tape()[index + 2].low32(),
        _ => word.compact_len(),
    };
    Span32::new(offset, len)
}

#[inline]
pub fn parent(node: Node<'_>) -> Option<Node<'_>> {
    let index = node.document.tape()[node.id.0 as usize + 1].low32();
    (index != NO_NODE).then_some(Node {
        document: node.document,
        id: NodeId(index),
    })
}
#[inline]
pub fn subtree_end(node: Node<'_>) -> u32 {
    node.document.tape()[node.id.0 as usize + 1].high32()
}
#[inline]
pub fn children(node: Node<'_>) -> Result<Children<'_>> {
    match kind(node) {
        NodeKind::Array | NodeKind::Object => Ok(container_children(node)),
        _ => Err(dom_error(
            ErrorCode::TypeMismatch,
            raw_span(node).offset,
            "DOM children require an array or object",
        )),
    }
}
#[inline]
pub fn object(node: Node<'_>) -> Result<Object<'_>> {
    if kind(node) == NodeKind::Object {
        Ok(Object { node })
    } else {
        Err(dom_error(
            ErrorCode::TypeMismatch,
            raw_span(node).offset,
            "DOM object view requires an object",
        ))
    }
}
#[inline]
pub fn array(node: Node<'_>) -> Result<Array<'_>> {
    if kind(node) == NodeKind::Array {
        Ok(Array { node })
    } else {
        Err(dom_error(
            ErrorCode::TypeMismatch,
            raw_span(node).offset,
            "DOM array view requires an array",
        ))
    }
}
fn child_count(node: Node<'_>) -> u32 {
    node.document.tape()[node.id.0 as usize + 2].low32()
}
fn container_children(node: Node<'_>) -> Children<'_> {
    let remaining = child_count(node);
    let object = kind(node) == NodeKind::Object;
    let first = node.id.0 + 3;
    let next = if remaining == 0 {
        None
    } else {
        Some(NodeId(if object {
            first + node.document.tape()[first as usize].record_words() as u32
        } else {
            first
        }))
    };
    Children {
        document: node.document,
        next,
        remaining,
        object,
    }
}

#[inline]
pub fn string_ref(node: Node<'_>) -> Option<StrRef> {
    (kind(node) == NodeKind::String).then(|| record_string_ref(node.document, node.id.0 as usize))
}
#[inline]
pub(crate) fn record_string_ref(view: TapeView<'_>, index: usize) -> StrRef {
    let header = view.tape()[index];
    if header.has_flag(TAPE_FLAG_EXTENDED) {
        let extension = if header.tag() == TAPE_TAG_KEY {
            index + 1
        } else {
            index + 2
        };
        let cooked = view.tape()[extension];
        StrRef {
            span: Span32::new(cooked.low32(), cooked.high32()),
            scalar_len: view.tape()[extension + 1].low32(),
            source: if header.has_flag(TAPE_FLAG_ESCAPED) {
                SpanSource::DecodedArena
            } else {
                SpanSource::RawPayload
            },
        }
    } else {
        let len = header.compact_len();
        StrRef {
            span: Span32::new(header.raw_offset() + 1, len),
            source: SpanSource::RawPayload,
            scalar_len: len,
        }
    }
}
fn key_index(node: Node<'_>) -> Option<usize> {
    let index = node.id.0 as usize;
    let header = node.document.tape()[index];
    if !header.has_flag(TAPE_FLAG_HAS_KEY) {
        return None;
    }
    Some(
        index
            - if header.has_flag(TAPE_FLAG_KEY_EXTENDED) {
                3
            } else {
                1
            },
    )
}
#[inline]
pub fn key_ref(node: Node<'_>) -> Option<StrRef> {
    Some(record_string_ref(node.document, key_index(node)?))
}
#[inline]
pub fn key_span(node: Node<'_>) -> Option<Span32> {
    Some(record_span(node.document, key_index(node)?))
}
/// Validated, already quoted member key, when its source has no escapes.
/// This borrows the original token, including quotes, without scanning bytes.
#[inline(always)]
#[inline]
pub fn unescaped_key_token<'a>(node: Node<'a>) -> Option<&'a [u8]> {
    unescaped_token(node.document, key_index(node)?)
}
/// Validated, already quoted string value, when its source has no escapes.
#[inline(always)]
#[inline]
pub fn unescaped_string_token<'a>(node: Node<'a>) -> Option<&'a [u8]> {
    if node.header().tag() != TAG_STRING {
        return None;
    }
    unescaped_token(node.document, node.id.0 as usize)
}
#[inline(always)]
fn unescaped_token<'a>(view: TapeView<'a>, index: usize) -> Option<&'a [u8]> {
    if view.tape()[index].has_flag(TAPE_FLAG_ESCAPED) {
        return None;
    }
    let span = record_span(view, index);
    let start = span.offset as usize;
    Some(&view.raw()[start..start + span.len as usize])
}
#[inline]
pub fn node_key_ref(document: &(impl TapeSource + ?Sized), id: NodeId) -> Option<StrRef> {
    node(document, id).and_then(key_ref)
}
#[inline]
pub fn node_key_span(document: &(impl TapeSource + ?Sized), id: NodeId) -> Option<Span32> {
    node(document, id).and_then(key_span)
}

impl<'a> Object<'a> {
    pub fn len(self) -> usize {
        child_count(self.node) as usize
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    /// Finds the first matching decoded key; O(members), O(1) per step.
    pub fn get(self, key: &str) -> Option<Node<'a>> {
        self.iter()
            .find(|member| member.key == key)
            .map(|member| member.value)
    }
    pub fn iter(self) -> Members<'a> {
        Members {
            children: container_children(self.node),
        }
    }
}
impl<'a> Array<'a> {
    pub fn len(self) -> usize {
        child_count(self.node) as usize
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    /// O(index) immediate sibling jumps; descendants are never visited.
    pub fn get(self, index: usize) -> Option<Node<'a>> {
        if index >= self.len() {
            None
        } else {
            self.iter().nth(index)
        }
    }
    pub fn iter(self) -> Children<'a> {
        container_children(self.node)
    }
}
impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let id = self.next?;
        self.remaining -= 1;
        self.next = if self.remaining == 0 {
            None
        } else {
            let cursor = self.document.tape()[id.0 as usize + 1].high32();
            Some(NodeId(if self.object {
                cursor + self.document.tape()[cursor as usize].record_words() as u32
            } else {
                cursor
            }))
        };
        Some(Node {
            document: self.document,
            id,
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining as usize, Some(self.remaining as usize))
    }
}
impl ExactSizeIterator for Children<'_> {}
impl core::iter::FusedIterator for Children<'_> {}
impl<'a> Iterator for Members<'a> {
    type Item = Member<'a>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let value = self.children.next()?;
        let reference = key_ref(value)?;
        Some(Member {
            key: value.document.resolve_record_str(reference),
            value,
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.children.size_hint()
    }
}
impl ExactSizeIterator for Members<'_> {}
impl core::iter::FusedIterator for Members<'_> {}
