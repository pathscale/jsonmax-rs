//! Borrowed views over the DOM's immutable preorder tables.
//!
//! Navigation follows immediate-child sibling links, so nested descendants do
//! not affect a container's length or iteration order. No operation allocates
//! or reparses the payload. Internal indexing relies on the layout validated
//! by construction; callers cannot create detached handles or change tables.

use super::*;

/// Returns the sole root value of a complete document.
pub fn root(document: &Document) -> Node<'_> {
    Node { document, id: NodeId(0) }
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
    node.document.metadata[node.id.0 as usize].parent
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
            self.document.metadata[id.0 as usize].next_sibling
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
        let reference = value.document.metadata[value.id.0 as usize].key
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

#[cfg(test)]
mod tests {
    use super::*;

    fn document(input: &[u8]) -> Document {
        build::parse(Payload::from_slice(input), Limits::strict_default()).unwrap()
    }

    fn bytes<'a>(value: Node<'a>) -> &'a [u8] {
        raw_span(value).slice(value.document().payload().as_slice()).unwrap()
    }

    fn ids<'a>(values: impl Iterator<Item = Node<'a>>) -> Vec<NodeId> {
        values.map(Node::id).collect()
    }

    fn assert_exact_fused<I: ExactSizeIterator + core::iter::FusedIterator>(
        iterator: &I,
        remaining: usize,
    ) {
        assert_eq!(iterator.len(), remaining);
        assert_eq!(iterator.size_hint(), (remaining, Some(remaining)));
    }

    fn assert_mismatch<T: core::fmt::Debug>(result: Result<T>, offset: u32) {
        let error = result.unwrap_err();
        assert_eq!(error.code, ErrorCode::TypeMismatch);
        assert_eq!(error.source, ErrorSource::Dom);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn root_kinds_and_raw_spans_cover_every_tag() {
        let cases: &[(&[u8], &[u8], NodeKind)] = &[
            (b" \tnull\r\n", b"null", NodeKind::Null),
            (b" false ", b"false", NodeKind::Bool),
            (b" true ", b"true", NodeKind::Bool),
            (b" -0 ", b"-0", NodeKind::Number),
            (b" 1.2300e+0042 ", b"1.2300e+0042", NodeKind::Number),
            (br#" "a\n\u0062" "#, br#""a\n\u0062""#, NodeKind::String),
            (b" [ 0, {} ] \n", b"[ 0, {} ]", NodeKind::Array),
            (br#" { "a" : [ ] } "#, br#"{ "a" : [ ] }"#, NodeKind::Object),
        ];
        for &(input, expected, expected_kind) in cases {
            let doc = document(input);
            let value = root(&doc);
            assert_eq!(value.id(), NodeId(0));
            assert!(core::ptr::eq(value.document(), &doc));
            assert_eq!(kind(value), expected_kind);
            assert_eq!(bytes(value), expected);
            assert_eq!(raw_span(value).len as usize, expected.len());
            assert!(parent(value).is_none());
            assert_eq!(node(&doc, NodeId(0)).unwrap().id(), value.id());
        }
    }

    #[test]
    fn node_lookup_checks_last_valid_index_and_all_out_of_range_boundaries() {
        let doc = document(br#"[null,[true,false],{}]"#);
        assert_eq!(doc.node_count(), 6);
        for index in 0..doc.node_count() {
            let value = node(&doc, NodeId(index as u32)).unwrap();
            assert_eq!(value.id(), NodeId(index as u32));
            assert!(core::ptr::eq(value.document(), &doc));
        }
        for index in [6, 7, u32::MAX - 1, NO_NODE] {
            assert!(node(&doc, NodeId(index)).is_none());
        }
        let scalar = document(b"null");
        assert!(node(&scalar, NodeId(0)).is_some());
        assert!(node(&scalar, NodeId(1)).is_none());
        assert!(node(&scalar, NodeId(NO_NODE)).is_none());
    }

    #[test]
    fn nested_views_follow_immediate_siblings_and_preserve_parents() {
        let doc = document(br#" {"a":[1,{"b":false},[]],"z":{"x":null},"empty":{}} "#);
        assert_eq!(doc.node_count(), 9);
        let expected_parents = [None, Some(0), Some(1), Some(1), Some(3),
            Some(1), Some(0), Some(6), Some(0)];
        for (index, expected) in expected_parents.into_iter().enumerate() {
            let value = node(&doc, NodeId(index as u32)).unwrap();
            assert_eq!(parent(value).map(Node::id), expected.map(NodeId));
            if let Some(owner) = parent(value) {
                assert!(core::ptr::eq(owner.document(), &doc));
            }
        }
        let obj = object(root(&doc)).unwrap();
        assert_eq!(obj.len(), 3);
        assert!(!obj.is_empty());
        assert_eq!(obj.node().id(), NodeId(0));
        assert_eq!(ids(children(root(&doc)).unwrap()), [NodeId(1), NodeId(6), NodeId(8)]);
        assert_eq!(obj.iter().map(|member| (member.key, member.value.id())).collect::<Vec<_>>(),
            [("a", NodeId(1)), ("z", NodeId(6)), ("empty", NodeId(8))]);

        let elements = array(obj.get("a").unwrap()).unwrap();
        assert_eq!(elements.len(), 3);
        assert!(!elements.is_empty());
        assert_eq!(elements.node().id(), NodeId(1));
        assert_eq!(ids(elements.iter()), [NodeId(2), NodeId(3), NodeId(5)]);
        assert_eq!(ids(children(elements.node()).unwrap()), ids(elements.iter()));
        for (index, expected) in [2, 3, 5].into_iter().enumerate() {
            assert_eq!(elements.get(index).unwrap().id(), NodeId(expected));
        }
        assert!(elements.get(3).is_none());
        assert!(elements.get(4).is_none());
        assert!(elements.get(usize::MAX).is_none());
        let nested = object(elements.get(1).unwrap()).unwrap();
        assert_eq!(nested.len(), 1);
        assert_eq!(nested.get("b").unwrap().id(), NodeId(4));
        assert_eq!(bytes(nested.node()), br#"{"b":false}"#);
        assert_eq!(bytes(elements.node()), br#"[1,{"b":false},[]]"#);
        assert_eq!(bytes(obj.get("z").unwrap()), br#"{"x":null}"#);
        assert_eq!(object(obj.get("z").unwrap()).unwrap().get("x").unwrap().id(), NodeId(7));
        assert!(obj.get("b").is_none());
        assert!(obj.get("x").is_none());
    }

    #[test]
    fn empty_containers_have_exact_fused_empty_views() {
        for input in [b"[]", b"{}"] {
            let doc = document(input);
            let value = root(&doc);
            let mut values = children(value).unwrap();
            assert_exact_fused(&values, 0);
            for _ in 0..3 {
                assert!(values.next().is_none());
                assert_exact_fused(&values, 0);
            }
            match kind(value) {
                NodeKind::Array => {
                    let view = array(value).unwrap();
                    assert!(view.is_empty());
                    assert_eq!(view.len(), 0);
                    assert!(view.get(0).is_none());
                    assert!(view.get(usize::MAX).is_none());
                    assert_exact_fused(&view.iter(), 0);
                }
                NodeKind::Object => {
                    let view = object(value).unwrap();
                    assert!(view.is_empty());
                    assert_eq!(view.len(), 0);
                    assert!(view.get("").is_none());
                    assert!(view.get("missing").is_none());
                    let mut members = view.iter();
                    for _ in 0..3 {
                        assert!(members.next().is_none());
                        assert_exact_fused(&members, 0);
                    }
                }
                other => panic!("unexpected empty container kind: {other:?}"),
            }
        }
        let doc = document(b"[{},[],{}]");
        assert_eq!(ids(array(root(&doc)).unwrap().iter()), [NodeId(1), NodeId(2), NodeId(3)]);
    }

    #[test]
    fn children_iterators_track_remaining_counts_and_clone_independently() {
        let doc = document(b"[0,[1,2],3,{}]");
        let view = array(root(&doc)).unwrap();
        let mut values = view.iter();
        assert_exact_fused(&values, 4);
        assert_eq!(values.next().unwrap().id(), NodeId(1));
        assert_exact_fused(&values, 3);
        let mut cloned = values.clone();
        assert_eq!(values.next().unwrap().id(), NodeId(2));
        assert_exact_fused(&values, 2);
        assert_exact_fused(&cloned, 3);
        assert_eq!(cloned.nth(1).unwrap().id(), NodeId(5));
        assert_exact_fused(&cloned, 1);
        assert_eq!(cloned.next().unwrap().id(), NodeId(6));
        assert_exact_fused(&cloned, 0);
        assert_eq!(ids(values), [NodeId(5), NodeId(6)]);
        for _ in 0..3 {
            assert!(cloned.next().is_none());
            assert_exact_fused(&cloned, 0);
        }
        let mut past_end = view.iter();
        assert!(past_end.nth(usize::MAX).is_none());
        assert_exact_fused(&past_end, 0);
        assert!(past_end.next().is_none());
        assert_eq!(view.iter().count(), 4);
        assert_eq!(view.iter().last().unwrap().id(), NodeId(6));
    }

    #[test]
    fn members_iterators_track_counts_clones_and_exhaustion() {
        let doc = document(br#"{"a":[],"b":{"nested":0},"c":false}"#);
        let view = object(root(&doc)).unwrap();
        let mut members = view.iter();
        assert_exact_fused(&members, 3);
        let first = members.next().unwrap();
        assert_eq!((first.key, first.value.id()), ("a", NodeId(1)));
        assert_exact_fused(&members, 2);
        let cloned = members.clone();
        assert_eq!(members.next().unwrap().key, "b");
        assert_exact_fused(&members, 1);
        assert_eq!(cloned.map(|member| member.key).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(members.next().unwrap().value.id(), NodeId(4));
        assert_exact_fused(&members, 0);
        for _ in 0..3 {
            assert!(members.next().is_none());
            assert_exact_fused(&members, 0);
        }
        let mut past_end = view.iter();
        assert!(past_end.nth(usize::MAX).is_none());
        assert_exact_fused(&past_end, 0);
        assert!(past_end.next().is_none());
        assert_eq!(view.iter().count(), 3);
        assert_eq!(view.iter().last().unwrap().key, "c");
    }

    #[test]
    fn stored_remaining_count_bounds_iteration_before_following_more_links() {
        let doc = document(b"[0,1]");
        assert_eq!(doc.metadata()[1].next_sibling, Some(NodeId(2)));
        // Exercise the count boundary separately from the end-of-chain marker
        // using a bounded prefix of a valid document's sibling chain.
        let mut prefix = Children { document: &doc, next: Some(NodeId(1)), remaining: 1 };
        assert_exact_fused(&prefix, 1);
        assert_eq!(prefix.next().unwrap().id(), NodeId(1));
        assert_exact_fused(&prefix, 0);
        for _ in 0..3 {
            assert!(prefix.next().is_none());
        }
        // A zero-count iterator must never dereference its next field.
        let mut exhausted = Children {
            document: &doc, next: Some(NodeId(NO_NODE)), remaining: 0,
        };
        assert!(exhausted.next().is_none());
        assert_exact_fused(&exhausted, 0);
        assert!(exhausted.next().is_none());
    }

    #[test]
    fn decoded_duplicate_lookup_returns_first_and_iteration_retains_all() {
        for input in [
            &br#"{"a":0,"\u0061":1,"a":2}"#[..],
            &br#"{"\u0061":0,"a":1,"\u0061":2}"#[..],
        ] {
            let limits = Limits { reject_duplicate_keys: false, ..Limits::strict_default() };
            let doc = build::parse(Payload::from_slice(input), limits).unwrap();
            let view = object(root(&doc)).unwrap();
            assert_eq!(view.len(), 3);
            assert_eq!(view.get("a").unwrap().id(), NodeId(1));
            assert_eq!(bytes(view.get("a").unwrap()), b"0");
            assert_eq!(view.iter().map(|member| member.key).collect::<Vec<_>>(), ["a", "a", "a"]);
            assert_eq!(ids(children(view.node()).unwrap()), [NodeId(1), NodeId(2), NodeId(3)]);
            assert!(view.get("A").is_none());
            assert!(view.get(r"\u0061").is_none());
            assert!(view.get("").is_none());
            assert_eq!(view.len(), 3);
        }
    }

    #[test]
    fn keys_compare_cooked_utf8_without_normalization_or_prefix_matching() {
        let input = r#"{"":0,"a\u0000b":1,"line\nfeed":2,"\"\\\/":3,"\uD83D\uDE00":4,"é":5,"e\u0301":6,"ab":7}"#;
        let doc = document(input.as_bytes());
        let view = object(root(&doc)).unwrap();
        let expected = ["", "a\0b", "line\nfeed", "\"\\/", "😀", "é", "e\u{301}", "ab"];
        assert_eq!(view.len(), expected.len());
        for (index, (member, key)) in view.iter().zip(expected).enumerate() {
            assert_eq!(member.key, key);
            assert_eq!(member.value.id(), NodeId(index as u32 + 1));
            assert_eq!(view.get(key).unwrap().id(), member.value.id());
            let reference = doc.metadata()[member.value.id().0 as usize].key.unwrap();
            let stored = doc.resolve_str(reference).unwrap();
            assert_eq!(member.key.as_ptr(), stored.as_ptr());
            assert_eq!(member.key.len(), stored.len());
            assert!(core::ptr::eq(member.value.document(), &doc));
        }
        for missing in ["a", "a\0", "abc", "line", "É", r"\uD83D\uDE00", "😀x"] {
            assert!(view.get(missing).is_none());
        }
        assert_eq!(view.get("é").unwrap().id(), NodeId(6));
        assert_eq!(view.get("e\u{301}").unwrap().id(), NodeId(7));
    }

    #[test]
    fn wrong_views_and_scalar_children_report_absolute_dom_offsets() {
        let cases: &[(&[u8], NodeKind)] = &[
            (b"null", NodeKind::Null), (b"false", NodeKind::Bool),
            (b"true", NodeKind::Bool), (b"-12.5e2", NodeKind::Number),
            (br#""text""#, NodeKind::String), (b"[]", NodeKind::Array),
            (b"{}", NodeKind::Object),
        ];
        for &(input, expected_kind) in cases {
            let mut padded = Vec::from(&b" \n\t"[..]);
            padded.extend_from_slice(input);
            padded.extend_from_slice(b"\r ");
            let doc = document(&padded);
            let value = root(&doc);
            assert_eq!(kind(value), expected_kind);
            assert_eq!(raw_span(value).offset, 3);
            if expected_kind != NodeKind::Object {
                assert_mismatch(object(value), 3);
            }
            if expected_kind != NodeKind::Array {
                assert_mismatch(array(value), 3);
            }
            if !matches!(expected_kind, NodeKind::Array | NodeKind::Object) {
                assert_mismatch(children(value), 3);
            }
        }
        let doc = document(br#"{"x": [null]}"#);
        let value = array(object(root(&doc)).unwrap().get("x").unwrap()).unwrap().get(0).unwrap();
        assert_eq!(raw_span(value), Span32::new(7, 4));
        assert_mismatch(children(value), 7);
        assert_mismatch(object(value), 7);
        assert_mismatch(array(value), 7);
    }

    #[test]
    fn handles_remain_bound_to_the_selected_document_owner() {
        let doc = document(br#"{"\u006b":["\n",{}]}"#);
        let cloned = doc.clone();
        let original_root = root(&doc);
        let cloned_root = root(&cloned);
        assert_eq!(original_root.id(), cloned_root.id());
        assert!(!core::ptr::eq(original_root.document(), cloned_root.document()));
        for (owner, value) in [(&doc, original_root), (&cloned, cloned_root)] {
            let member = object(value).unwrap().iter().next().unwrap();
            assert_eq!(member.key, "k");
            assert!(core::ptr::eq(member.value.document(), owner));
            for child in array(member.value).unwrap().iter() {
                assert!(core::ptr::eq(child.document(), owner));
                assert!(core::ptr::eq(parent(child).unwrap().document(), owner));
            }
        }
        drop(doc);
        let view = object(root(&cloned)).unwrap();
        assert_eq!(view.iter().next().unwrap().key, "k");
        assert_eq!(bytes(array(view.get("k").unwrap()).unwrap().get(0).unwrap()), br#""\n""#);
    }

    #[test]
    fn deep_and_wide_navigation_uses_links_without_recursive_traversal() {
        let depth = 256usize;
        let mut input = Vec::new();
        input.resize(depth, b'[');
        input.push(b'0');
        input.resize(depth * 2 + 1, b']');
        let doc = build::parse(Payload::from_vec(input), Limits {
            max_depth: depth as u16, ..Limits::strict_default()
        }).unwrap();
        let mut current = root(&doc);
        for index in 0..depth {
            assert_eq!(current.id(), NodeId(index as u32));
            let view = array(current).unwrap();
            assert_eq!(view.len(), 1);
            assert_exact_fused(&view.iter(), 1);
            let child = view.get(0).unwrap();
            assert_eq!(parent(child).unwrap().id(), current.id());
            current = child;
        }
        assert_eq!(current.id(), NodeId(depth as u32));
        assert_eq!(kind(current), NodeKind::Number);
        assert_eq!(bytes(current), b"0");
        assert_mismatch(children(current), depth as u32);
        for index in (0..depth).rev() {
            current = parent(current).unwrap();
            assert_eq!(current.id(), NodeId(index as u32));
        }
        assert!(parent(current).is_none());

        let width = 257usize;
        let mut input = Vec::from(&b"["[..]);
        for index in 0..width {
            if index != 0 {
                input.push(b',');
            }
            input.extend_from_slice(b"[]");
        }
        input.push(b']');
        let doc = document(&input);
        let view = array(root(&doc)).unwrap();
        assert_eq!(view.len(), width);
        let mut values = view.iter();
        for index in 0..width {
            assert_exact_fused(&values, width - index);
            let value = values.next().unwrap();
            assert_eq!(value.id(), NodeId(index as u32 + 1));
            assert!(array(value).unwrap().is_empty());
        }
        assert_exact_fused(&values, 0);
        assert!(values.next().is_none());
        assert_eq!(view.get(width - 1).unwrap().id(), NodeId(width as u32));
        assert!(view.get(width).is_none());
        assert!(view.get(usize::MAX).is_none());
    }
}
