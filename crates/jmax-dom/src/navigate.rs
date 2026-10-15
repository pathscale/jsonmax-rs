//! Borrowed views over the DOM's immutable preorder tables.
//!
//! Navigation follows immediate-child sibling links, so nested descendants do
//! not affect a container's length or iteration order. No operation allocates
//! or reparses the payload. Internal indexing relies on the layout validated
//! by construction. Callers changing public ABI tables must preserve the layout
//! invariants or revalidate with `Document::from_validated` before navigation.

use super::*;

/// Returns the sole root value of a complete document.
pub fn root(document: &Document) -> Node<'_> {
    Node { document, id: document.root() }
}

/// Resolves an index in this document. The sentinel is never a node ID.
pub fn node(document: &Document, id: NodeId) -> Option<Node<'_>> {
    if id.0 == NO_NODE {
        return None;
    }
    let index = usize::try_from(id.0).ok()?;
    document.parts.nodes.get(index)?;
    Some(Node { document, id })
}

/// Returns the value kind, with both boolean tags represented as `Bool`.
pub fn kind(node: Node<'_>) -> NodeKind {
    NodeKind::from_tag(node.document.parts.nodes[node.id.0 as usize].kind())
        .expect("validated DOM nodes have known tags")
}

/// Returns the complete raw value span, including quotes or delimiters.
pub fn raw_span(node: Node<'_>) -> Span32 {
    // Container payload indices address container records, not raw spans.
    // Every raw span is parallel to its node, regardless of the node kind.
    node.document.parts.spans[node.id.0 as usize]
}

/// Returns the immediate containing value, or `None` for the root.
pub fn parent<'a>(node: Node<'a>) -> Option<Node<'a>> {
    node.document.parts.node_metadata[node.id.0 as usize].parent
        .map(|id| Node { document: node.document, id })
}

/// Iterates immediate values of an array or object in document order.
///
/// Object keys are metadata on value nodes and are not separate children.
/// Scalars return `TypeMismatch` at the raw value's starting byte.
pub fn children<'a>(node: Node<'a>) -> Result<Children<'a>> {
    match kind(node) {
        NodeKind::Array | NodeKind::Object => Ok(container_children(node)),
        _ => Err(dom_error(ErrorCode::TypeMismatch, raw_span(node).offset,
            "DOM children require an array or object")),
    }
}

/// Converts an object value to a borrowed member view.
pub fn object<'a>(node: Node<'a>) -> Result<Object<'a>> {
    if kind(node) == NodeKind::Object {
        Ok(Object { node })
    } else {
        Err(dom_error(ErrorCode::TypeMismatch, raw_span(node).offset,
            "DOM object view requires an object"))
    }
}

/// Converts an array value to a borrowed element view.
pub fn array<'a>(node: Node<'a>) -> Result<Array<'a>> {
    if kind(node) == NodeKind::Array {
        Ok(Array { node })
    } else {
        Err(dom_error(ErrorCode::TypeMismatch, raw_span(node).offset,
            "DOM array view requires an array"))
    }
}

/// Called only with a container handle obtained through a checked view or kind.
fn container_record<'a>(node: Node<'a>) -> &'a Container32 {
    let index = node.document.parts.nodes[node.id.0 as usize].payload_index();
    &node.document.parts.containers[index as usize]
}

fn container_children<'a>(node: Node<'a>) -> Children<'a> {
    let record = container_record(node);
    Children {
        document: node.document,
        next: if record.child_count == 0 { None } else { Some(NodeId(record.first_child)) },
        remaining: record.child_count,
    }
}

impl<'a> Object<'a> {
    /// Returns the number of immediate members, including allowed duplicates.
    pub fn len(self) -> usize {
        container_record(self.node).child_count as usize
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Finds the first member with exactly these decoded UTF-8 bytes.
    ///
    /// Lookup is linear in the member count. It neither normalizes Unicode nor
    /// treats JSON escape spellings as part of the cooked member name.
    pub fn get(self, key: &str) -> Option<Node<'a>> {
        self.iter().find(|member| member.key == key).map(|member| member.value)
    }

    /// Iterates all members, preserving their order and any duplicate names.
    pub fn iter(self) -> Members<'a> {
        Members { children: container_children(self.node) }
    }
}

impl<'a> Array<'a> {
    /// Returns the number of immediate elements, excluding descendants.
    pub fn len(self) -> usize {
        container_record(self.node).child_count as usize
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Finds a zero-based element by following sibling links in linear time.
    pub fn get(self, index: usize) -> Option<Node<'a>> {
        // Reject even usize::MAX before traversing or doing index arithmetic.
        if index >= self.len() {
            return None;
        }
        self.iter().nth(index)
    }

    pub fn iter(self) -> Children<'a> {
        container_children(self.node)
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            self.next = None;
            return None;
        }
        let id = self.next.expect("validated DOM child links cover the child count");
        self.remaining -= 1;
        // The count bounds the walk independently of the final sibling link.
        self.next = if self.remaining == 0 {
            None
        } else {
            self.document.parts.node_metadata[id.0 as usize].next_sibling
        };
        Some(Node { document: self.document, id })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining as usize;
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for Children<'a> {}
impl<'a> core::iter::FusedIterator for Children<'a> {}

impl<'a> Iterator for Members<'a> {
    type Item = Member<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let value = self.children.next()?;
        let reference = value.document.parts.node_metadata[value.id.0 as usize].key
            .expect("validated DOM object children have member keys");
        let key = value.document.resolve_str(reference)
            .expect("validated DOM member keys resolve in their document");
        Some(Member { key, value })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.children.size_hint()
    }
}

impl<'a> ExactSizeIterator for Members<'a> {}
impl<'a> core::iter::FusedIterator for Members<'a> {}
