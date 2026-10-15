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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn key(text: &str) -> Segment {
        Segment::Key(String::from(text))
    }

    fn path(segments: Vec<Segment>) -> Path {
        Path::from_segments(segments, PathLimits::default()).unwrap()
    }

    fn edge(segment: Segment, target: u32) -> SelectorEdge {
        SelectorEdge { segment, target }
    }

    fn assert_budget(error: Error) {
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.source, ErrorSource::Cursor);
        assert_eq!(error.byte_offset, 0);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_invariants(compiled: &CompiledPaths) {
        let nodes = compiled.plan().nodes();
        assert!(!nodes.is_empty());
        assert_eq!(compiled.plan().slot_count() as usize, compiled.paths().len());
        let mut incoming = vec![0usize; nodes.len()];
        let mut slots = vec![0usize; compiled.paths().len()];
        for (parent, node) in nodes.iter().enumerate() {
            for (index, edge) in node.edges.iter().enumerate() {
                assert!(edge.target as usize > parent);
                assert!((edge.target as usize) < nodes.len());
                incoming[edge.target as usize] += 1;
                assert!(node.edges[..index].iter().all(|other| other.segment != edge.segment));
            }
            assert!(node.slots.windows(2).all(|pair| pair[0] < pair[1]));
            for &slot in &node.slots {
                assert!((slot as usize) < slots.len());
                slots[slot as usize] += 1;
            }
        }
        assert_eq!(incoming[0], 0);
        assert!(incoming[1..].iter().all(|count| *count == 1));
        assert!(slots.iter().all(|count| *count == 1));
        for (slot, path) in compiled.paths().iter().enumerate() {
            let mut current = 0;
            for segment in path.segments() {
                current = nodes[current].edges.iter()
                    .find(|edge| &edge.segment == segment).unwrap().target as usize;
            }
            assert!(nodes[current].slots.contains(&(slot as u32)));
        }
    }

    #[test]
    fn no_inputs_still_produce_one_root_with_no_slots() {
        let limits = PathLimits {
            max_path_bytes: 0, max_segments: 0, max_key_bytes: 0, max_paths: 0,
            max_trie_nodes: 1, max_matches: 0, max_visits: 0,
        };
        for compiled in [compile(&[], limits).unwrap(), compile_strings(&[], limits).unwrap()] {
            assert!(compiled.paths().is_empty());
            assert_eq!(compiled.plan().slot_count(), 0);
            assert_eq!(compiled.plan().nodes(), &[SelectorNode::default()]);
            assert_invariants(&compiled);
        }
    }

    #[test]
    fn zero_node_budget_rejects_even_an_empty_input_list() {
        let limits = PathLimits { max_trie_nodes: 0, ..PathLimits::default() };
        assert_budget(compile(&[], limits).unwrap_err());
        assert_budget(compile(&[Path::root()], limits).unwrap_err());
        assert_budget(compile_strings(&[], limits).unwrap_err());
    }

    #[test]
    fn root_aliases_and_repetitions_keep_distinct_slots() {
        let limits = PathLimits {
            max_trie_nodes: 1, max_segments: 0, max_key_bytes: 0,
            ..PathLimits::default()
        };
        let compiled = compile_strings(&["", "$", "", "$"], limits).unwrap();
        assert_eq!(compiled.paths(), &[Path::root(), Path::root(), Path::root(), Path::root()]);
        assert_eq!(compiled.plan().nodes(), &[SelectorNode {
            edges: vec![], slots: vec![0, 1, 2, 3],
        }]);
        assert_invariants(&compiled);
    }

    #[test]
    fn shared_prefixes_have_exact_insertion_order_and_terminals() {
        let compiled = compile_strings(
            &["book[2].price", "book[2].title", "book", "book[2].price", "", "other", "book[0]"],
            PathLimits::default(),
        ).unwrap();
        assert_eq!(compiled.plan().nodes(), &[
            SelectorNode { edges: vec![edge(key("book"), 1), edge(key("other"), 5)], slots: vec![4] },
            SelectorNode { edges: vec![edge(Segment::Index(2), 2), edge(Segment::Index(0), 6)], slots: vec![2] },
            SelectorNode { edges: vec![edge(key("price"), 3), edge(key("title"), 4)], slots: vec![] },
            SelectorNode { edges: vec![], slots: vec![0, 3] },
            SelectorNode { edges: vec![], slots: vec![1] },
            SelectorNode { edges: vec![], slots: vec![5] },
            SelectorNode { edges: vec![], slots: vec![6] },
        ]);
        assert_invariants(&compiled);
    }

    #[test]
    fn root_and_prefix_terminals_can_precede_descendants() {
        let compiled = compile_strings(&["$", "a", "a.b", "a.b[0]", "a"], PathLimits::default()).unwrap();
        let nodes = compiled.plan().nodes();
        assert_eq!(nodes.len(), 4);
        assert_eq!(nodes[0].slots, &[0]);
        assert_eq!(nodes[1].slots, &[1, 4]);
        assert_eq!(nodes[2].slots, &[2]);
        assert_eq!(nodes[3].slots, &[3]);
        assert_invariants(&compiled);
    }

    #[test]
    fn sibling_edges_preserve_input_order_instead_of_segment_sort_order() {
        let compiled = compile_strings(
            &["[9]", "z", "[0]", "a", "[4294967295]", "[1]", "z"], PathLimits::default(),
        ).unwrap();
        assert_eq!(compiled.plan().nodes()[0].edges, &[
            edge(Segment::Index(9), 1), edge(key("z"), 2), edge(Segment::Index(0), 3),
            edge(key("a"), 4), edge(Segment::Index(u32::MAX), 5), edge(Segment::Index(1), 6),
        ]);
        assert_eq!(compiled.plan().nodes()[2].slots, &[1, 6]);
        assert_invariants(&compiled);
    }

    #[test]
    fn identical_suffixes_under_different_parents_remain_separate() {
        let compiled = compile_strings(&["a.tail[0]", "b.tail[0]"], PathLimits::default()).unwrap();
        assert_eq!(compiled.plan().nodes().len(), 7);
        assert_eq!(compiled.plan().nodes()[3].slots, &[0]);
        assert_eq!(compiled.plan().nodes()[6].slots, &[1]);
        assert_invariants(&compiled);
    }

    #[test]
    fn equivalent_escape_spellings_share_decoded_edges_without_normalization() {
        let compiled = compile_strings(&[
            "é", r#"["\u00e9"]"#, "e\u{0301}", r"a\.b", r#"["a.b"]"#,
            "😀", r#"["\uD83D\uDE00"]"#,
        ], PathLimits::default()).unwrap();
        assert_eq!(compiled.plan().nodes()[0].edges, &[
            edge(key("é"), 1), edge(key("e\u{0301}"), 2), edge(key("a.b"), 3), edge(key("😀"), 4),
        ]);
        assert_eq!(compiled.plan().nodes()[1].slots, &[0, 1]);
        assert_eq!(compiled.plan().nodes()[3].slots, &[3, 4]);
        assert_eq!(compiled.plan().nodes()[4].slots, &[5, 6]);
        assert_invariants(&compiled);
    }

    #[test]
    fn keys_and_indices_are_distinct_and_programmatic_keys_are_literal() {
        let paths = [
            path(vec![key("0")]), path(vec![Segment::Index(0)]),
            path(vec![key("")]), path(vec![key("*")]), path(vec![key("?")]),
            path(vec![key("\0\n.$[]\\")]), path(vec![Segment::Index(u32::MAX)]),
        ];
        let compiled = compile(&paths, PathLimits::default()).unwrap();
        assert_eq!(compiled.paths(), &paths);
        assert_eq!(compiled.plan().nodes().len(), paths.len() + 1);
        for (slot, expected) in paths.iter().enumerate() {
            assert_eq!(compiled.plan().nodes()[0].edges[slot].segment, expected.segments()[0]);
        }
        assert_invariants(&compiled);
    }

    #[test]
    fn compilation_owns_independent_path_and_selector_key_copies() {
        let mut inputs = vec![path(vec![key("original"), key("child")])];
        let compiled = compile(&inputs, PathLimits::default()).unwrap();
        let Segment::Key(input_key) = &mut inputs[0].segments[0] else { panic!("expected key") };
        let Segment::Key(path_key) = &compiled.paths()[0].segments()[0] else { panic!("expected key") };
        let Segment::Key(plan_key) = &compiled.plan().nodes()[0].edges[0].segment else { panic!("expected key") };
        assert_ne!(input_key.as_ptr(), path_key.as_ptr());
        assert_ne!(input_key.as_ptr(), plan_key.as_ptr());
        assert_ne!(path_key.as_ptr(), plan_key.as_ptr());
        input_key.clear();
        input_key.push_str("changed");
        inputs.clear();
        assert_eq!(compiled.paths()[0].segments(), &[key("original"), key("child")]);
        assert_eq!(compiled.plan().nodes()[0].edges[0].segment, key("original"));
        assert_invariants(&compiled);
    }

    #[test]
    fn compiled_strings_outlive_their_source_storage() {
        let compiled = {
            let input = String::from(r#"["a.b"][4294967295]"#);
            compile_strings(&[input.as_str()], PathLimits::default()).unwrap()
        };
        assert_eq!(compiled.paths()[0].segments(), &[key("a.b"), Segment::Index(u32::MAX)]);
        assert_invariants(&compiled);
    }

    #[test]
    fn programmatic_segment_limits_are_rechecked_and_inclusive() {
        let paths = [path(vec![key("a"), Segment::Index(0)])];
        let limits = PathLimits { max_segments: 2, ..PathLimits::default() };
        assert!(compile(&paths, limits).is_ok());
        assert_budget(compile(&paths, PathLimits { max_segments: 1, ..limits }).unwrap_err());
        assert_budget(compile(&paths, PathLimits { max_segments: 0, ..limits }).unwrap_err());
        assert!(compile(&[Path::root()], PathLimits { max_segments: 0, ..limits }).is_ok());
        // Internal construction cannot bypass the compiler's current limits.
        let unchecked = Path { segments: vec![Segment::Index(0); 3] };
        assert_budget(compile(&[unchecked], limits).unwrap_err());
    }

    #[test]
    fn key_limits_count_decoded_utf8_bytes_per_segment() {
        let paths = [path(vec![key("é"), key("ab")])];
        let limits = PathLimits { max_key_bytes: 2, ..PathLimits::default() };
        assert!(compile(&paths, limits).is_ok());
        assert_budget(compile(&paths, PathLimits { max_key_bytes: 1, ..limits }).unwrap_err());
        let unchecked = Path { segments: vec![key("😀")] };
        assert_budget(compile(&[unchecked], limits).unwrap_err());
        let empty_keys = [path(vec![key(""), Segment::Index(u32::MAX), key("")])];
        assert!(compile(&empty_keys, PathLimits { max_key_bytes: 0, ..limits }).is_ok());
    }

    #[test]
    fn textual_and_execution_budgets_do_not_restrict_typed_compilation() {
        let limits = PathLimits {
            max_path_bytes: 0, max_matches: 0, max_visits: 0,
            ..PathLimits::default()
        };
        let compiled = compile(&[path(vec![key("a long literal key"), Segment::Index(u32::MAX)])], limits).unwrap();
        assert_invariants(&compiled);
        assert_budget(compile_strings(&["a"], limits).unwrap_err());
        assert!(compile_strings(&[""], limits).is_ok());
    }

    #[test]
    fn path_count_limits_include_duplicates_and_are_inclusive() {
        let limits = PathLimits { max_paths: 2, ..PathLimits::default() };
        let paths = [Path::root(), Path::root(), Path::root()];
        assert_eq!(compile(&paths[..2], limits).unwrap().plan().slot_count(), 2);
        assert_budget(compile(&paths, limits).unwrap_err());
        assert!(compile(&[], PathLimits { max_paths: 0, ..limits }).is_ok());
        assert_budget(compile(&paths[..1], PathLimits { max_paths: 0, ..limits }).unwrap_err());
    }

    #[test]
    fn node_budget_counts_unique_prefixes_and_includes_the_root() {
        let inputs = ["a.b", "a.c", "a", "a.b", "$"];
        let limits = PathLimits { max_trie_nodes: 4, ..PathLimits::default() };
        let compiled = compile_strings(&inputs, limits).unwrap();
        assert_eq!(compiled.plan().nodes().len(), 4);
        assert_invariants(&compiled);
        assert_budget(compile_strings(&inputs, PathLimits { max_trie_nodes: 3, ..limits }).unwrap_err());
        assert_budget(compile_strings(&["a"], PathLimits { max_trie_nodes: 1, ..limits }).unwrap_err());
    }

    #[test]
    fn duplicate_paths_fit_an_already_full_trie() {
        let limits = PathLimits { max_trie_nodes: 3, ..PathLimits::default() };
        let compiled = compile_strings(&["a.b", "a.b", "a", "a.b"], limits).unwrap();
        assert_eq!(compiled.plan().nodes().len(), 3);
        assert_eq!(compiled.plan().nodes()[2].slots, &[0, 1, 3]);
        assert_invariants(&compiled);
    }

    #[test]
    fn cursor_slot_ceiling_applies_even_when_the_path_limit_is_larger() {
        let limits = PathLimits { max_paths: u32::MAX, max_trie_nodes: 1, ..PathLimits::default() };
        let mut paths = vec![Path::root(); jmax_cursor::MAX_SELECTION_SLOTS as usize];
        let compiled = compile(&paths, limits).unwrap();
        assert_eq!(compiled.plan().slot_count(), jmax_cursor::MAX_SELECTION_SLOTS);
        let slots = &compiled.plan().nodes()[0].slots;
        assert_eq!(slots.len(), paths.len());
        assert_eq!(slots.first(), Some(&0));
        assert_eq!(slots.last(), Some(&(jmax_cursor::MAX_SELECTION_SLOTS - 1)));
        paths.push(Path::root());
        assert_budget(compile(&paths, limits).unwrap_err());
    }

    #[test]
    fn cursor_node_ceiling_and_deep_paths_do_not_require_recursion() {
        let limits = PathLimits {
            max_segments: u32::MAX, max_trie_nodes: u32::MAX,
            ..PathLimits::default()
        };
        let mut paths = [Path { segments: vec![Segment::Index(0); jmax_cursor::MAX_SELECTOR_NODES - 1] }];
        {
            let compiled = compile(&paths, limits).unwrap();
            let nodes = compiled.plan().nodes();
            assert_eq!(nodes.len(), jmax_cursor::MAX_SELECTOR_NODES);
            for (index, node) in nodes[..nodes.len() - 1].iter().enumerate() {
                assert_eq!(node.edges, &[edge(Segment::Index(0), (index + 1) as u32)]);
                assert!(node.slots.is_empty());
            }
            assert_eq!(nodes.last().unwrap().slots, &[0]);
            assert!(nodes.last().unwrap().edges.is_empty());
        }
        paths[0].segments.push(Segment::Index(0));
        assert_budget(compile(&paths, limits).unwrap_err());
    }

    #[test]
    fn strings_preserve_parser_errors_exactly_and_return_the_first_error() {
        for inputs in [
            ["valid", "é?", "["],
            ["valid", "[", "[01]"],
            ["valid", r#"["\x"]"#, "["],
            ["valid", r#"["\uD800"]"#, "["],
            ["valid", "[4294967296]", "["],
        ] {
            let limits = PathLimits::default();
            let expected = super::super::parse::parse_many(&inputs, limits).unwrap_err();
            assert_eq!(compile_strings(&inputs, limits).unwrap_err(), expected);
        }
    }

    #[test]
    fn strings_do_not_enable_extensions_but_allow_literal_extension_keys() {
        for input in ["*", "a.*", "[*]", "[?(@.x==1)]", "a..b", "[0,1]", "[0:2]"] {
            let error = compile_strings(&[input], PathLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::UnsupportedSchemaFeature);
            assert_eq!(error.source, ErrorSource::Cursor);
            assert_eq!(error, super::super::parse::parse(input, PathLimits::default()).unwrap_err());
        }
        let compiled = compile_strings(&[r"\*", r#"["*"]"#, r#"["?"]"#], PathLimits::default()).unwrap();
        assert_eq!(compiled.plan().nodes()[1].slots, &[0, 1]);
        assert_eq!(compiled.plan().nodes()[2].slots, &[2]);
        assert_invariants(&compiled);
    }

    #[test]
    fn strings_parse_before_compiling_and_check_path_count_before_syntax() {
        let limits = PathLimits { max_paths: 1, ..PathLimits::default() };
        let inputs = ["[", "good"];
        assert_eq!(compile_strings(&inputs, limits).unwrap_err(),
            super::super::parse::parse_many(&inputs, limits).unwrap_err());
        let limits = PathLimits { max_trie_nodes: 0, ..PathLimits::default() };
        let error = compile_strings(&["["], limits).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnexpectedEof);
        assert_eq!(error.byte_offset, 1);
        let limits = PathLimits { max_path_bytes: 2, ..PathLimits::default() };
        let error = compile_strings(&["abc"], limits).unwrap_err();
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.byte_offset, 2);
    }

    #[test]
    fn strings_and_typed_compilation_produce_identical_owned_results() {
        let inputs = ["a.b", "$", r#"[""][0]"#, "a.b", "[4294967295]", r"\*"];
        let limits = PathLimits::default();
        let parsed = super::super::parse::parse_many(&inputs, limits).unwrap();
        let typed = compile(&parsed, limits).unwrap();
        let textual = compile_strings(&inputs, limits).unwrap();
        assert_eq!(typed.paths(), textual.paths());
        assert_eq!(typed.plan(), textual.plan());
        assert_invariants(&textual);
    }

    #[test]
    fn repeated_compilation_is_deterministic_across_growth_boundaries() {
        let mut paths = Vec::new();
        for index in (0..129u32).rev() {
            paths.push(path(vec![key("common"), Segment::Index(index), key("tail")]));
        }
        let limits = PathLimits::default();
        let first = compile(&paths, limits).unwrap();
        let second = compile(&paths, limits).unwrap();
        assert_eq!(first.paths(), &paths);
        assert_eq!(first.paths(), second.paths());
        assert_eq!(first.plan(), second.plan());
        assert_eq!(first.plan().nodes().len(), 260);
        for (position, edge) in first.plan().nodes()[1].edges.iter().enumerate() {
            assert_eq!(edge.segment, Segment::Index(128 - position as u32));
            assert_eq!(edge.target, 2 + 2 * position as u32);
        }
        assert_invariants(&first);
    }

    #[test]
    fn reservation_capacity_overflow_is_a_deterministic_allocation_error() {
        let mut values = vec![SelectorNode::default()];
        // This request exceeds the representable allocation layout regardless
        // of available memory, so no allocator-failure injection is necessary.
        let error = reserve_exact(&mut values, usize::MAX, "test allocation failed").unwrap_err();
        assert_eq!(error.detail, "test allocation failed");
        assert_budget(error);
        assert_eq!(values, &[SelectorNode::default()]);
    }

    #[test]
    fn bounded_reservation_accepts_the_limit_and_rejects_the_next_push() {
        let mut values = Vec::new();
        assert_budget(reserve_one(&mut values, 0, "test bound exhausted").unwrap_err());
        assert!(values.is_empty());
        for index in 0..9u32 {
            reserve_one(&mut values, 9, "test bound exhausted").unwrap();
            assert!(values.capacity() > values.len());
            values.push(index);
        }
        assert_eq!(values.len(), 9);
        assert_budget(reserve_one(&mut values, 9, "test bound exhausted").unwrap_err());
        assert_eq!(values, &[0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
