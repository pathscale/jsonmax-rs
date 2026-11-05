use jsonmax::trace::{self, site};

#[cfg(feature = "trace")]
#[test]
fn spans_count_hits_and_reset() {
    trace::reset();
    for _ in 0..3 {
        let _outer = jsonmax::span!(@ site::BENCH_ROUTE);
        let _inner = jsonmax::span!(@ site::CURSOR_REFILL);
    }
    let early = jsonmax::span!(@ site::SCAN_TAIL);
    jsonmax::end!(early);

    assert_eq!(trace::read(site::BENCH_ROUTE).1, 3);
    assert_eq!(trace::read(site::CURSOR_REFILL).1, 3);
    assert_eq!(trace::read(site::SCAN_TAIL).1, 1);

    trace::reset();
    assert_eq!(trace::read(site::BENCH_ROUTE), (0, 0));
    assert_eq!(trace::read(site::CURSOR_REFILL), (0, 0));
    assert_eq!(trace::read(site::SCAN_TAIL), (0, 0));
}

#[test]
fn names_cover_every_site() {
    assert_eq!(trace::NAMES.len(), trace::SITES);
    assert!(trace::NAMES.iter().all(|name| !name.is_empty()));
}
