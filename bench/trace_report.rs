//! Per-route trace table, compiled in only with the bench crate's `trace` feature.
//! After a route is timed, `report` runs it again for a fixed number of calls with
//! the jmax-trace marks recording, then prints to stderr, for every site hit:
//! hits per call, self ticks per call, and the share of the route's total ticks.
//! Ticks are TSC (x86_64) or cntvct (aarch64) counts, not core cycles. The cost of
//! an empty span, measured here, is subtracted per hit; sites with many tiny hits
//! are still approximate. Routes with no marked site (the rival parsers) print nothing.

#[cfg(feature = "trace")]
pub fn report<E>(label: &str, f: &mut dyn FnMut() -> Result<(), E>) -> Result<(), E> {
    let span_cost = jsonmax::trace::calibrate();
    for _ in 0..16 {
        f()?;
    }
    let started = std::time::Instant::now();
    f()?;
    let one = started.elapsed().as_nanos().max(1) as f64;
    let calls = ((20_000_000.0 / one).ceil() as u64).clamp(20, 20_000);
    jsonmax::trace::reset();
    let begin = jsonmax::trace::now();
    for _ in 0..calls {
        f()?;
    }
    let total_raw = jsonmax::trace::now().wrapping_sub(begin);
    let mut rows: Vec<(&str, u64, u64)> = Vec::new();
    let mut hits_all = 0u64;
    for (index, name) in jsonmax::trace::NAMES.iter().enumerate() {
        let (ticks, hits) = jsonmax::trace::read(jsonmax::trace::Site(index as u16));
        if hits == 0 {
            continue;
        }
        hits_all += hits;
        rows.push((name, hits, ticks.saturating_sub(hits * span_cost)));
    }
    if rows.is_empty() {
        return Ok(());
    }
    let total = total_raw.saturating_sub(hits_all * span_cost).max(1);
    let marked: u64 = rows.iter().map(|r| r.2).sum();
    rows.push(("(unmarked: harness, alloc, glue)", 0, total.saturating_sub(marked)));
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    eprintln!(
        "trace {label}: {calls} calls, {:.0} ticks/call, {:.0} hits/call, empty span {span_cost} ticks (subtracted)",
        total as f64 / calls as f64,
        hits_all as f64 / calls as f64
    );
    eprintln!("  {:<44} {:>10} {:>12} {:>7}", "site", "hits/call", "ticks/call", "share");
    for (name, hits, ticks) in rows {
        eprintln!(
            "  {:<44} {:>10.1} {:>12.1} {:>6.1}%",
            name,
            hits as f64 / calls as f64,
            ticks as f64 / calls as f64,
            ticks as f64 * 100.0 / total as f64
        );
    }
    Ok(())
}

#[cfg(not(feature = "trace"))]
pub fn report<E>(_label: &str, _f: &mut dyn FnMut() -> Result<(), E>) -> Result<(), E> {
    Ok(())
}
