//! Lightweight tracing marks: where does time go inside jsonmax.
//!
//! `span!(SITE)` returns a guard; the time from the call to the guard's drop is
//! added to the site's slot, minus the time spent in nested spans (self time),
//! together with a hit count. The counter is the x86_64 `rdtsc` or the aarch64
//! `cntvct_el0`; other targets record hits only.
//!
//! With the `trace` feature off (the default) `span!` evaluates to a zero-sized
//! guard with no `Drop`, so every mark compiles to nothing. Crates forward their
//! own `trace` feature to `jmax-trace/trace`; cargo unifies it, so any one
//! crate enabling it turns the marks on in all of them.
//!
//! Slots are static relaxed atomics updated with load and store (no locked
//! read-modify-write): exact on one thread, which is how a compile runs here,
//! and merely lossy if two threads race. There is no allocation and no std.
//! Each mark costs about one counter read at entry and one at exit, so tiny
//! per-token sites inflate themselves; `calibrate` measures an empty span so a
//! report can subtract it.
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

/// Identifier of one trace site: an index into the static slot table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site(pub u16);

macro_rules! sites {
    ($($id:ident = $name:literal,)*) => {
        #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
        #[repr(u16)]
        enum Index { $($id,)* Count }
        /// Site constants, one per mark location.
        pub mod site {
            use super::{Index, Site};
            $(pub const $id: Site = Site(Index::$id as u16);)*
        }
        /// Number of sites.
        pub const SITES: usize = Index::Count as usize;
        /// Human-readable site names, indexed by `Site.0`.
        pub const NAMES: [&str; SITES] = [$($name,)*];
    };
}

sites! {
    // scanner: block classification and the short bounded scans
    SCAN_BLOCK_SCALAR = "scan classify block (scalar backend)",
    SCAN_BLOCK_NEON = "scan classify block (neon)",
    SCAN_BLOCK_AVX2 = "scan classify block (avx2)",
    SCAN_BLOCK_AVX512 = "scan classify block (avx512)",
    SCAN_TAIL = "scan classify partial tail block",
    SCAN_STRING_SPECIAL = "scan string_special(_ascii) short scan",
    SCAN_WHITESPACE = "scan whitespace_prefix short scan",
    // syntax: block cursor, strict syntax, strict_fast
    CURSOR_REFILL = "cursor refill (excl. classify)",
    CURSOR_WHITESPACE = "cursor skip_whitespace slow path",
    CURSOR_STRING = "cursor string token scan",
    CURSOR_NUMBER = "cursor number token scan",
    CURSOR_LITERAL = "cursor literal token scan",
    CURSOR_SKIP = "cursor skip_value / skip container",
    SYNTAX_FEED = "syntax incremental feed",
    STRICT_FAST_RUN = "strict_fast run (excl. children)",
    STRICT_FAST_STRING = "strict_fast string",
    STRICT_FAST_NUMBER = "strict_fast number",
    STRICT_FAST_KEY = "strict_fast key",
    STRICT_FAST_SHAPE = "strict_fast shape / duplicate check",
    STRICT_FAST_ARRAY = "strict_fast numeric array / matrix",
    // string, utf8, number
    STRING_UNESCAPE = "string unescape / cook_into",
    STRING_SCAN = "string scan_string",
    UTF8_VALIDATE = "utf8 validate",
    UTF8_COUNT = "utf8 validate and count scalars",
    NUMBER_LEX = "number lex",
    NUMBER_INT = "number integer convert",
    NUMBER_FLOAT = "number float convert",
    DECIMAL_PARSE = "decimal parse",
    // dom
    DOM_BUILD = "dom build total (excl. children)",
    DOM_SETUP = "dom fast setup (cursor, shape tables)",
    DOM_FINISH = "dom fast finish_eof",
    DOM_PREPARE = "dom prepare buffers",
    DOM_FAST_KEY = "dom fast key (shape match / scan)",
    DOM_FAST_STRING = "dom fast string value",
    DOM_FAST_NUMBER = "dom fast number",
    DOM_FAST_LITERAL = "dom fast literal",
    DOM_FAST_CONTAINER = "dom fast open container",
    DOM_FAST_CLOSE = "dom fast close container",
    DOM_FAST_SCALAR_ARRAY = "dom fast scalar array / coordinate ring",
    DOM_SHAPE = "dom shape hash / seed",
    DOM_DUP = "dom duplicate-key table",
    DOM_TAPE_PUSH = "dom tape append",
    DOM_SLOW_BUILD = "dom slow builder (after fast reject)",
    // schema vm
    VM_STRICT_ROOT = "vm strict root pass (error-path only)",
    VM_RUN = "vm run total (excl. children)",
    VM_SETUP = "vm setup (scratch, cursor, selection)",
    VM_DISPATCH = "vm per-token dispatch (loop, take_value)",
    VM_OBJECT = "vm container open / close frames",
    VM_KEY = "vm object key (scan + lookup)",
    VM_SCALAR = "vm scalar check",
    VM_SLOT_STORE = "vm slot store",
    VM_ARRAY = "vm array / item group",
    VM_SKIP = "vm skip unknown value",
    VM_CONSTRAINT = "vm constraints / budget",
    VM_DIRECT = "vm direct path (excl. children)",
    VM_FINISH = "vm finish / assemble",
    // native decoder
    NATIVE_DECODE = "native decode_into / decide (install, budget proof)",
    NATIVE_SETUP = "native decode_core glue (generated code, excl. reader calls)",
    NATIVE_READER_OPEN = "native reader open / close container",
    NATIVE_KEY_DISPATCH = "native key dispatch",
    NATIVE_SCALAR_STRING = "native decode string",
    NATIVE_SCALAR_NUMBER = "native decode integer / float",
    NATIVE_SCALAR_DECIMAL = "native decode decimal",
    NATIVE_SCALAR_BOOL = "native decode bool / null",
    NATIVE_SKIP = "native skip value",
    NATIVE_WS = "native whitespace / punctuation",
    NATIVE_ARRAY = "native array element loop",
    // bench harness
    BENCH_ROUTE = "bench route total",
}

/// Raw tick count. Monotonic enough for deltas on one core.
#[inline(always)]
pub fn now() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: rdtsc has no preconditions and is available on every x86_64 CPU.
        unsafe { core::arch::x86_64::_rdtsc() }
    }
    #[cfg(target_arch = "aarch64")]
    {
        let ticks: u64;
        // SAFETY: cntvct_el0 is readable from user mode on Linux and Darwin.
        unsafe {
            core::arch::asm!("mrs {}, cntvct_el0", out(reg) ticks, options(nomem, nostack, preserves_flags));
        }
        ticks
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        0
    }
}

#[cfg(feature = "trace")]
mod imp {
    use super::{now, Site, SITES};
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static TICKS: [AtomicU64; SITES] = [ZERO; SITES];
    static HITS: [AtomicU64; SITES] = [ZERO; SITES];
    /// Ticks spent in spans that closed since the enclosing span opened.
    static CHILD: AtomicU64 = AtomicU64::new(0);

    pub struct Guard {
        site: u16,
        start: u64,
        outer_child: u64,
    }

    #[inline(always)]
    pub fn enter(site: Site) -> Guard {
        let outer_child = CHILD.load(Relaxed);
        CHILD.store(0, Relaxed);
        Guard { site: site.0, start: now(), outer_child }
    }

    impl Drop for Guard {
        #[inline(always)]
        fn drop(&mut self) {
            let total = now().wrapping_sub(self.start);
            let child = CHILD.load(Relaxed);
            let own = total.saturating_sub(child);
            let i = self.site as usize;
            TICKS[i].store(TICKS[i].load(Relaxed).wrapping_add(own), Relaxed);
            HITS[i].store(HITS[i].load(Relaxed).wrapping_add(1), Relaxed);
            CHILD.store(self.outer_child.wrapping_add(total), Relaxed);
        }
    }

    pub fn reset() {
        for i in 0..SITES {
            TICKS[i].store(0, Relaxed);
            HITS[i].store(0, Relaxed);
        }
        CHILD.store(0, Relaxed);
    }

    pub fn read(site: Site) -> (u64, u64) {
        (TICKS[site.0 as usize].load(Relaxed), HITS[site.0 as usize].load(Relaxed))
    }
}

#[cfg(not(feature = "trace"))]
mod imp {
    use super::Site;
    /// Zero-sized, no `Drop`: a span compiles to nothing.
    pub struct Guard;
    #[inline(always)]
    pub fn enter(_site: Site) -> Guard {
        Guard
    }
    pub fn reset() {}
    pub fn read(_site: Site) -> (u64, u64) {
        (0, 0)
    }
}

pub use imp::Guard;

/// True when the crate was built with the `trace` feature.
pub const ENABLED: bool = cfg!(feature = "trace");

/// Opens a span for `site`; bind the result: `let _t = jmax_trace::span!(SITE);`.
#[inline(always)]
pub fn enter(site: Site) -> Guard {
    imp::enter(site)
}

/// Zeroes every slot.
pub fn reset() {
    imp::reset()
}

/// Self ticks and hits recorded for `site` since the last reset.
pub fn read(site: Site) -> (u64, u64) {
    imp::read(site)
}

/// Ticks one empty span costs on this machine, for subtracting from reports.
pub fn calibrate() -> u64 {
    if !ENABLED {
        return 0;
    }
    reset();
    const N: u64 = 20_000;
    for _ in 0..N {
        let _t = enter(site::BENCH_ROUTE);
    }
    let (ticks, hits) = read(site::BENCH_ROUTE);
    reset();
    ticks / hits.max(1)
}


/// `span!(NAME)` opens a scoped span for `jmax_trace::site::NAME`;
/// `span!(@ expr)` takes any `Site` expression. Bind the result:
/// `let _t = jmax_trace::span!(NAME);`. With the feature off it expands to a
/// unit value, with no function call even before optimization, so it cannot
/// move inlining decisions.
#[cfg(feature = "trace")]
#[macro_export]
macro_rules! span {
    (@ $site:expr) => {
        $crate::enter($site)
    };
    ($name:ident) => {
        $crate::enter($crate::site::$name)
    };
}

/// Disabled form: the site expression is not evaluated.
#[cfg(not(feature = "trace"))]
#[macro_export]
macro_rules! span {
    (@ $site:expr) => {
        $crate::Guard
    };
    ($name:ident) => {
        $crate::Guard
    };
}

/// `end!(guard)` closes a span before the end of its scope.
#[cfg(feature = "trace")]
#[macro_export]
macro_rules! end {
    ($guard:expr) => {
        drop($guard)
    };
}

/// Disabled form of `end!`.
#[cfg(not(feature = "trace"))]
#[macro_export]
macro_rules! end {
    ($guard:expr) => {
        let _ = $guard;
    };
}

#[cfg(all(test, feature = "trace"))]
mod tests {
    use super::*;

    #[test]
    fn spans_count_hits_and_nested_time_is_exclusive() {
        reset();
        for _ in 0..3 {
            let _outer = span!(BENCH_ROUTE);
            let _inner = span!(CURSOR_REFILL);
        }
        let early = span!(SCAN_TAIL);
        end!(early);
        assert_eq!(read(site::BENCH_ROUTE).1, 3);
        assert_eq!(read(site::CURSOR_REFILL).1, 3);
        assert_eq!(read(site::SCAN_TAIL).1, 1);
        assert_eq!(read(site::SCAN_WHITESPACE), (0, 0));
        reset();
        assert_eq!(read(site::BENCH_ROUTE), (0, 0));
    }

    #[test]
    fn names_cover_every_site() {
        assert_eq!(NAMES.len(), SITES);
        assert!(NAMES.iter().all(|name| !name.is_empty()));
    }
}

#[cfg(all(test, not(feature = "trace")))]
mod tests_off {
    use super::*;

    #[test]
    fn disabled_guard_is_zero_sized_and_reads_zero() {
        assert_eq!(core::mem::size_of::<Guard>(), 0);
        assert!(!core::mem::needs_drop::<Guard>());
        let _t = span!(BENCH_ROUTE);
        let early = span!(@ site::SCAN_TAIL);
        end!(early);
        assert_eq!(read(site::BENCH_ROUTE), (0, 0));
    }
}
