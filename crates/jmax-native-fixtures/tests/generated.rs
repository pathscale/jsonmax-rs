//! Generated decoders against the schema VM and the tree oracle.

use jmax_native_fixtures::harness::*;
use jmax_native_fixtures::regen::{BINANCE, SHAPES};
use jmax_native_fixtures::{binance, binance_open, shapes, shapes_open};

fn trade_seeds(open: bool) -> Vec<Vec<u8>> {
    let valid = AGG_TRADE.as_bytes().to_vec();
    let mut seeds = vec![
        valid.clone(),
        AGG_TRADE.replace(",\"M\":true", "").into_bytes(),
        AGG_TRADE.replace("0.00100000", "0.00000000").into_bytes(),
        AGG_TRADE.replace("aggTrade", "other").into_bytes(),
        AGG_TRADE.replace("\"e\"", "\"\\u0065\"").replace("BNBBTC", "BNB\\u0042TC").into_bytes(),
        AGG_TRADE.replace(',', " ,\n ").replace(':', " : ").into_bytes(),
        AGG_TRADE.replace("\"m\":true", "\"m\":true,\"m\":false").into_bytes(),
        AGG_TRADE.replace("\"a\":12345", "\"a\":-0").into_bytes(),
        AGG_TRADE.replace("\"a\":12345", "\"a\":9223372036854775808").into_bytes(),
        AGG_TRADE.replace("\"a\":12345", "\"a\":1e2").into_bytes(),
    ];
    if open {
        seeds.push(AGG_TRADE.replace("\"m\"", "\"x\":[1,{\"y\":null,\"z\":\"\\u0041\"}],\"m\"").into_bytes());
        seeds.push(AGG_TRADE.replace("\"m\"", "\"x\":1,\"x\":2,\"m\"").into_bytes());
        seeds.push(AGG_TRADE.replace("\"m\"", "\"\\u0078\":1,\"x\":2,\"m\"").into_bytes());
        seeds.push(AGG_TRADE.replace("\"m\"", "\"x\":{\"a\":1,\"a\":2},\"m\"").into_bytes());
    }
    seeds
}

fn depth_seeds() -> Vec<Vec<u8>> {
    let mut seeds: Vec<Vec<u8>> = [0usize, 1, 2, 3, 4, 20, 1000].iter().map(|n| depth_document(*n)).collect();
    seeds.push(String::from_utf8(depth_document(2)).unwrap().replace("\"U\":157", "\"U\":999").into_bytes());
    seeds.push(String::from_utf8(depth_document(2)).unwrap().replace("\"b\"", "\"x\":[[1,2]],\"b\"").into_bytes());
    seeds
}

fn snapshot_seeds() -> Vec<Vec<u8>> {
    let mut seeds: Vec<Vec<u8>> = [0usize, 1, 3, 4, 20, 1000].iter().map(|n| snapshot_document(*n)).collect();
    seeds.push(String::from_utf8(snapshot_document(2)).unwrap().replace("\"bids\"", "\"x\":null,\"bids\"").into_bytes());
    seeds
}

fn report(name: &str, tally: Tally, accepted: u32) {
    assert!(tally.accepted > accepted, "{name}: {tally:?}");
    assert!(tally.rejected > 100, "{name}: {tally:?}");
    eprintln!("{name}: {tally:?}");
}

#[test]
fn strict_agg_trade() {
    let subject = Subject::<binance::models::AggTrade>::new(&BINANCE, "AggTrade", true, oracle_strict::trade);
    report("strict agg_trade", differential(&subject, &trade_seeds(false), 400, 1), 1_000);
}

#[test]
fn strict_depth_update() {
    let subject = Subject::<binance::models::DepthUpdate>::new(&BINANCE, "DepthUpdate", true, oracle_strict::depth);
    report("strict depth_update", differential(&subject, &depth_seeds(), 250, 2), 500);
}

#[test]
fn strict_depth_snapshot() {
    let subject = Subject::<binance::models::DepthSnapshot>::new(&BINANCE, "DepthSnapshot", true, oracle_strict::snapshot);
    report("strict depth_snapshot", differential(&subject, &snapshot_seeds(), 250, 3), 500);
}

#[test]
fn open_agg_trade() {
    let subject = Subject::<binance_open::models::AggTrade>::new(&BINANCE, "AggTrade", false, oracle_open::trade);
    report("open agg_trade", differential(&subject, &trade_seeds(true), 400, 4), 1_000);
}

#[test]
fn open_depth_update() {
    let subject = Subject::<binance_open::models::DepthUpdate>::new(&BINANCE, "DepthUpdate", false, oracle_open::depth);
    report("open depth_update", differential(&subject, &depth_seeds(), 250, 5), 500);
}

#[test]
fn open_depth_snapshot() {
    let subject = Subject::<binance_open::models::DepthSnapshot>::new(&BINANCE, "DepthSnapshot", false, oracle_open::snapshot);
    report("open depth_snapshot", differential(&subject, &snapshot_seeds(), 250, 6), 500);
}

#[test]
fn embedded_programs_equal_the_independently_lowered_ones() {
    use jmax_native::NativeDecode;
    let pairs: [(&str, jmax_native::Result<jmax_native::ValidatedProgram>); 3] = [
        ("AggTrade", binance::models::AggTrade::build_program()),
        ("DepthUpdate", binance::models::DepthUpdate::build_program()),
        ("DepthSnapshot", binance::models::DepthSnapshot::build_program()),
    ];
    for (root, embedded) in pairs {
        let embedded = embedded.unwrap();
        let lowered = reference_program(&BINANCE, root, true);
        assert_eq!(dump(&embedded), dump(&lowered), "{root}");
    }
    let open = binance_open::models::AggTrade::build_program().unwrap();
    assert_eq!(dump(&open), dump(&reference_program(&BINANCE, "AggTrade", false)));
    assert_ne!(dump(&open), dump(&reference_program(&BINANCE, "AggTrade", true)));
}

#[test]
fn seeds_decode_to_the_expected_values() {
    use jmax_abi::number::{Decimal128, NumericPolicy};
    use jmax_native::{decode_into, NativeContext, VmLimits};
    let mut ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    let mut trade = binance::models::AggTrade::default();
    decode_into(&mut ctx, AGG_TRADE.as_bytes(), &mut trade).unwrap();
    assert_eq!(trade.symbol, "BNBBTC");
    assert_eq!(trade.aggregate_trade_id, 12345);
    assert_eq!(trade.price, Decimal128::new(100_000, 8));
    assert_eq!(trade.quantity, Decimal128::new(10_000_000_000, 8));
    assert_eq!(trade.best_match, Some(true));
    assert!(trade.buyer_is_maker);
    let mut depth = binance::models::DepthUpdate::default();
    decode_into(&mut ctx, &depth_document(3), &mut depth).unwrap();
    assert_eq!(depth.bids.len(), 3);
    assert_eq!(depth.asks[2].quantity, Decimal128::new(200_000_014, 8));
    // The context switches programs when the decoder type changes.
    decode_into(&mut ctx, AGG_TRADE.as_bytes(), &mut trade).unwrap();
    // An optional field absent from a later document is reset.
    decode_into(&mut ctx, AGG_TRADE.replace(",\"M\":true", "").as_bytes(), &mut trade).unwrap();
    assert_eq!(trade.best_match, None);
}

fn dump(program: &jmax_native::ValidatedProgram) -> Vec<u8> {
    jmax_schema_aot::dump_program(program).unwrap()
}

fn shape_seeds(open: bool) -> Vec<Vec<u8>> {
    let mut seeds = vec![
        SHAPE.as_bytes().to_vec(),
        SHAPE.replace(",\"note\":\"hi\"", "").into_bytes(),
        SHAPE.replace(",\"extra\":[1,-2]", "").replace(",\"pin\":{\"name\":\"p\"}", "").into_bytes(),
        SHAPE.replace(",\"cost\":\"0.00000001\",\"flag\":false,\"count\":9", "").into_bytes(),
        SHAPE.replace("\"tags\":[{\"name\":\"a\",\"weight\":0.5},{\"name\":\"b\"}]", "\"tags\":[]").into_bytes(),
        SHAPE.replace("\"id\":7", "\"id\":4294967295").into_bytes(),
        SHAPE.replace("\"id\":7", "\"id\":4294967296").into_bytes(),
        SHAPE.replace("\"delta\":-3", "\"delta\":-2147483648").into_bytes(),
        SHAPE.replace("\"ratio\":1.5", "\"ratio\":-0.0e-3").into_bytes(),
        SHAPE.replace("\"ratio\":1.5", "\"ratio\":1e999").into_bytes(),
        SHAPE.replace("\"pairs\":[[1,2],[3,4]]", "\"pairs\":[[1,2,3]]").into_bytes(),
        SHAPE.replace("\"grid\":[[1,2],[],[3]]", "\"grid\":[[[1]]]").into_bytes(),
        SHAPE.replace("\"type\"", "\"t\\u0079pe\"").into_bytes(),
        SHAPE.replace("\"ok\":true", "\"ok\":true,\"ok\":false").into_bytes(),
        SHAPE.replace("\"note\":\"hi\"", "\"note\":\"a\",\"note\":\"bb\"").into_bytes(),
        SHAPE.replace(",\"flag\":false", ",\"flag\":null").into_bytes(),
    ];
    if open {
        seeds.push(SHAPE.replace("\"ok\"", "\"u\":[1,{\"k\":[null,true]}],\"ok\"").into_bytes());
        seeds.push(SHAPE.replace("\"x\":1", "\"zz\":{},\"x\":1").into_bytes());
        seeds.push(SHAPE.replace("{\"name\":\"b\"}", "{\"name\":\"b\",\"w\":1,\"w\":2}").into_bytes());
        seeds.push(SHAPE.replace("\"ok\"", "\"u\":1,\"\\u0075\":2,\"ok\"").into_bytes());
    }
    seeds
}

#[test]
fn strict_shape() {
    let subject = Subject::<shapes::models::Shape>::new(&SHAPES, "Shape", true, shape_strict::shape);
    report("strict shape", differential(&subject, &shape_seeds(false), 600, 7), 1_000);
}

#[test]
fn open_shape() {
    let subject = Subject::<shapes_open::models::Shape>::new(&SHAPES, "Shape", false, shape_open::shape);
    report("open shape", differential(&subject, &shape_seeds(true), 600, 8), 1_000);
}

#[test]
fn shape_seed_decodes_to_the_expected_value() {
    use jmax_abi::number::{Decimal128, NumericPolicy};
    use jmax_native::{decode_into, NativeContext, VmLimits};
    let mut ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    let mut shape = shapes::models::Shape::default();
    decode_into(&mut ctx, SHAPE.as_bytes(), &mut shape).unwrap();
    assert_eq!((shape.id, shape.delta, shape.ratio, shape.ok), (7, -3, 1.5, true));
    assert_eq!(shape.r#type, "circle");
    assert_eq!((shape.origin.x, shape.origin.y), (1, -2));
    assert_eq!(shape.tags.len(), 2);
    assert_eq!((shape.tags[0].name.as_str(), shape.tags[0].weight), ("a", Some(0.5)));
    assert_eq!(shape.tags[1].weight, None);
    assert_eq!(shape.names, ["u", "v\n"]);
    assert_eq!(shape.grid, vec![vec![1, 2], vec![], vec![3]]);
    assert_eq!((shape.pairs[1].low, shape.pairs[1].high), (3, 4));
    assert_eq!(shape.price, Decimal128::new(1_250_000_000, 8));
    assert_eq!(shape.note.as_deref(), Some("hi"));
    assert_eq!(shape.extra, Some(vec![1, -2]));
    assert_eq!(shape.pin.as_ref().map(|tag| tag.name.as_str()), Some("p"));
    assert_eq!(shape.cost, Some(Decimal128::new(1, 8)));
    assert_eq!((shape.flag, shape.count), (Some(false), Some(9)));
    // Optional fields absent from a later document are reset to None.
    let bare = SHAPE
        .replace(",\"note\":\"hi\"", "")
        .replace(",\"extra\":[1,-2]", "")
        .replace(",\"pin\":{\"name\":\"p\"}", "")
        .replace(",\"cost\":\"0.00000001\",\"flag\":false,\"count\":9", "");
    decode_into(&mut ctx, bare.as_bytes(), &mut shape).unwrap();
    assert_eq!((&shape.note, &shape.extra, shape.pin.is_none(), shape.cost, shape.flag, shape.count), (&None, &None, true, None, None, None));
}
