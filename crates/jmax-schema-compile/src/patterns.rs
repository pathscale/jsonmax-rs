//! Deterministic, bounded Thompson programs over Unicode scalar values.
//!
//! Parsing uses heap frames, never recursive descent. Patch lists are linked
//! through a flat arena, so nested alternations do not repeatedly copy exits.
//! Execution merges all search origins at each scalar boundary and visits each
//! PC at most once per closure, including in the presence of epsilon cycles.

use super::{Error, ErrorCode, PATTERN_VERSION, PatternOp, PatternProgram, Result, compile_error};
use crate::ValidatedProgram;
use alloc::vec::Vec;
use core::{iter::Peekable, str::Chars};

fn budget() -> Error {
    compile_error(
        ErrorCode::BudgetExhausted,
        "pattern resource budget exhausted",
    )
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
        push(
            &mut self.patches,
            Patch {
                pc,
                second,
                next: None,
            },
        )?;
        Ok(Exits {
            head: Some(index),
            tail: Some(index),
        })
    }

    fn join(&mut self, left: Exits, right: Exits) -> Result<Exits> {
        match (left.tail, right.head) {
            (Some(tail), Some(_)) => {
                self.patches
                    .get_mut(tail)
                    .ok_or_else(|| malformed("invalid compiler patch tail"))?
                    .next = right.head;
                Ok(Exits {
                    head: left.head,
                    tail: right.tail,
                })
            }
            (None, _) => Ok(right),
            (_, None) => Ok(left),
        }
    }

    fn patch(&mut self, exits: Exits, target: u32) -> Result<()> {
        let mut cursor = exits.head;
        while let Some(index) = cursor {
            let patch = self
                .patches
                .get(index)
                .ok_or_else(|| malformed("invalid compiler patch index"))?;
            let pc = usize::try_from(patch.pc).map_err(|_| budget())?;
            let op = self
                .ops
                .get_mut(pc)
                .ok_or_else(|| malformed("invalid compiler patch instruction"))?;
            let edge = match op {
                PatternOp::Split { first, second } => {
                    if patch.second {
                        second
                    } else {
                        first
                    }
                }
                PatternOp::Char { next, .. }
                | PatternOp::Class { next, .. }
                | PatternOp::Any { next }
                | PatternOp::Jump { next }
                | PatternOp::AssertStart { next }
                | PatternOp::AssertEnd { next }
                    if !patch.second =>
                {
                    next
                }
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
        Ok(Fragment {
            start,
            exits: self.exit(start, false)?,
        })
    }

    fn empty(&mut self) -> Result<Fragment> {
        self.atom(PatternOp::Jump { next: DANGLING })
    }

    fn concat(&mut self, left: Fragment, right: Fragment) -> Result<Fragment> {
        self.patch(left.exits, right.start)?;
        Ok(Fragment {
            start: left.start,
            exits: right.exits,
        })
    }

    fn alternate(&mut self, left: Fragment, right: Fragment) -> Result<Fragment> {
        let start = self.emit(PatternOp::Split {
            first: left.start,
            second: right.start,
        })?;
        Ok(Fragment {
            start,
            exits: self.join(left.exits, right.exits)?,
        })
    }

    fn repeat(&mut self, atom: Fragment, quantifier: char) -> Result<Fragment> {
        let split = self.emit(PatternOp::Split {
            first: atom.start,
            second: DANGLING,
        })?;
        let exit = self.exit(split, true)?;
        if quantifier == '?' {
            return Ok(Fragment {
                start: split,
                exits: self.join(atom.exits, exit)?,
            });
        }
        self.patch(atom.exits, split)?;
        Ok(Fragment {
            start: if quantifier == '+' { atom.start } else { split },
            exits: exit,
        })
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
        self.alternatives
            .ok_or_else(|| malformed("compiler expression has no fragment"))
    }
}

fn escaped(chars: &mut Peekable<Chars<'_>>) -> Result<char> {
    match chars
        .next()
        .ok_or_else(|| syntax("unfinished pattern escape"))?
    {
        'n' => Ok('\n'),
        'r' => Ok('\r'),
        't' => Ok('\t'),
        ch @ ('\\' | '.' | '*' | '+' | '?' | '|' | '(' | ')' | '[' | ']' | '^' | '$' | '-'
        | '{' | '}') => Ok(ch),
        _ => Err(unsupported("unsupported pattern escape")),
    }
}

fn class_scalar(chars: &mut Peekable<Chars<'_>>) -> Result<u32> {
    match chars
        .next()
        .ok_or_else(|| syntax("unclosed pattern class"))?
    {
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
            Some(']') => {
                chars.next();
                break;
            }
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
    Ok(PatternOp::Class {
        ranges,
        negated,
        next: DANGLING,
    })
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
    let mut builder = Builder {
        ops: Vec::new(),
        patches: Vec::new(),
        max_states,
    };
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
                let mut parent = parents
                    .pop()
                    .ok_or_else(|| syntax("unmatched closing group"))?;
                let atom = frame.finish(&mut builder)?;
                parent.set_atom(&mut builder, atom)?;
                frame = parent;
                continue;
            }
            '|' => {
                frame.finish_branch(&mut builder)?;
                continue;
            }
            '*' | '+' | '?' => {
                if frame.quantified {
                    return Err(if ch == '?' || ch == '+' {
                        unsupported("lazy and possessive quantifiers are unsupported")
                    } else {
                        syntax("repeated pattern quantifier")
                    });
                }
                let atom = frame
                    .atom
                    .take()
                    .ok_or_else(|| syntax("misplaced pattern quantifier"))?;
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
            '\\' => PatternOp::Char {
                scalar: u32::from(escaped(&mut chars)?),
                next: DANGLING,
            },
            ch => PatternOp::Char {
                scalar: u32::from(ch),
                next: DANGLING,
            },
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
    Ok(PatternProgram {
        version: PATTERN_VERSION,
        start: expression.start,
        ops: builder.ops,
        max_steps,
    })
}

fn pc_index(pc: u32, len: usize) -> Result<usize> {
    let index = usize::try_from(pc).map_err(|_| malformed("pattern target does not fit usize"))?;
    if index >= len {
        Err(malformed(
            "pattern target is outside its instruction vector",
        ))
    } else {
        Ok(index)
    }
}

fn successors(op: &PatternOp) -> (Option<u32>, Option<u32>) {
    match *op {
        PatternOp::Char { next, .. }
        | PatternOp::Class { next, .. }
        | PatternOp::Any { next }
        | PatternOp::Jump { next }
        | PatternOp::AssertStart { next }
        | PatternOp::AssertEnd { next } => (Some(next), None),
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
        return Err(malformed(
            "invalid pattern version, allowance, or empty instruction vector",
        ));
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
                    if char::from_u32(first).is_none()
                        || char::from_u32(last).is_none()
                        || first > last
                        || previous.is_some_and(|end| first <= end.saturating_add(1))
                    {
                        return Err(malformed(
                            "pattern ranges are not canonical scalar intervals",
                        ));
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
        Ok(Self {
            seen,
            generation: 0,
            pending,
        })
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

fn consuming_target(
    op: &PatternOp,
    scalar: Option<char>,
    meter: &mut Meter<'_>,
) -> Result<Option<u32>> {
    // Charge a transition test even at the end of input, where it must fail.
    meter.charge()?;
    let Some(ch) = scalar else {
        return Ok(None);
    };
    match op {
        PatternOp::Char { scalar, next } => Ok((u32::from(ch) == *scalar).then_some(*next)),
        PatternOp::Any { next } => {
            Ok((!matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')).then_some(*next))
        }
        PatternOp::Class {
            ranges,
            negated,
            next,
        } => Ok((class_contains(ranges, u32::from(ch), meter)? != *negated).then_some(*next)),
        _ => Err(malformed(
            "non-consuming instruction used as a scalar transition",
        )),
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
    is_match_inner(program, text, steps_left)
}

/// Execute a pattern owned by a fully validated immutable schema, without
/// repeating structural validation of its instructions on every string.
pub fn is_match_validated(
    program: &ValidatedProgram,
    pattern_index: u32,
    text: &str,
    steps_left: &mut u64,
) -> Result<bool> {
    let pattern = program
        .program()
        .patterns
        .get(pattern_index as usize)
        .ok_or_else(|| malformed("pattern index outside validated program"))?;
    is_match_inner(pattern, text, steps_left)
}

fn is_match_inner(program: &PatternProgram, text: &str, steps_left: &mut u64) -> Result<bool> {
    let mut meter = Meter {
        local: u64::from(program.max_steps),
        caller: steps_left,
    };
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
                    if at_start {
                        closure.schedule(*next)?;
                    }
                }
                PatternOp::AssertEnd { next } => {
                    meter.charge()?;
                    if at_end {
                        closure.schedule(*next)?;
                    }
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
