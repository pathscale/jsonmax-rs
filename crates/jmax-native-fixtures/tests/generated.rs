//! Generated decoders against the schema VM and the tree oracle.

use jmax_native_fixtures::harness::*;
use jmax_native_fixtures::regen::{BINANCE, FORMATS, SHAPES, SHAPES_NULL};
use jmax_native_fixtures::{binance, binance_open, binance_plain, formats, formats_open, shapes, shapes_null, shapes_null_open, shapes_open};

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
        // Keys out of declaration order, so a predicted key order misses.
        r#"{"M":true,"m":true,"T":123456785,"l":105,"f":100,"q":"100.00000000","p":"0.00100000","a":12345,"s":"BNBBTC","E":123456789,"e":"aggTrade"}"#.as_bytes().to_vec(),
        r#"{"E":123456789,"e":"aggTrade","s":"BNBBTC","a":12345,"q":"100.00000000","p":"0.00100000","f":100,"l":105,"T":123456785,"m":true}"#.as_bytes().to_vec(),
        // A space between a key and its colon, and a key with a prefix of another.
        AGG_TRADE.replace("\"s\":", "\"s\" :").into_bytes(),
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
    seeds.push(br#"{"a":[["1.00000000","2.00000000"]],"b":[],"u":160,"U":157,"s":"BNBBTC","E":123456789,"e":"depthUpdate"}"#.to_vec());
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

// The same strict decoders generated without layout prediction.
#[test]
fn plain_agg_trade() {
    let subject = Subject::<binance_plain::models::AggTrade>::new(&BINANCE, "AggTrade", true, oracle_plain::trade);
    report("plain agg_trade", differential(&subject, &trade_seeds(false), 400, 1), 1_000);
}

#[test]
fn plain_depth_update() {
    let subject = Subject::<binance_plain::models::DepthUpdate>::new(&BINANCE, "DepthUpdate", true, oracle_plain::depth);
    report("plain depth_update", differential(&subject, &depth_seeds(), 250, 2), 500);
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

fn formats_seeds(open: bool) -> Vec<Vec<u8>> {
    let doc = FORMATS_DOC;
    let swap = |from: &str, to: &str| doc.replace(from, to).into_bytes();
    let mut seeds = vec![
        doc.as_bytes().to_vec(),
        swap(",\"nick\":\"nick_ok1\",\"extra\":\"AQID\",\"peer\":\"::1\"", ""),
        // NanoIds: wrong length, wrong alphabet, escapes, the Base62 field with '_' and '-'.
        swap("\"id\":\"abc_DEF-12345678\"", "\"id\":\"abc_DEF-1234567\""),
        swap("\"id\":\"abc_DEF-12345678\"", "\"id\":\"abc_DEF-123456789\""),
        swap("\"id\":\"abc_DEF-12345678\"", "\"id\":\"abc DEF-12345678\""),
        swap("\"id\":\"abc_DEF-12345678\"", "\"id\":\"abc_DEF-1234567\\u0038\""),
        swap("\"id\":\"abc_DEF-12345678\"", "\"id\":\"abc_DEF-1234567\u{e9}\""),
        swap("\"sess\":\"Ab12Cd34Ef56Gh78\"", "\"sess\":\"Ab12Cd34Ef56Gh_8\""),
        swap("\"sess\":\"Ab12Cd34Ef56Gh78\"", "\"sess\":\"Ab12Cd34Ef56Gh-8\""),
        swap("\"sessions\":[\"Zz09Aa12Bb34Cc56\"]", "\"sessions\":[\"Zz09Aa12Bb34Cc_6\"]"),
        swap("\"many\":[\"aaaaaaaaaaaaaaaa\",\"bbbb_bbb-bbbbbbb\"]", "\"many\":[\"aaaaaaaaaaaaaaaa\",\"bbbb_bbb-bbbbbbb\",\"short\"]"),
        swap("\"many\":[\"aaaaaaaaaaaaaaaa\",\"bbbb_bbb-bbbbbbb\"]", "\"many\":[]"),
        swap("\"tag\":\"V1StGXR8_Z5jdHi6B-myT\"", "\"tag\":\"V1StGXR8_Z5jdHi6B-my\""),
        swap("\"nick\":\"nick_ok1\"", "\"nick\":\"nick_ok12\""),
        // UUID, IP address, address and hash.
        swap("123e4567-e89b-12d3-a456-426614174000", "123E4567-E89B-12D3-A456-426614174000"),
        swap("123e4567-e89b-12d3-a456-426614174000", "123e4567e89b12d3a456426614174000"),
        swap("\"ip\":\"192.168.0.1\"", "\"ip\":\"192.168.00.1\""),
        swap("\"ip\":\"192.168.0.1\"", "\"ip\":\"2001:db8::ff00:42:8329\""),
        swap("\"ip\":\"192.168.0.1\"", "\"ip\":\"1::2::3\""),
        swap("\"peer\":\"::1\"", "\"peer\":\"::1%x\""),
        swap("0x00112233445566778899aabbccddeeff00112233\"", "0x00112233445566778899AABBCCDDEEFF00112233\""),
        swap("0x00112233445566778899aabbccddeeff00112233\"", "00112233445566778899aabbccddeeff00112233\""),
        swap("0x00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff", "0x00112233445566778899aabbccddeeff00112233445566778899aabbccddeef"),
        // Bytea.
        swap("\"blob\":\"Zm9vYmE=\"", "\"blob\":\"\""),
        swap("\"blob\":\"Zm9vYmE=\"", "\"blob\":\"Zm9vYg==\""),
        swap("\"blob\":\"Zm9vYmE=\"", "\"blob\":\"Zm9vYmE\""),
        swap("\"blob\":\"Zm9vYmE=\"", "\"blob\":\"Zm9vYmF=\""),
        swap("\"blob\":\"Zm9vYmE=\"", "\"blob\":\"Zm9v\\u0059mE=\""),
        swap("\"extra\":\"AQID\"", "\"extra\":\"AQI=\""),
        swap("\"extra\":\"AQID\"", "\"extra\":\"AQI\""),
        // Out of declaration order, so a predicted order misses.
        r#"{"items":[],"names":[],"sessions":[],"many":[],"tx":"0x00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff","addr":"0x00112233445566778899aabbccddeeff00112233","blob":"","ip":"::","uid":"00000000-0000-0000-0000-000000000000","sess":"aaaaaaaaaaaaaaaa","tag":"aaaaaaaaaaaaaaaaaaaaa","id":"aaaaaaaaaaaaaaaa"}"#.as_bytes().to_vec(),
    ];
    if open {
        seeds.push(swap("\"names\"", "\"x\":[1,{\"y\":null}],\"names\""));
    }
    seeds
}

#[test]
fn strict_formats() {
    let subject = Subject::<formats::models::Ids>::new(&FORMATS, "Ids", true, formats_strict::ids);
    report("strict formats", differential(&subject, &formats_seeds(false), 600, 9), 1_000);
}

#[test]
fn open_formats() {
    let subject = Subject::<formats_open::models::Ids>::new(&FORMATS, "Ids", false, formats_open_oracle::ids);
    report("open formats", differential(&subject, &formats_seeds(true), 600, 10), 1_000);
}

#[test]
fn formats_seed_decodes_to_the_expected_value() {
    use jmax_abi::number::NumericPolicy;
    use jmax_native::{decode_into, NativeContext, VmLimits};
    let mut ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    let mut ids = formats::models::Ids::default();
    decode_into(&mut ctx, FORMATS_DOC.as_bytes(), &mut ids).unwrap();
    assert_eq!(ids.id, "abc_DEF-12345678");
    assert_eq!(ids.blob, b"fooba");
    assert_eq!(ids.many.len(), 2);
    assert_eq!(ids.sessions, ["Zz09Aa12Bb34Cc56"]);
    assert_eq!(ids.items[0].r#ref, "abcd1234");
    assert_eq!(ids.extra.as_deref(), Some(&[1u8, 2, 3][..]));
    // Base62 rejects what the default alphabet accepts; the default one accepts it.
    let sess = FORMATS_DOC.replace("Ab12Cd34Ef56Gh78", "Ab12Cd34Ef56Gh_8");
    assert!(decode_into(&mut ctx, sess.as_bytes(), &mut ids).is_err());
    let id = FORMATS_DOC.replace("abc_DEF-12345678", "abc_DEF_12345678");
    assert!(decode_into(&mut ctx, id.as_bytes(), &mut ids).is_ok());
    // An absent optional is reset by the next document.
    decode_into(&mut ctx, FORMATS_DOC.replace(",\"extra\":\"AQID\"", "").as_bytes(), &mut ids).unwrap();
    assert_eq!(ids.extra, None);
}

fn null_shape_seeds(open: bool) -> Vec<Vec<u8>> {
    let mut seeds = shape_seeds(open);
    let null_for = |key: &str, value: &str| {
        let mut texts = Vec::new();
        for from in [value] {
            texts.push(SHAPE.replace(&format!("\"{key}\":{from}"), &format!("\"{key}\":null")).into_bytes());
        }
        texts
    };
    for (key, value) in [
        ("note", "\"hi\""),
        ("extra", "[1,-2]"),
        ("pin", "{\"name\":\"p\"}"),
        ("cost", "\"0.00000001\""),
        ("flag", "false"),
        ("count", "9"),
        // Required fields do not take null.
        ("id", "7"),
        ("origin", "{\"x\":1,\"y\":-2}"),
        ("tags", "[{\"name\":\"a\",\"weight\":0.5},{\"name\":\"b\"}]"),
        ("price", "\"12.50000000\""),
        ("ok", "true"),
    ] {
        seeds.extend(null_for(key, value));
    }
    // The optional weight inside a tag, nulls in arrays, spellings of null.
    seeds.push(SHAPE.replace("\"weight\":0.5", "\"weight\":null").into_bytes());
    seeds.push(SHAPE.replace("\"names\":[\"u\",\"v\\n\"]", "\"names\":[null]").into_bytes());
    seeds.push(SHAPE.replace("\"note\":\"hi\"", "\"note\":nul").into_bytes());
    seeds.push(SHAPE.replace("\"note\":\"hi\"", "\"note\":nullx").into_bytes());
    seeds.push(SHAPE.replace("\"note\":\"hi\"", "\"note\":Null").into_bytes());
    seeds.push(SHAPE.replace("\"note\":\"hi\"", "\"note\": null ").into_bytes());
    seeds.push(SHAPE.replace("\"note\":\"hi\"", "\"note\":null,\"note\":\"x\"").into_bytes());
    seeds.push(SHAPE.replace("\"note\":\"hi\"", "\"note\":\"x\",\"note\":null").into_bytes());
    seeds.push(SHAPE.replace(",\"note\":\"hi\"", "").replace("\"id\":7", "\"id\":7,\"note\":null").into_bytes());
    seeds
}

#[test]
fn strict_null_shape() {
    let subject = Subject::<shapes_null::models::Shape>::new(&SHAPES_NULL, "Shape", true, shape_null::shape);
    report("strict null shape", differential(&subject, &null_shape_seeds(false), 600, 11), 1_000);
}

#[test]
fn open_null_shape() {
    let subject = Subject::<shapes_null_open::models::Shape>::new(&SHAPES_NULL, "Shape", false, shape_null_open::shape);
    report("open null shape", differential(&subject, &null_shape_seeds(true), 600, 12), 1_000);
}

#[test]
fn null_optionals_decode_as_absent_and_reset_the_output() {
    use jmax_abi::number::NumericPolicy;
    use jmax_native::{decode_into, NativeContext, VmLimits};
    let mut ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    let mut shape = shapes_null::models::Shape::default();
    decode_into(&mut ctx, SHAPE.as_bytes(), &mut shape).unwrap();
    assert_eq!(shape.note.as_deref(), Some("hi"));
    let nulled = SHAPE.replace("\"note\":\"hi\"", "\"note\":null").replace("\"count\":9", "\"count\":null");
    decode_into(&mut ctx, nulled.as_bytes(), &mut shape).unwrap();
    assert_eq!((shape.note.clone(), shape.count), (None, None));
    assert_eq!(shape.flag, Some(false));
    // The strict specification still rejects the same document.
    let mut strict = shapes::models::Shape::default();
    assert!(decode_into(&mut ctx, nulled.as_bytes(), &mut strict).is_err());
    // A required field never takes null.
    let bad = SHAPE.replace("\"id\":7", "\"id\":null");
    assert!(decode_into(&mut ctx, bad.as_bytes(), &mut shape).is_err());
}
