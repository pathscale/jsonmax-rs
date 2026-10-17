fn main() {
    for n in [100usize, 1000, 5000, 1_000_000] {
        let r = jmax_dispatch::dispatch(n, Default::default()).map(|d| d.backend());
        println!("{n}: {r:?}");
    }
    println!("avx512 supported: {}", jmax_scan_avx512::is_supported());
    println!("caps {:?}", jmax_dispatch::detect::detect());
}
