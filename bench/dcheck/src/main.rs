fn main() {
    for n in [100usize, 1000, 5000, 1_000_000] {
        let r = jsonmax::dispatch::dispatch(n, Default::default()).map(|d| d.backend());
        println!("{n}: {r:?}");
    }
    println!("avx512 supported: {}", jsonmax::scan_avx512::is_supported());
    println!("caps {:?}", jsonmax::dispatch::detect::detect());
}
