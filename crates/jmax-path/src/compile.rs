//! Owned compilation of exact paths into one shared selector trie.

use super::*;

/// Compile paths in input order, assigning one selection slot to each input.
///
/// Common prefixes share nodes, including terminals of repeated paths. Edges
/// retain their first-insertion order and every new node follows its parent.
/// Programmatic paths are checked against the supplied segment and key limits;
/// the textual byte limit applies only when parsing strings.
pub fn compile(paths: &[Path], limits: PathLimits) -> Result<CompiledPaths> {
    let maximum_slots = limits.max_paths.min(jmax_cursor::MAX_SELECTION_SLOTS);
    if paths.len() > maximum_slots as usize {
        return Err(budget("path count limit exceeded"));
    }
    let maximum_nodes = (limits.max_trie_nodes as usize).min(jmax_cursor::MAX_SELECTOR_NODES);
    if maximum_nodes == 0 {
        return Err(budget("selector root exceeds the trie node limit"));
    }

    // Validate all caller-owned paths before allocating any output storage.
    for path in paths {
        if path.segments.len() > limits.max_segments as usize {
            return Err(budget("path segment limit exceeded"));
        }
        for segment in &path.segments {
            if let Segment::Key(key) = segment {
                if key.len() > limits.max_key_bytes as usize {
                    return Err(budget("path key limit exceeded"));
                }
            }
        }
    }

    // Both counts are bounded by the cursor's supported u32 index range.
    let slot_count = u32::try_from(paths.len())
        .map_err(|_| budget("path count exceeds the selector slot range"))?;
    let mut owned_paths = Vec::new();
    reserve_exact(&mut owned_paths, paths.len(), "compiled path list allocation failed")?;
    let mut nodes = Vec::new();
    reserve_one(&mut nodes, maximum_nodes, "selector node allocation failed")?;
    nodes.push(SelectorNode::default());

    for (slot, path) in paths.iter().enumerate() {
        let owned_path = copy_path(path)?;
        let mut current = 0usize;
        for segment in &owned_path.segments {
            let existing = nodes[current].edges.iter()
                .find(|edge| &edge.segment == segment)
                .map(|edge| edge.target);
            if let Some(target) = existing {
                current = target as usize;
                continue;
            }

            // Only a genuinely new prefix consumes another node. In
            // particular, duplicate paths remain legal at the node limit.
            if nodes.len() >= maximum_nodes {
                return Err(budget("selector trie node limit exceeded"));
            }
            let target = u32::try_from(nodes.len())
                .map_err(|_| budget("selector node index exceeds u32"))?;
            let segment = copy_segment(segment)?;
            reserve_one(&mut nodes, maximum_nodes, "selector node allocation failed")?;
            reserve_one(&mut nodes[current].edges, maximum_nodes - 1,
                "selector edge allocation failed")?;
            nodes.push(SelectorNode::default());
            nodes[current].edges.push(SelectorEdge { segment, target });
            current = target as usize;
        }

        reserve_one(&mut nodes[current].slots, paths.len(), "selector terminal allocation failed")?;
        // slot < paths.len() <= MAX_SELECTION_SLOTS, checked above.
        nodes[current].slots.push(slot as u32);
        owned_paths.push(owned_path);
    }

    // Keep the canonical cursor validator as the final authority. Its errors,
    // including fallible validation allocations, propagate without translation.
    let plan = SelectorPlan::new(nodes, slot_count)?;
    Ok(CompiledPaths { paths: owned_paths, plan })
}

/// Parse basic exact-path syntax and compile one shared selector trie.
///
/// Parsing runs once in input order. Parse errors retain their original byte
/// offsets and extension syntax is never enabled by compilation.
pub fn compile_strings(inputs: &[&str], limits: PathLimits) -> Result<CompiledPaths> {
    let paths = super::parse::parse_many(inputs, limits)?;
    compile(&paths, limits)
}

fn budget(detail: &'static str) -> Error {
    path_error(ErrorCode::BudgetExhausted, 0, detail)
}

fn reserve_exact<T>(values: &mut Vec<T>, additional: usize, detail: &'static str) -> Result<()> {
    values.try_reserve_exact(additional).map_err(|_| budget(detail))
}

/// Reserve space before a push, with geometric growth bounded by a logical
/// element limit. The allocator may round the actual capacity up further.
fn reserve_one<T>(values: &mut Vec<T>, maximum: usize, detail: &'static str) -> Result<()> {
    if values.len() >= maximum {
        return Err(budget(detail));
    }
    if values.len() == values.capacity() {
        let target = values.capacity().saturating_mul(2).max(4).min(maximum);
        reserve_exact(values, target - values.len(), detail)?;
    }
    Ok(())
}

fn copy_segment(segment: &Segment) -> Result<Segment> {
    match segment {
        Segment::Key(key) => {
            let mut owned = String::new();
            owned.try_reserve_exact(key.len())
                .map_err(|_| budget("selector key allocation failed"))?;
            owned.push_str(key);
            Ok(Segment::Key(owned))
        }
        Segment::Index(index) => Ok(Segment::Index(*index)),
    }
}

fn copy_path(path: &Path) -> Result<Path> {
    let mut segments = Vec::new();
    reserve_exact(&mut segments, path.segments.len(), "compiled path segment allocation failed")?;
    for segment in &path.segments {
        segments.push(copy_segment(segment)?);
    }
    Ok(Path { segments })
}
