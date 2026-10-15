//! Deterministic, bounded Thompson programs over Unicode scalar values.
//!
//! Parsing uses heap frames, never recursive descent. Patch lists are linked
//! through a flat arena, so nested alternations do not repeatedly copy exits.
//! Execution merges all search origins at each scalar boundary and visits each
//! PC at most once per closure, including in the presence of epsilon cycles.

use super::{compile_error, Error, ErrorCode, PatternOp, PatternProgram, Result, PATTERN_VERSION};
use alloc::vec::Vec;
use core::{iter::Peekable, str::Chars};

fn budget() -> Error {
    compile_error(ErrorCode::BudgetExhausted, "pattern resource budget exhausted")
}

fn syntax(detail: &'static str) -> Error {
    compile_error(ErrorCode::ConstraintViolation, detail)
}

fn unsupported(detail: &'static str) -> Error {
    compile_error(ErrorCode::UnsupportedSchemaFeature, detail)
}

fn malformed(detail: &'static str) -> Error {
    compile_error(ErrorCode::SchemaImageInvalid, detail)
}

fn count(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| budget())
}

fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<()> {
    count(values.len().checked_add(additional).ok_or_else(budget)?)?;
    values.try_reserve(additional).map_err(|_| budget())
}

fn push<T>(values: &mut Vec<T>, value: T) -> Result<()> {
    reserve(values, 1)?;
    values.push(value);
    Ok(())
}

fn filled<T: Copy>(len: usize, value: T) -> Result<Vec<T>> {
    let mut values = Vec::new();
    reserve(&mut values, len)?;
    values.resize(len, value);
    Ok(values)
}

// A dangling edge is never exposed in a successful PatternProgram. Keeping
// its linkage outside the instruction also avoids overloading real PC values.
const DANGLING: u32 = u32::MAX;

#[derive(Default)]
struct Exits {
    head: Option<usize>,
    tail: Option<usize>,
}

struct Patch {
    pc: u32,
    second: bool,
    next: Option<usize>,
}

struct Fragment {
    start: u32,
    exits: Exits,
}

struct Builder {
    ops: Vec<PatternOp>,
    patches: Vec<Patch>,
    max_states: u32,
}

impl Builder {
    fn emit(&mut self, op: PatternOp) -> Result<u32> {
        let pc = count(self.ops.len())?;
        if pc >= self.max_states {
            return Err(budget());
        }
        push(&mut self.ops, op)?;
        Ok(pc)
    }

    fn exit(&mut self, pc: u32, second: bool) -> Result<Exits> {
        let index = self.patches.len();
        push(&mut self.patches, Patch { pc, second, next: None })?;
        Ok(Exits { head: Some(index), tail: Some(index) })
    }

    fn join(&mut self, left: Exits, right: Exits) -> Result<Exits> {
        match (left.tail, right.head) {
            (Some(tail), Some(_)) => {
                self.patches.get_mut(tail)
                    .ok_or_else(|| malformed("invalid compiler patch tail"))?.next = right.head;
                Ok(Exits { head: left.head, tail: right.tail })
            }
            (None, _) => Ok(right),
            (_, None) => Ok(left),
        }
    }

    fn patch(&mut self, exits: Exits, target: u32) -> Result<()> {
        let mut cursor = exits.head;
        while let Some(index) = cursor {
            let patch = self.patches.get(index)
                .ok_or_else(|| malformed("invalid compiler patch index"))?;
            let pc = usize::try_from(patch.pc).map_err(|_| budget())?;
            let op = self.ops.get_mut(pc)
                .ok_or_else(|| malformed("invalid compiler patch instruction"))?;
            let edge = match op {
                PatternOp::Split { first, second } => {
                    if patch.second { second } else { first }
                }
                PatternOp::Char { next, .. } | PatternOp::Class { next, .. }
                | PatternOp::Any { next } | PatternOp::Jump { next }
                | PatternOp::AssertStart { next } | PatternOp::AssertEnd { next }
                    if !patch.second => next,
                _ => return Err(malformed("compiler patch has no outgoing edge")),
            };
            if *edge != DANGLING {
                return Err(malformed("compiler attempted to patch an edge twice"));
            }
            *edge = target;
            cursor = patch.next;
        }
        Ok(())
    }

    fn atom(&mut self, op: PatternOp) -> Result<Fragment> {
        let start = self.emit(op)?;
        Ok(Fragment { start, exits: self.exit(start, false)? })
    }

    fn empty(&mut self) -> Result<Fragment> {
        self.atom(PatternOp::Jump { next: DANGLING })
    }

    fn concat(&mut self, left: Fragment, right: Fragment) -> Result<Fragment> {
        self.patch(left.exits, right.start)?;
        Ok(Fragment { start: left.start, exits: right.exits })
    }

    fn alternate(&mut self, left: Fragment, right: Fragment) -> Result<Fragment> {
        let start = self.emit(PatternOp::Split { first: left.start, second: right.start })?;
        Ok(Fragment { start, exits: self.join(left.exits, right.exits)? })
    }

    fn repeat(&mut self, atom: Fragment, quantifier: char) -> Result<Fragment> {
        let split = self.emit(PatternOp::Split { first: atom.start, second: DANGLING })?;
        let exit = self.exit(split, true)?;
        if quantifier == '?' {
            return Ok(Fragment { start: split, exits: self.join(atom.exits, exit)? });
        }
        self.patch(atom.exits, split)?;
        Ok(Fragment { start: if quantifier == '+' { atom.start } else { split }, exits: exit })
    }
}

#[derive(Default)]
struct Frame {
    alternatives: Option<Fragment>,
    sequence: Option<Fragment>,
    atom: Option<Fragment>,
    quantified: bool,
}

impl Frame {
    fn flush_atom(&mut self, builder: &mut Builder) -> Result<()> {
        if let Some(atom) = self.atom.take() {
            self.sequence = Some(match self.sequence.take() {
                Some(left) => builder.concat(left, atom)?,
                None => atom,
            });
        }
        self.quantified = false;
        Ok(())
    }

    fn set_atom(&mut self, builder: &mut Builder, atom: Fragment) -> Result<()> {
        self.flush_atom(builder)?;
        self.atom = Some(atom);
        Ok(())
    }

    fn finish_branch(&mut self, builder: &mut Builder) -> Result<()> {
        self.flush_atom(builder)?;
        let branch = match self.sequence.take() {
            Some(sequence) => sequence,
            None => builder.empty()?,
        };
        self.alternatives = Some(match self.alternatives.take() {
            Some(left) => builder.alternate(left, branch)?,
            None => branch,
        });
        Ok(())
    }

    fn finish(mut self, builder: &mut Builder) -> Result<Fragment> {
        self.finish_branch(builder)?;
        self.alternatives.ok_or_else(|| malformed("compiler expression has no fragment"))
    }
}

fn escaped(chars: &mut Peekable<Chars<'_>>) -> Result<char> {
    match chars.next().ok_or_else(|| syntax("unfinished pattern escape"))? {
        'n' => Ok('\n'),
        'r' => Ok('\r'),
        't' => Ok('\t'),
        ch @ ('\\' | '.' | '*' | '+' | '?' | '|' | '(' | ')' | '[' | ']'
            | '^' | '$' | '-' | '{' | '}') => Ok(ch),
        _ => Err(unsupported("unsupported pattern escape")),
    }
}

fn class_scalar(chars: &mut Peekable<Chars<'_>>) -> Result<u32> {
    match chars.next().ok_or_else(|| syntax("unclosed pattern class"))? {
        '\\' => Ok(u32::from(escaped(chars)?)),
        ']' => Err(syntax("missing class range endpoint")),
        '{' | '}' => Err(unsupported("unescaped braces are unsupported")),
        ch => Ok(u32::from(ch)),
    }
}

fn parse_class(chars: &mut Peekable<Chars<'_>>) -> Result<PatternOp> {
    let negated = chars.peek() == Some(&'^');
    if negated {
        chars.next();
    }
    let mut ranges = Vec::new();
    loop {
        match chars.peek() {
            Some(']') => { chars.next(); break; }
            None => return Err(syntax("unclosed pattern class")),
            _ => {}
        }
        let first = class_scalar(chars)?;
        let mut last = first;
        if chars.peek() == Some(&'-') {
            let mut lookahead = chars.clone();
            lookahead.next();
            if lookahead.peek() != Some(&']') {
                chars.next();
                last = class_scalar(chars)?;
                if first > last {
                    return Err(syntax("reversed pattern class range"));
                }
            }
        }
        push(&mut ranges, (first, last))?;
    }
    // [] is the empty set; [^] is its complement over valid scalars. A literal
    // closing bracket must be escaped. Leading/trailing hyphens are literals.
    ranges.sort_unstable();
    let mut written = 0usize;
    for read in 0..ranges.len() {
        let (first, last) = ranges[read];
        if written != 0 && first <= ranges[written - 1].1.saturating_add(1) {
            ranges[written - 1].1 = ranges[written - 1].1.max(last);
        } else {
            ranges[written] = (first, last);
            written = written.checked_add(1).ok_or_else(budget)?;
        }
    }
    ranges.truncate(written);
    Ok(PatternOp::Class { ranges, negated, next: DANGLING })
}

/// Compile the supported grammar without recursion or backtracking.
///
/// Operations are emitted as atoms are read, with combination instructions
/// following their operands and exactly one final Accept. Empty expressions
/// use an explicit Jump, including the completely empty pattern. State limits
/// include that Jump and the final Accept. Parser frames and class storage are
/// bounded by the checked u32 source byte length, independently of NFA size.
/// All dynamic growth is fallible; `max_steps` is an execution allowance.
pub fn compile(pattern: &str, max_states: u32, max_steps: u32) -> Result<PatternProgram> {
    if max_states == 0 || max_steps == 0 {
        return Err(budget());
    }
    count(pattern.len())?;
    let mut builder = Builder { ops: Vec::new(), patches: Vec::new(), max_states };
    let mut parents = Vec::new();
    let mut frame = Frame::default();
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        let op = match ch {
            '(' => {
                if chars.peek() == Some(&'?') {
                    return Err(unsupported("special pattern groups are unsupported"));
                }
                push(&mut parents, frame)?;
                frame = Frame::default();
                continue;
            }
            ')' => {
                let mut parent = parents.pop().ok_or_else(|| syntax("unmatched closing group"))?;
                let atom = frame.finish(&mut builder)?;
                parent.set_atom(&mut builder, atom)?;
                frame = parent;
                continue;
            }
            '|' => { frame.finish_branch(&mut builder)?; continue; }
            '*' | '+' | '?' => {
                if frame.quantified {
                    return Err(if ch == '?' || ch == '+' {
                        unsupported("lazy and possessive quantifiers are unsupported")
                    } else {
                        syntax("repeated pattern quantifier")
                    });
                }
                let atom = frame.atom.take().ok_or_else(|| syntax("misplaced pattern quantifier"))?;
                frame.atom = Some(builder.repeat(atom, ch)?);
                frame.quantified = true;
                continue;
            }
            '[' => parse_class(&mut chars)?,
            ']' => return Err(syntax("unmatched closing class")),
            '{' | '}' => return Err(unsupported("counted pattern syntax is unsupported")),
            '.' => PatternOp::Any { next: DANGLING },
            '^' => PatternOp::AssertStart { next: DANGLING },
            '$' => PatternOp::AssertEnd { next: DANGLING },
            '\\' => PatternOp::Char { scalar: u32::from(escaped(&mut chars)?), next: DANGLING },
            ch => PatternOp::Char { scalar: u32::from(ch), next: DANGLING },
        };
        let atom = builder.atom(op)?;
        frame.set_atom(&mut builder, atom)?;
    }
    if !parents.is_empty() {
        return Err(syntax("unclosed pattern group"));
    }
    let expression = frame.finish(&mut builder)?;
    let accept = builder.emit(PatternOp::Accept)?;
    builder.patch(expression.exits, accept)?;
    Ok(PatternProgram { version: PATTERN_VERSION, start: expression.start,
        ops: builder.ops, max_steps })
}

fn pc_index(pc: u32, len: usize) -> Result<usize> {
    let index = usize::try_from(pc).map_err(|_| malformed("pattern target does not fit usize"))?;
    if index >= len {
        Err(malformed("pattern target is outside its instruction vector"))
    } else {
        Ok(index)
    }
}

fn successors(op: &PatternOp) -> (Option<u32>, Option<u32>) {
    match *op {
        PatternOp::Char { next, .. } | PatternOp::Class { next, .. }
        | PatternOp::Any { next } | PatternOp::Jump { next }
        | PatternOp::AssertStart { next } | PatternOp::AssertEnd { next } => (Some(next), None),
        PatternOp::Split { first, second } => (Some(first), Some(second)),
        PatternOp::Accept => (None, None),
    }
}

/// Check every instruction, including unreachable instructions, then establish
/// graph reachability of an Accept. Reachability is structural, not a claim
/// that assertions or consuming transitions can all succeed for some string.
/// Empty classes and epsilon cycles are valid. Resource limits are inclusive.
pub fn validate(program: &PatternProgram, max_states: u32) -> Result<()> {
    if program.version != PATTERN_VERSION || program.max_steps == 0 || program.ops.is_empty() {
        return Err(malformed("invalid pattern version, allowance, or empty instruction vector"));
    }
    let len = u32::try_from(program.ops.len())
        .map_err(|_| malformed("pattern instruction count does not fit u32"))?;
    if len > max_states {
        return Err(budget());
    }
    pc_index(program.start, program.ops.len())?;
    for op in &program.ops {
        match op {
            PatternOp::Char { scalar, .. } if char::from_u32(*scalar).is_none() => {
                return Err(malformed("pattern character is not a Unicode scalar"));
            }
            PatternOp::Class { ranges, .. } => {
                u32::try_from(ranges.len())
                    .map_err(|_| malformed("pattern range count does not fit u32"))?;
                let mut previous: Option<u32> = None;
                for &(first, last) in ranges {
                    if char::from_u32(first).is_none() || char::from_u32(last).is_none()
                        || first > last || previous.is_some_and(|end| first <= end.saturating_add(1))
                    {
                        return Err(malformed("pattern ranges are not canonical scalar intervals"));
                    }
                    previous = Some(last);
                }
            }
            _ => {}
        }
        let (first, second) = successors(op);
        for target in first.into_iter().chain(second) {
            pc_index(target, program.ops.len())?;
        }
    }
    let mut seen = filled(program.ops.len(), false)?;
    let mut pending = Vec::new();
    reserve(&mut pending, program.ops.len())?;
    let start = pc_index(program.start, seen.len())?;
    seen[start] = true;
    pending.push(program.start);
    while let Some(pc) = pending.pop() {
        let index = pc_index(pc, program.ops.len())?;
        let op = &program.ops[index];
        if matches!(op, PatternOp::Accept) {
            return Ok(());
        }
        let (first, second) = successors(op);
        // LIFO traversal follows the first branch before the second branch.
        for target in second.into_iter().chain(first) {
            let index = pc_index(target, seen.len())?;
            if !seen[index] {
                seen[index] = true;
                pending.push(target);
            }
        }
    }
    Err(malformed("pattern has no reachable accept instruction"))
}

struct Meter<'a> {
    local: u64,
    caller: &'a mut u64,
}

impl Meter<'_> {
    fn charge(&mut self) -> Result<()> {
        if self.local == 0 || *self.caller == 0 {
            return Err(budget());
        }
        self.local -= 1;
        *self.caller -= 1;
        Ok(())
    }
}

struct Closure {
    seen: Vec<u64>,
    generation: u64,
    pending: Vec<u32>,
}

impl Closure {
    fn new(len: usize) -> Result<Self> {
        let seen = filled(len, 0)?;
        let mut pending = Vec::new();
        reserve(&mut pending, len)?;
        Ok(Self { seen, generation: 0, pending })
    }

    fn begin(&mut self) -> Result<()> {
        // Each boundary requires an injection, so a u32 execution allowance
        // makes u64 generation wrap impossible. Keep the addition checked.
        self.generation = self.generation.checked_add(1).ok_or_else(budget)?;
        self.pending.clear();
        Ok(())
    }

    fn schedule(&mut self, pc: u32) -> Result<()> {
        let index = pc_index(pc, self.seen.len())?;
        if self.seen[index] != self.generation {
            self.seen[index] = self.generation;
            // Mark on insertion: even a convergent graph cannot grow this
            // stack beyond the reserved number of distinct instructions.
            self.pending.push(pc);
        }
        Ok(())
    }
}

fn class_contains(ranges: &[(u32, u32)], scalar: u32, meter: &mut Meter<'_>) -> Result<bool> {
    let mut low = 0usize;
    let mut high = ranges.len();
    while low < high {
        meter.charge()?;
        let middle = low + (high - low) / 2;
        let (first, last) = ranges[middle];
        if scalar < first {
            high = middle;
        } else if scalar > last {
            low = middle + 1;
        } else {
            return Ok(true);
        }
    }
    Ok(false)
}

fn consuming_target(op: &PatternOp, scalar: Option<char>, meter: &mut Meter<'_>) -> Result<Option<u32>> {
    // Charge a transition test even at the end of input, where it must fail.
    meter.charge()?;
    let Some(ch) = scalar else { return Ok(None); };
    match op {
        PatternOp::Char { scalar, next } => Ok((u32::from(ch) == *scalar).then_some(*next)),
        PatternOp::Any { next } => {
            Ok((!matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')).then_some(*next))
        }
        PatternOp::Class { ranges, negated, next } => {
            Ok((class_contains(ranges, u32::from(ch), meter)? != *negated).then_some(*next))
        }
        _ => Err(malformed("non-consuming instruction used as a scalar transition")),
    }
}

/// Search at every scalar boundary, including the absolute end of the string.
///
/// The complete raw program is validated before budgets are spent, so an early
/// match cannot conceal malformed unreachable instructions. Structural checking
/// is separate from the execution allowance. Each execution instruction visit,
/// edge/assertion/consuming test, class binary-search comparison, and injected
/// start costs one unit in both budgets. Exhaustion never becomes a nonmatch.
/// Only the per-call local allowance is initialized; the caller is never reset.
pub fn is_match(program: &PatternProgram, text: &str, steps_left: &mut u64) -> Result<bool> {
    validate(program, u32::MAX)?;
    let mut meter = Meter { local: u64::from(program.max_steps), caller: steps_left };
    let mut closure = Closure::new(program.ops.len())?;
    let mut seeds = Vec::new();
    let mut next = Vec::new();
    reserve(&mut seeds, program.ops.len())?;
    reserve(&mut next, program.ops.len())?;
    let mut chars = text.chars();
    let mut at_start = true;
    loop {
        closure.begin()?;
        meter.charge()?; // Unanchored start injection, including the end boundary.
        closure.schedule(program.start)?;
        for &pc in seeds.iter().rev() {
            closure.schedule(pc)?;
        }
        let scalar = chars.next();
        let at_end = scalar.is_none();
        while let Some(pc) = closure.pending.pop() {
            meter.charge()?;
            let index = pc_index(pc, program.ops.len())?;
            match &program.ops[index] {
                PatternOp::Accept => return Ok(true),
                PatternOp::Split { first, second } => {
                    meter.charge()?;
                    closure.schedule(*second)?;
                    meter.charge()?;
                    closure.schedule(*first)?;
                }
                PatternOp::Jump { next } => {
                    meter.charge()?;
                    closure.schedule(*next)?;
                }
                PatternOp::AssertStart { next } => {
                    meter.charge()?;
                    if at_start { closure.schedule(*next)?; }
                }
                PatternOp::AssertEnd { next } => {
                    meter.charge()?;
                    if at_end { closure.schedule(*next)?; }
                }
                op => {
                    if let Some(target) = consuming_target(op, scalar, &mut meter)? {
                        // At most one target per visited consuming instruction.
                        // Duplicate seeds are merged by the next closure.
                        next.push(target);
                    }
                }
            }
        }
        if at_end {
            return Ok(false);
        }
        core::mem::swap(&mut seeds, &mut next);
        next.clear();
        at_start = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::{String, ToString}, vec};
    use crate::ErrorSource;

    fn program(source: &str) -> PatternProgram {
        compile(source, 65_536, 1_000_000).unwrap()
    }

    fn matches(source: &str, text: &str) -> bool {
        let mut steps = 1_000_000;
        is_match(&program(source), text, &mut steps).unwrap()
    }

    fn error_is(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    fn raw(ops: Vec<PatternOp>, start: u32) -> PatternProgram {
        PatternProgram { version: PATTERN_VERSION, start, ops, max_steps: 100_000 }
    }

    fn invalid(program: &PatternProgram) {
        error_is(validate(program, u32::MAX).unwrap_err(), ErrorCode::SchemaImageInvalid);
        for text in ["", "a", "é", "unrelated text"] {
            for allowance in [0, 1, 100_000] {
                let mut remaining = allowance;
                error_is(is_match(program, text, &mut remaining).unwrap_err(), ErrorCode::SchemaImageInvalid);
                assert_eq!(remaining, allowance);
            }
        }
    }

    #[test]
    fn exact_source_order_and_final_accept_layouts() {
        let empty = program("");
        assert_eq!(empty.start, 0);
        assert_eq!(empty.version, PATTERN_VERSION);
        assert_eq!(empty.ops, vec![PatternOp::Jump { next: 1 }, PatternOp::Accept]);
        let alternative = program("a|bc");
        assert_eq!(alternative.start, 3);
        assert_eq!(alternative.ops, vec![
            PatternOp::Char { scalar: 97, next: 4 },
            PatternOp::Char { scalar: 98, next: 2 },
            PatternOp::Char { scalar: 99, next: 4 },
            PatternOp::Split { first: 0, second: 1 },
            PatternOp::Accept,
        ]);
        let quantified = program("a*b?");
        assert_eq!(quantified.start, 1);
        assert_eq!(quantified.ops, vec![
            PatternOp::Char { scalar: 97, next: 1 },
            PatternOp::Split { first: 0, second: 3 },
            PatternOp::Char { scalar: 98, next: 4 },
            PatternOp::Split { first: 2, second: 4 },
            PatternOp::Accept,
        ]);
        for pattern in [empty, alternative, quantified] {
            validate(&pattern, u32::try_from(pattern.ops.len()).unwrap()).unwrap();
            assert!(matches!(pattern.ops.last(), Some(PatternOp::Accept)));
            assert_eq!(pattern.ops.iter().filter(|op| matches!(op, PatternOp::Accept)).count(), 1);
        }
    }

    #[test]
    fn precedence_concatenation_and_repetition_vectors() {
        let vectors = [
            ("a|bc", "xxbcxx", true), ("a|bc", "b", false),
            ("^(a|bc)d$", "ad", true), ("^(a|bc)d$", "bcd", true),
            ("^(a|bc)d$", "abd", false), ("^ab*$", "a", true),
            ("^ab*$", "abbbb", true), ("^ab*$", "b", false),
            ("^(ab)+$", "abab", true), ("^(ab)+$", "", false),
            ("^ab?c$", "ac", true), ("^ab?c$", "abc", true),
            ("^ab?c$", "abbc", false), ("^a|b$", "ax", true),
            ("^a|b$", "xb", true), ("^a|b$", "xax", false),
            ("^((a|b)c)+d?$", "acbcd", true),
            ("^((a|b)c)+d?$", "abcd", false),
        ];
        for (source, text, expected) in vectors {
            assert_eq!(matches(source, text), expected, "{source:?} on {text:?}");
        }
    }

    #[test]
    fn empty_expressions_and_epsilon_cycles_terminate() {
        for source in ["", "()", "|", "||", "a|", "|a", "(a|)", "()()",
            "()*", "()+", "()?", "(|a)*", "((a?)*)*", "((|)*)+"]
        {
            for text in ["", "a", "bbb", "😀"] {
                assert!(matches(source, text), "{source:?} on {text:?}");
            }
        }
        assert!(matches("^((a?)*)*$", "aaaa"));
        assert!(!matches("^((a?)*)*$", "aaab"));
        assert!(matches("^(|a)*b$", "aaaab"));
        assert!(!matches("^(|a)*b$", "aaaa"));
    }

    #[test]
    fn anchors_are_absolute_and_search_includes_the_end() {
        for text in ["", "a", "\n", "é😀"] {
            assert!(matches("$", text));
            assert!(matches("^", text));
            assert_eq!(matches("^$", text), text.is_empty());
        }
        assert!(matches("a$", "éa"));
        assert!(!matches("a$", "a\n"));
        assert!(!matches("^a", "\na"));
        assert!(!matches("^a$", "a\r\n"));
        assert!(!matches("a^", "a"));
        assert!(!matches("$a", "a"));
        assert!(matches("$^", ""));
        assert!(!matches("$^", "a"));
    }

    #[test]
    fn dot_consumes_exactly_one_scalar_and_excludes_four_line_terminators() {
        for ch in ['a', 'é', '😀', '\0', '\t', '\u{85}', '\u{10ffff}'] {
            assert!(matches("^.$", &ch.to_string()));
        }
        for text in ["", "ab", "e\u{301}", "\n", "\r", "\u{2028}", "\u{2029}"] {
            assert!(!matches("^.$", text), "{text:?}");
        }
        assert!(matches("^..$", "é😀"));
        assert!(matches("^..$", "e\u{301}"));
        assert!(matches(".", "\n😀\r"));
    }

    #[test]
    fn unicode_is_not_normalized_and_search_never_starts_inside_utf8() {
        assert!(matches("é", "prefixé😀suffix"));
        assert!(matches("😀+", "x😀😀y"));
        assert!(!matches("é", "e\u{301}"));
        assert!(!matches("e\u{301}", "é"));
        assert!(!matches("©", "é"));
        assert!(matches("^\0é😀\u{10ffff}$", "\0é😀\u{10ffff}"));
        assert!(matches("^[α-ω]+$", "αλφα"));
        assert!(!matches("^[α-ω]+$", "αλφαA"));
    }

    #[test]
    fn classes_are_sorted_merged_and_deterministic() {
        let compiled = program("[d-fa-cbbx-zm-no-p]");
        assert_eq!(compiled.ops, vec![
            PatternOp::Class { ranges: vec![(97, 102), (109, 112), (120, 122)],
                negated: false, next: 1 },
            PatternOp::Accept,
        ]);
        assert_eq!(program("[fedcbaxzypomn]"), compiled);
        for text in ["a", "f", "m", "p", "x", "z"] {
            assert!(matches("^[d-fa-cbbx-zm-no-p]$", text));
        }
        for text in ["", "g", "q", "w", "aa", "😀"] {
            assert!(!matches("^[d-fa-cbbx-zm-no-p]$", text));
        }
    }

    #[test]
    fn class_literals_empty_sets_negation_and_scalar_edges() {
        for source in ["[-a]", "[a-]", "[a\\-]"] {
            for text in ["-", "a"] { assert!(matches(source, text)); }
            assert!(!matches(source, "b"));
        }
        assert!(matches(r"^[\]\[\^\-]+$", "][^-"));
        assert!(matches("^[a^]$", "^"));
        assert!(!matches("[]", "anything"));
        assert!(!matches("[]", ""));
        assert!(!matches("[^]", ""));
        for text in ["a", "\n", "\r", "\0", "😀", "\u{2028}", "\u{10ffff}"] {
            assert!(matches("^[^]$", text));
        }
        assert!(matches("^[^a-c]+$", "dé😀"));
        assert!(!matches("^[^a-c]+$", "deb"));
        for ch in ['\0', '\u{d7ff}', '\u{e000}', '\u{10ffff}'] {
            let text = ch.to_string();
            assert!(matches("^[\0-\u{10ffff}]$", &text));
            assert!(!matches("^[^\0-\u{10ffff}]$", &text));
        }
        assert!(matches("^[\u{d7ff}-\u{e000}]$", "\u{d7ff}"));
        assert!(matches("^[\u{d7ff}-\u{e000}]$", "\u{e000}"));
        assert!(!matches("^[\u{d7ff}-\u{e000}]$", "\u{e001}"));
    }

    #[test]
    fn exactly_the_documented_escapes_are_supported() {
        for ch in ['\\', '.', '*', '+', '?', '|', '(', ')', '[', ']', '^', '$', '-', '{', '}'] {
            let source = format!("^\\{ch}$");
            assert!(matches(&source, &ch.to_string()), "{source:?}");
            assert!(!matches(&source, "a"));
        }
        assert!(matches(r"^\n\r\t$", "\n\r\t"));
        assert!(matches(r"^[\n\r\t]+$", "\t\n\r"));
        assert!(matches(r"^\{2\}$", "{2}"));
        assert!(matches(r"^[\{\}]$", "{"));
        assert!(matches(r"^[\{\}]$", "}"));
    }

    #[test]
    fn malformed_syntax_has_constraint_errors() {
        for source in ["(", ")", "a)", "((a)", "[", "[^", "[a", "[a-", "\\",
            "[\\", "[z-a]", "[é-a]", "]", "a]", "*a", "+", "?", "a|*b",
            "(*)", "a**", "a+*", "a?*", "[a--]"]
        {
            error_is(compile(source, 1000, 1000).unwrap_err(), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn unsupported_semantics_are_never_approximated() {
        for source in ["a{2}", "{", "}", "a{2,3}", "a{,3}", r"\1", r"\0", r"\k<name>",
            "(?=a)", "(?!a)", "(?<=a)", "(?<!a)", "(?:a)", "(?<name>a)", "(?>a)",
            "(?i)a", "(?i:a)", "a*?", "a+?", "a??", "a*+", "a++", "a?+",
            r"\d", r"\D", r"\s", r"\w", r"\b", r"\p{L}", r"\u0041", r"\x41",
            r"\a", r"\/", r"\é", r"[\d]", r"[\b]", "[{}]"]
        {
            error_is(compile(source, 1000, 1000).unwrap_err(), ErrorCode::UnsupportedSchemaFeature);
        }
    }

    #[test]
    fn state_and_execution_configuration_limits_are_inclusive() {
        for source in ["", "a", "a|bc", "^a*b?$", "[]", "(|a)*"] {
            let baseline = program(source);
            let len = u32::try_from(baseline.ops.len()).unwrap();
            let exact = compile(source, len, 1).unwrap();
            assert_eq!(exact.ops, baseline.ops);
            assert_eq!(exact.max_steps, 1);
            validate(&exact, len).unwrap();
            error_is(compile(source, len - 1, 1).unwrap_err(), ErrorCode::BudgetExhausted);
            error_is(validate(&exact, len - 1).unwrap_err(), ErrorCode::BudgetExhausted);
        }
        error_is(compile("a", 0, 1).unwrap_err(), ErrorCode::BudgetExhausted);
        error_is(compile("a", 2, 0).unwrap_err(), ErrorCode::BudgetExhausted);
        assert_eq!(compile("a", 2, u32::MAX).unwrap().max_steps, u32::MAX);
    }

    #[test]
    fn deeply_nested_groups_use_heap_frames_and_drop_iteratively() {
        let depth = 16_384;
        let source = format!("{}a{}", "(".repeat(depth), ")".repeat(depth));
        let nested = compile(&source, 2, 100).unwrap();
        assert_eq!(nested.ops, program("a").ops);
        assert_eq!(nested.start, 0);
        let mut steps = 100;
        assert!(is_match(&nested, "a", &mut steps).unwrap());
        let broken = "(".repeat(depth);
        error_is(compile(&broken, 2, 100).unwrap_err(), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn long_alternations_have_linear_size_and_stable_patch_order() {
        let count = 2048usize;
        let source = format!("{}z", "a|".repeat(count));
        let compiled = program(&source);
        assert_eq!(compiled.ops.len(), 2 * count + 2);
        assert_eq!(compiled, program(&source));
        validate(&compiled, 65_536).unwrap();
        let mut steps = 100_000;
        assert!(is_match(&compiled, "z", &mut steps).unwrap());
        let mut steps = 100_000;
        assert!(!is_match(&compiled, "b", &mut steps).unwrap());
    }

    #[test]
    fn malformed_headers_and_unreachable_accepts_are_rejected() {
        invalid(&raw(vec![], 0));
        invalid(&raw(vec![PatternOp::Accept], 1));
        invalid(&raw(vec![PatternOp::Accept], u32::MAX));
        let mut bad_version = raw(vec![PatternOp::Accept], 0);
        bad_version.version = 0;
        invalid(&bad_version);
        bad_version.version = u16::MAX;
        invalid(&bad_version);
        let mut zero_steps = raw(vec![PatternOp::Accept], 0);
        zero_steps.max_steps = 0;
        invalid(&zero_steps);
        invalid(&raw(vec![PatternOp::Jump { next: 0 }], 0));
        invalid(&raw(vec![PatternOp::Jump { next: 0 }, PatternOp::Accept], 0));
        invalid(&raw(vec![PatternOp::Char { scalar: 97, next: 0 }, PatternOp::Accept], 0));
    }

    #[test]
    fn every_branch_kind_is_checked_even_when_unreachable() {
        for target in [2, u32::MAX] {
            let bad_ops = [
                PatternOp::Char { scalar: 97, next: target },
                PatternOp::Class { ranges: vec![], negated: false, next: target },
                PatternOp::Any { next: target },
                PatternOp::Split { first: target, second: 0 },
                PatternOp::Split { first: 0, second: target },
                PatternOp::Jump { next: target },
                PatternOp::AssertStart { next: target },
                PatternOp::AssertEnd { next: target },
            ];
            for op in bad_ops {
                invalid(&raw(vec![PatternOp::Accept, op], 0));
            }
        }
        for scalar in [0xd800, 0xdfff, 0x110000, u32::MAX] {
            invalid(&raw(vec![PatternOp::Accept, PatternOp::Char { scalar, next: 0 }], 0));
        }
    }

    #[test]
    fn raw_class_ranges_must_be_canonical_and_valid_scalars() {
        for ranges in [vec![(2, 1)], vec![(0xd800, 0xe000)], vec![(0, 0xdfff)],
            vec![(0x110000, 0x110000)], vec![(u32::MAX, u32::MAX)],
            vec![(5, 7), (1, 2)], vec![(1, 3), (3, 5)], vec![(1, 3), (4, 5)],
            vec![(1, 5), (2, 3)], vec![(1, 1), (1, 1)]]
        {
            invalid(&raw(vec![PatternOp::Accept,
                PatternOp::Class { ranges, negated: false, next: 0 }], 0));
        }
        for ranges in [vec![], vec![(0, 0x10ffff)], vec![(0xd7ff, 0xe000)],
            vec![(0, 0), (2, 2), (0x10ffff, 0x10ffff)]]
        {
            validate(&raw(vec![PatternOp::Class { ranges, negated: false, next: 1 },
                PatternOp::Accept], 0), 2).unwrap();
        }
    }

    #[test]
    fn legal_raw_cycles_duplicates_and_multiple_accepts_are_supported() {
        let cases = [
            raw(vec![PatternOp::Split { first: 0, second: 1 }, PatternOp::Accept], 0),
            raw(vec![PatternOp::Split { first: 1, second: 1 }, PatternOp::Accept], 0),
            raw(vec![PatternOp::Split { first: 1, second: 2 }, PatternOp::Accept, PatternOp::Accept], 0),
            raw(vec![PatternOp::Accept, PatternOp::Jump { next: 1 }], 0),
        ];
        for compiled in cases {
            validate(&compiled, 3).unwrap();
            let mut steps = 50;
            assert!(is_match(&compiled, "", &mut steps).unwrap());
            assert!(steps < 50);
        }
        let impossible = raw(vec![PatternOp::AssertStart { next: 1 },
            PatternOp::Char { scalar: 97, next: 2 }, PatternOp::AssertStart { next: 3 },
            PatternOp::Accept], 0);
        validate(&impossible, 4).unwrap();
        let mut steps = 1000;
        assert!(!is_match(&impossible, "a", &mut steps).unwrap());
    }

    #[test]
    fn exact_caller_and_local_budget_cutoffs_preserve_results() {
        for (source, text) in [("", ""), ("a", "a"), ("a", "b"), ("$", "😀"),
            ("^(a|aa)*b$", "aaaab"), ("^(a|aa)*b$", "aaaa"),
            ("^[a-ce-gx-z]+$", "agz"), ("[]", "a")]
        {
            let mut compiled = program(source);
            let mut remaining = 1_000_000;
            let expected = is_match(&compiled, text, &mut remaining).unwrap();
            let used = 1_000_000 - remaining;
            assert!(used > 0);
            let mut exact = used;
            assert_eq!(is_match(&compiled, text, &mut exact).unwrap(), expected);
            assert_eq!(exact, 0);
            let mut short = used - 1;
            error_is(is_match(&compiled, text, &mut short).unwrap_err(), ErrorCode::BudgetExhausted);
            assert_eq!(short, 0);
            compiled.max_steps = u32::try_from(used).unwrap();
            let mut shared = 2 * used;
            assert_eq!(is_match(&compiled, text, &mut shared).unwrap(), expected);
            assert_eq!(shared, used);
            assert_eq!(is_match(&compiled, text, &mut shared).unwrap(), expected);
            assert_eq!(shared, 0);
            compiled.max_steps -= 1;
            assert!(compiled.max_steps > 0);
            let mut ample = u64::MAX;
            error_is(is_match(&compiled, text, &mut ample).unwrap_err(), ErrorCode::BudgetExhausted);
            assert_eq!(ample, u64::MAX - u64::from(compiled.max_steps));
        }
    }

    #[test]
    fn injection_visitation_and_transition_tests_each_spend_budget() {
        let compiled = raw(vec![PatternOp::Accept], 0);
        let mut steps = 2;
        assert!(is_match(&compiled, "", &mut steps).unwrap());
        assert_eq!(steps, 0);
        let compiled = program("a");
        let mut steps = 3;
        assert!(!is_match(&compiled, "", &mut steps).unwrap());
        assert_eq!(steps, 0);
        let mut steps = 5;
        assert!(is_match(&compiled, "a", &mut steps).unwrap());
        assert_eq!(steps, 0);
        let mut steps = 0;
        error_is(is_match(&compiled, "a", &mut steps).unwrap_err(), ErrorCode::BudgetExhausted);
        assert_eq!(steps, 0);
    }

    #[test]
    fn class_binary_search_comparisons_are_metered() {
        let ranges = [(1, 1), (3, 3), (5, 5), (7, 7), (9, 9)];
        let mut steps = 10;
        let mut meter = Meter { local: 10, caller: &mut steps };
        assert!(class_contains(&ranges, 5, &mut meter).unwrap());
        assert_eq!(meter.local, 9);
        assert_eq!(*meter.caller, 9);
        let mut steps = 1;
        let mut meter = Meter { local: 10, caller: &mut steps };
        error_is(class_contains(&ranges, 2, &mut meter).unwrap_err(), ErrorCode::BudgetExhausted);
        assert_eq!(*meter.caller, 0);
    }

    #[test]
    fn large_inputs_and_ambiguous_repetition_cannot_turn_exhaustion_into_a_miss() {
        let text = "a".repeat(100_000);
        for source in ["z", "^(a|aa)*b$", "((a?)*)*b", "[z]"] {
            let compiled = compile(source, 100, 500).unwrap();
            let mut steps = 10_000;
            error_is(is_match(&compiled, &text, &mut steps).unwrap_err(), ErrorCode::BudgetExhausted);
            assert_eq!(steps, 9_500);
        }
        let source = "^(a|aa)*b$";
        let text = format!("{}b", "a".repeat(1024));
        let mut steps = 100_000;
        assert!(is_match(&program(source), &text, &mut steps).unwrap());
    }

    #[test]
    fn deterministic_small_language_corpus_matches_independent_predicates() {
        let sources = ["a", "^a*b?$", "^(ab|é)+$", "^[^a]+$", "^a?b+$", "a$", "^.$"];
        let compiled: Vec<_> = sources.iter().map(|source| program(source)).collect();
        let alphabet = ['a', 'b', 'é', '\n'];
        for len in 0..=5u32 {
            for mut code in 0..4usize.pow(len) {
                let mut text = String::new();
                for _ in 0..len {
                    text.push(alphabet[code % 4]);
                    code /= 4;
                }
                let without_a = text.trim_start_matches('a');
                let optional_a = text.strip_prefix('a').unwrap_or(&text);
                let mut pieces = text.as_str();
                let mut any_piece = false;
                while let Some(rest) = pieces.strip_prefix("ab").or_else(|| pieces.strip_prefix('é')) {
                    pieces = rest;
                    any_piece = true;
                }
                let expected = [
                    text.contains('a'),
                    without_a.is_empty() || without_a == "b",
                    any_piece && pieces.is_empty(),
                    !text.is_empty() && !text.contains('a'),
                    !optional_a.is_empty() && optional_a.chars().all(|ch| ch == 'b'),
                    text.ends_with('a'),
                    text.chars().count() == 1 && !text.contains('\n'),
                ];
                for ((source, pattern), expected) in sources.iter().zip(&compiled).zip(expected) {
                    let mut steps = 10_000;
                    assert_eq!(is_match(pattern, &text, &mut steps).unwrap(), expected,
                        "{source:?} on {text:?}");
                }
            }
        }
    }

    #[test]
    fn repeated_builds_are_identical_and_all_outputs_validate() {
        for source in ["", "é😀", "a|bc|d", "(a|(b|c))?", "^[c-aa-c]*$", "[^]",
            "(|a)*", "^((ab)?|c+)*$", r"\{[\]\-\n]\}"]
        {
            // The reversed-range vector must fail identically as well.
            let first = compile(source, 1000, 12_345);
            for _ in 0..4 {
                assert_eq!(compile(source, 1000, 12_345), first);
            }
            if let Ok(compiled) = first {
                validate(&compiled, 1000).unwrap();
            }
        }
    }

    #[test]
    fn arithmetic_and_allocation_failures_report_budget_errors() {
        let mut values = Vec::<u64>::new();
        error_is(reserve(&mut values, usize::MAX).unwrap_err(), ErrorCode::BudgetExhausted);
        assert!(values.is_empty());
        error_is(pc_index(u32::MAX, 1).unwrap_err(), ErrorCode::SchemaImageInvalid);
        let mut closure = Closure::new(1).unwrap();
        closure.generation = u64::MAX;
        error_is(closure.begin().unwrap_err(), ErrorCode::BudgetExhausted);
        #[cfg(target_pointer_width = "64")]
        error_is(count(u32::MAX as usize + 1).unwrap_err(), ErrorCode::BudgetExhausted);
    }
}
