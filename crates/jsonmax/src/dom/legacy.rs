//! Explicit conversion into the public, parallel-table compatibility ABI.
use super::*;

struct Frame {
    record: usize,
    node: NodeId,
    end: u32,
    last: Option<NodeId>,
}
fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<()> {
    values.try_reserve_exact(additional).map_err(|_| {
        dom_error(
            ErrorCode::BudgetExhausted,
            0,
            "DOM legacy conversion allocation failed",
        )
    })
}
pub(crate) fn parts(view: TapeView<'_>, copy_decoded: bool) -> Result<DocumentParts> {
    let mut parts = DocumentParts {
        max_depth: view.max_depth(),
        ..DocumentParts::default()
    };
    reserve(&mut parts.nodes, view.node_count())?;
    reserve(&mut parts.spans, view.node_count())?;
    reserve(&mut parts.node_metadata, view.node_count())?;
    reserve(&mut parts.containers, view.container_count())?;
    if copy_decoded {
        reserve(&mut parts.decoded, view.decoded().len())?;
        parts.decoded.extend_from_slice(view.decoded());
    }
    let mut stack: Vec<Frame> = Vec::new();
    reserve(&mut stack, view.max_depth() as usize)?;
    let mut cursor = 0usize;
    let mut key = None;
    while cursor < view.tape().len() {
        // Pop at the subtree boundary before consuming a following key record.
        while stack
            .last()
            .is_some_and(|frame| frame.end as usize == cursor)
        {
            if let Some(frame) = stack.pop() {
                parts.containers[frame.record].end_node = parts.nodes.len() as u32;
            }
        }
        let header = view.tape()[cursor];
        if header.tag() == TAPE_TAG_KEY {
            key = Some((
                navigate::record_string_ref(view, cursor),
                navigate::record_span(view, cursor),
            ));
            cursor += header.record_words();
            continue;
        }
        let id = NodeId(parts.nodes.len() as u32);
        let parent = stack.last().map(|frame| frame.node);
        if let Some(frame) = stack.last_mut() {
            if let Some(last) = frame.last {
                parts.node_metadata[last.0 as usize].next_sibling = Some(id);
            }
            frame.last = Some(id);
        }
        let container = matches!(header.tag(), TAG_ARRAY | TAG_OBJECT);
        let payload_index = if container {
            parts.containers.len() as u32
        } else {
            id.0
        };
        parts
            .nodes
            .push(NodeWord::pack(header.tag(), 0, payload_index));
        parts.spans.push(navigate::record_span(view, cursor));
        parts.node_metadata.push(NodeMeta {
            parent,
            next_sibling: None,
            key: key.map(|entry| entry.0),
            key_span: key.map(|entry| entry.1),
            string: (header.tag() == TAG_STRING).then(|| navigate::record_string_ref(view, cursor)),
            number_index: None,
        });
        key = None;
        if container {
            let count = view.tape()[cursor + 2].low32();
            parts.containers.push(Container32 {
                first_child: if count == 0 { NO_NODE } else { id.0 + 1 },
                child_count: count,
                end_node: 0,
                reserved: 0,
            });
            stack.push(Frame {
                record: payload_index as usize,
                node: id,
                end: view.tape()[cursor + 1].high32(),
                last: None,
            });
        }
        cursor += header.record_words();
    }
    while let Some(frame) = stack.pop() {
        if frame.end as usize != cursor {
            return Err(dom_error(
                ErrorCode::InternalError,
                0,
                "DOM legacy conversion encountered an invalid subtree end",
            ));
        }
        parts.containers[frame.record].end_node = parts.nodes.len() as u32;
    }
    Ok(parts)
}
