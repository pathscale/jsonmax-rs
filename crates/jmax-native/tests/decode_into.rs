//! decode_into with handwritten decoders shaped like generated ones: the
//! decision flow, the accept set against the VM, public errors against the VM,
//! decoded values against an independent oracle, and zero steady-state allocation.

mod common;

use common::{AGG_TRADE, Json, Rng, depth_document, mutate, tree};
use jmax_abi::number::{Decimal128, DecimalPolicy, NumericPolicy};
use jmax_native::{
    Error, ErrorCode, Fail, NativeContext, NativeDecode, NativeResult, Reader, Result,
    ValidatedProgram, VmLimits, decode_into,
};
use jmax_schema_vm::ValidationScratch;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

struct CountingAllocator;
// SAFETY: forwards every call unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|count| count.set(count.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|count| count.set(count.get() + 1));
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const POLICY: DecimalPolicy = DecimalPolicy::spot_8();

fn constraint(detail: &'static str) -> Error {
    Error::new(ErrorCode::ConstraintViolation, jmax_native::ErrorSource::Decode, 0, detail)
}

#[derive(Clone, Debug, Default, PartialEq)]
struct AggTrade {
    event_type: String,
    event_time_ms: i64,
    symbol: String,
    aggregate_trade_id: i64,
    price: Decimal128,
    quantity: Decimal128,
    first_trade_id: i64,
    last_trade_id: i64,
    trade_time_ms: i64,
    buyer_is_maker: bool,
    best_match: Option<bool>,
}

fn set(target: &mut String, value: &str) {
    target.clear();
    target.push_str(value);
}

impl NativeDecode for AggTrade {
    fn build_program() -> Result<ValidatedProgram> {
        common::program("AggTrade")
    }

    fn decode_core(r: &mut Reader<'_>, ctx: &mut NativeContext, out: &mut Self) -> NativeResult<()> {
        let limits = ctx.limits().syntax;
        if limits.max_depth < 1 {
            return Err(Fail);
        }
        r.expect(b'{')?;
        out.best_match = None;
        let (mut seen, mut fields) = (0u16, 0u32);
        loop {
            let which = match r.key(ctx)? {
                "e" => 0u16, "E" => 1, "s" => 2, "a" => 3, "p" => 4, "q" => 5,
                "f" => 6, "l" => 7, "T" => 8, "m" => 9, "M" => 10,
                _ => return Err(Fail),
            };
            fields += 1;
            if fields > limits.max_fields_per_object
                || (seen & (1 << which) != 0 && limits.reject_duplicate_keys)
            {
                return Err(Fail);
            }
            seen |= 1 << which;
            match which {
                0 => set(&mut out.event_type, r.string(ctx)?),
                1 => out.event_time_ms = r.int(ctx, i64::MIN, i64::MAX)?,
                2 => set(&mut out.symbol, r.string(ctx)?),
                3 => out.aggregate_trade_id = r.int(ctx, i64::MIN, i64::MAX)?,
                4 => out.price = r.decimal(ctx, &POLICY)?,
                5 => out.quantity = r.decimal(ctx, &POLICY)?,
                6 => out.first_trade_id = r.int(ctx, i64::MIN, i64::MAX)?,
                7 => out.last_trade_id = r.int(ctx, i64::MIN, i64::MAX)?,
                8 => out.trade_time_ms = r.int(ctx, i64::MIN, i64::MAX)?,
                9 => out.buyer_is_maker = r.boolean()?,
                _ => out.best_match = Some(r.boolean()?),
            }
            if r.eat(b',') {
                continue;
            }
            r.expect(b'}')?;
            break;
        }
        if seen & 0x3ff != 0x3ff { Err(Fail) } else { Ok(()) }
    }

    fn post_check(&self) -> Result<()> {
        if self.event_type != "aggTrade" {
            return Err(constraint("event tag must be aggTrade"));
        }
        if self.price.coefficient <= 0 || self.quantity.coefficient <= 0 {
            return Err(constraint("price and quantity must be positive"));
        }
        Ok(())
    }
}

fn oracle_trade(input: &[u8]) -> Option<AggTrade> {
    let root = tree(input)?;
    let trade = AggTrade {
        event_type: root.get("e")?.string()?.to_owned(),
        event_time_ms: root.get("E")?.int()?,
        symbol: root.get("s")?.string()?.to_owned(),
        aggregate_trade_id: root.get("a")?.int()?,
        price: root.get("p")?.decimal()?,
        quantity: root.get("q")?.decimal()?,
        first_trade_id: root.get("f")?.int()?,
        last_trade_id: root.get("l")?.int()?,
        trade_time_ms: root.get("T")?.int()?,
        buyer_is_maker: root.get("m")?.bool()?,
        best_match: match root.get("M") { None => None, Some(v) => Some(v.bool()?) },
    };
    Some(trade)
}

#[derive(Clone, Debug, Default, PartialEq)]
struct DepthUpdate {
    event_type: String,
    event_time_ms: i64,
    symbol: String,
    first_update_id: i64,
    final_update_id: i64,
    bids: Vec<(Decimal128, Decimal128)>,
    asks: Vec<(Decimal128, Decimal128)>,
}

fn levels(
    r: &mut Reader<'_>, ctx: &mut NativeContext, out: &mut Vec<(Decimal128, Decimal128)>, depth: u16,
) -> NativeResult<()> {
    let limits = ctx.limits().syntax;
    if depth > limits.max_depth {
        return Err(Fail);
    }
    out.clear();
    r.expect(b'[')?;
    if r.eat(b']') {
        return Ok(());
    }
    let mut count = 0u32;
    loop {
        count += 1;
        // Each level is a two item array one level deeper.
        if count > limits.max_array_items || depth + 1 > limits.max_depth || limits.max_array_items < 2 {
            return Err(Fail);
        }
        r.expect(b'[')?;
        let price = r.decimal(ctx, &POLICY)?;
        r.expect(b',')?;
        let quantity = r.decimal(ctx, &POLICY)?;
        r.expect(b']')?;
        out.push((price, quantity));
        if r.eat(b',') {
            continue;
        }
        r.expect(b']')?;
        return Ok(());
    }
}

impl NativeDecode for DepthUpdate {
    fn build_program() -> Result<ValidatedProgram> {
        common::program("DepthUpdate")
    }

    fn decode_core(r: &mut Reader<'_>, ctx: &mut NativeContext, out: &mut Self) -> NativeResult<()> {
        let limits = ctx.limits().syntax;
        if limits.max_depth < 1 {
            return Err(Fail);
        }
        r.expect(b'{')?;
        let (mut seen, mut fields) = (0u8, 0u32);
        loop {
            let which = match r.key(ctx)? {
                "e" => 0u8, "E" => 1, "s" => 2, "U" => 3, "u" => 4, "b" => 5, "a" => 6,
                _ => return Err(Fail),
            };
            fields += 1;
            if fields > limits.max_fields_per_object
                || (seen & (1 << which) != 0 && limits.reject_duplicate_keys)
            {
                return Err(Fail);
            }
            seen |= 1 << which;
            match which {
                0 => set(&mut out.event_type, r.string(ctx)?),
                1 => out.event_time_ms = r.int(ctx, i64::MIN, i64::MAX)?,
                2 => set(&mut out.symbol, r.string(ctx)?),
                3 => out.first_update_id = r.int(ctx, i64::MIN, i64::MAX)?,
                4 => out.final_update_id = r.int(ctx, i64::MIN, i64::MAX)?,
                5 => levels(r, ctx, &mut out.bids, 2)?,
                _ => levels(r, ctx, &mut out.asks, 2)?,
            }
            if r.eat(b',') {
                continue;
            }
            r.expect(b'}')?;
            break;
        }
        if seen != 0x7f { Err(Fail) } else { Ok(()) }
    }

    fn post_check(&self) -> Result<()> {
        if self.event_type != "depthUpdate" {
            return Err(constraint("event tag must be depthUpdate"));
        }
        if self.first_update_id > self.final_update_id {
            return Err(constraint("first update id exceeds final update id"));
        }
        Ok(())
    }
}

fn oracle_depth(input: &[u8]) -> Option<DepthUpdate> {
    let root = tree(input)?;
    let book = |value: &Json| -> Option<Vec<(Decimal128, Decimal128)>> {
        match value {
            Json::Array(items) => items.iter().map(|level| match level {
                Json::Array(pair) if pair.len() == 2 => Some((pair[0].decimal()?, pair[1].decimal()?)),
                _ => None,
            }).collect(),
            _ => None,
        }
    };
    Some(DepthUpdate {
        event_type: root.get("e")?.string()?.to_owned(),
        event_time_ms: root.get("E")?.int()?,
        symbol: root.get("s")?.string()?.to_owned(),
        first_update_id: root.get("U")?.int()?,
        final_update_id: root.get("u")?.int()?,
        bids: book(root.get("b")?)?,
        asks: book(root.get("a")?)?,
    })
}

fn limit_variants() -> Vec<VmLimits> {
    let base = VmLimits::default();
    let mut variants = vec![base];
    let mut change = |edit: &dyn Fn(&mut VmLimits)| {
        let mut limits = base;
        edit(&mut limits);
        variants.push(limits);
    };
    change(&|l| l.syntax.max_depth = 2);
    change(&|l| l.syntax.max_depth = 3);
    change(&|l| l.syntax.max_fields_per_object = 3);
    change(&|l| l.syntax.max_array_items = 1);
    change(&|l| l.syntax.max_array_items = 2);
    change(&|l| l.syntax.max_string_bytes = 5);
    change(&|l| l.syntax.max_number_digits = 4);
    change(&|l| l.syntax.max_decoded_bytes = 40);
    change(&|l| l.syntax.reject_duplicate_keys = false);
    change(&|l| l.max_steps = 600);
    change(&|l| l.max_scratch_bytes = 3_000);
    variants
}

fn run_vm(program: &ValidatedProgram, input: &[u8], limits: VmLimits) -> std::result::Result<(), Error> {
    jmax_schema_vm::direct::validate_borrowed_into(
        program, input, limits, NumericPolicy::strict_default(), &mut ValidationScratch::default(),
    )
    .map(|_| ())
}

fn resource(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::BudgetExhausted | ErrorCode::DecodedArenaOverflow | ErrorCode::PayloadTooLarge)
}

#[derive(Default)]
struct Tally {
    inputs: u32,
    ok: u32,
    post_failures: u32,
    unsafe_route: u32,
}

fn differential<T: NativeDecode + Default + PartialEq + std::fmt::Debug>(
    root: &str, seeds: &[Vec<u8>], rounds: usize, oracle: impl Fn(&[u8]) -> Option<T>, tally: &mut Tally,
) {
    let program = common::program(root).unwrap();
    let mut rng = Rng(0xfeed_beef_0000_0001 ^ root.len() as u64);
    let mut inputs: Vec<Vec<u8>> = seeds.to_vec();
    for seed in seeds {
        let rounds = if seed.len() > 4_000 { 4 } else { rounds };
        for _ in 0..rounds {
            inputs.push(mutate(seed, &mut rng));
        }
    }
    for limits in limit_variants() {
        let numeric = NumericPolicy::strict_default();
        let mut ctx = NativeContext::new(limits, numeric);
        let mut out = T::default();
        // Install the program once through a first decode of a valid seed.
        let _ = decode_into(&mut ctx, &seeds[0], &mut out);
        for input in &inputs {
            tally.inputs += 1;
            let vm = run_vm(&program, input, limits);
            let safe = jmax_schema_vm::budget_safe(&program, input.len(), limits);

            // The core alone: its accept set equals the VM's.
            let mut core_out = T::default();
            let mut reader = Reader::new(input);
            let core = T::decode_core(&mut reader, &mut ctx, &mut core_out).and_then(|()| reader.end());
            match &vm {
                Ok(()) => assert!(core.is_ok(), "{root}: core rejected an input the VM accepts: {:?}", String::from_utf8_lossy(input)),
                Err(error) if resource(error.code) => {}
                Err(error) => assert!(core.is_err(), "{root}: core accepted an input the VM rejects ({:?}): {:?}", error.code, String::from_utf8_lossy(input)),
            }
            if safe {
                assert_eq!(vm.is_ok(), core.is_ok(), "{root} safe input: {:?}", String::from_utf8_lossy(input));
            } else {
                tally.unsafe_route += 1;
            }

            // The public result: the VM's exact error, or the typed value.
            let mut public_out = T::default();
            let public = decode_into(&mut ctx, input, &mut public_out);
            match (&vm, &public) {
                (Err(expected), Err(got)) => {
                    assert_eq!((got.code, got.byte_offset), (expected.code, expected.byte_offset), "{root}: {:?}", String::from_utf8_lossy(input));
                }
                (Err(expected), Ok(())) => panic!("{root}: public Ok but VM {:?}: {:?}", expected.code, String::from_utf8_lossy(input)),
                (Ok(()), Err(got)) => {
                    assert_eq!(got.code, ErrorCode::ConstraintViolation, "{root}: {:?}", String::from_utf8_lossy(input));
                    assert_eq!(got.source, jmax_native::ErrorSource::Decode);
                    tally.post_failures += 1;
                }
                (Ok(()), Ok(())) => {
                    tally.ok += 1;
                    assert_eq!(Some(public_out), oracle(input), "{root}: {:?}", String::from_utf8_lossy(input));
                }
            }
        }
    }
}

#[test]
fn agg_trade_matches_the_vm_and_the_oracle() {
    let mut tally = Tally::default();
    let valid = AGG_TRADE.as_bytes().to_vec();
    let no_best_match = AGG_TRADE.replace(",\"M\":true", "").into_bytes();
    let zero_price = AGG_TRADE.replace("0.00100000", "0.00000000").into_bytes();
    let wrong_tag = AGG_TRADE.replace("aggTrade", "other").into_bytes();
    let escaped = AGG_TRADE.replace("\"e\"", "\"\\u0065\"").replace("BNBBTC", "BNB\\u0042TC").into_bytes();
    let spaced = AGG_TRADE.replace(',', " ,\n ").replace(':', " : ").into_bytes();
    let seeds = vec![valid, no_best_match, zero_price, wrong_tag, escaped, spaced];
    differential::<AggTrade>("AggTrade", &seeds, 500, oracle_trade, &mut tally);
    assert!(tally.ok > 1_000 && tally.post_failures > 50, "{} {} {}", tally.inputs, tally.ok, tally.post_failures);
    assert!(tally.unsafe_route > 100);
}

#[test]
fn depth_update_matches_the_vm_and_the_oracle() {
    let mut tally = Tally::default();
    let mut seeds: Vec<Vec<u8>> = [0usize, 1, 20, 1000].iter().map(|n| depth_document(*n)).collect();
    seeds.push(String::from_utf8(depth_document(2)).unwrap().replace("\"U\":157", "\"U\":999").into_bytes());
    differential::<DepthUpdate>("DepthUpdate", &seeds, 300, oracle_depth, &mut tally);
    assert!(tally.ok > 500 && tally.post_failures > 5, "{} {} {}", tally.inputs, tally.ok, tally.post_failures);
}

#[test]
fn seeds_decode_to_the_expected_values() {
    let mut ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    let mut trade = AggTrade::default();
    decode_into(&mut ctx, AGG_TRADE.as_bytes(), &mut trade).unwrap();
    assert_eq!(trade.symbol, "BNBBTC");
    assert_eq!(trade.aggregate_trade_id, 12345);
    assert_eq!(trade.price, Decimal128::new(100_000, 8));
    assert_eq!(trade.quantity, Decimal128::new(10_000_000_000, 8));
    assert_eq!(trade.best_match, Some(true));
    assert!(trade.buyer_is_maker);

    let mut depth = DepthUpdate::default();
    decode_into(&mut ctx, &depth_document(3), &mut depth).unwrap();
    assert_eq!(depth.bids.len(), 3);
    assert_eq!(depth.asks[2].1, Decimal128::new(200_000_014, 8));
    // The context switches programs when the decoder type changes.
    decode_into(&mut ctx, AGG_TRADE.as_bytes(), &mut trade).unwrap();
}

#[test]
fn diverged_decoders_are_internal_errors_not_wrong_answers() {
    #[derive(Default)]
    struct Rejecting;
    impl NativeDecode for Rejecting {
        fn build_program() -> Result<ValidatedProgram> { common::program("AggTrade") }
        fn decode_core(_: &mut Reader<'_>, _: &mut NativeContext, _: &mut Self) -> NativeResult<()> { Err(Fail) }
    }
    #[derive(Default)]
    struct Accepting;
    impl NativeDecode for Accepting {
        fn build_program() -> Result<ValidatedProgram> { common::program("AggTrade") }
        fn decode_core(r: &mut Reader<'_>, ctx: &mut NativeContext, _: &mut Self) -> NativeResult<()> {
            r.skip_value(ctx, 0)
        }
    }
    let mut ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    // The VM accepts a valid trade but the decoder says Fail: divergence.
    let error = decode_into(&mut ctx, AGG_TRADE.as_bytes(), &mut Rejecting).unwrap_err();
    assert_eq!(error.code, ErrorCode::InternalError);
    // A genuinely invalid document gets the VM's own error through the same path.
    let error = decode_into(&mut ctx, b"{\"e\":1}", &mut Rejecting).unwrap_err();
    assert_eq!(error.code, ErrorCode::TypeMismatch);
    // In the VM-first route (tiny budget) a VM error wins, then the decoder's Fail diverges.
    let mut tiny = VmLimits::default();
    tiny.max_steps = 100;
    let mut ctx = NativeContext::new(tiny, NumericPolicy::strict_default());
    let error = decode_into(&mut ctx, AGG_TRADE.as_bytes(), &mut Accepting).unwrap_err();
    assert_eq!(error.code, ErrorCode::BudgetExhausted);
}

#[test]
fn steady_state_decoding_allocates_nothing() {
    // One context per decoder type: switching types rebuilds the program.
    let mut ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    let mut book_ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    let mut trade = AggTrade::default();
    let mut depth = DepthUpdate::default();
    let book = depth_document(20);
    let escaped = AGG_TRADE.replace("BNBBTC", "BNB\\u0042TC");
    for _ in 0..2 {
        decode_into(&mut ctx, AGG_TRADE.as_bytes(), &mut trade).unwrap();
        decode_into(&mut ctx, escaped.as_bytes(), &mut trade).unwrap();
        decode_into(&mut book_ctx, &book, &mut depth).unwrap();
    }
    COUNTING.with(|flag| flag.set(true));
    ALLOCATIONS.with(|count| count.set(0));
    for _ in 0..50 {
        decode_into(&mut ctx, AGG_TRADE.as_bytes(), &mut trade).unwrap();
        decode_into(&mut ctx, escaped.as_bytes(), &mut trade).unwrap();
        decode_into(&mut book_ctx, &book, &mut depth).unwrap();
    }
    COUNTING.with(|flag| flag.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
}
