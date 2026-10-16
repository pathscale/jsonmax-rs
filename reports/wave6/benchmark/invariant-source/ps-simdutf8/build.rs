fn main() {
    println!("cargo::rustc-check-cfg=cfg(avx512_stable)");
    println!("cargo::rustc-check-cfg=cfg(runtime_detection)");

    // Runtime CPU feature detection, which the x86 dispatch uses to pick an
    // implementation. It used to be spelled `feature = "std"`, because
    // `std::is_x86_feature_detected!` was the only way to ask. `cpufeatures`
    // asks `cpuid` directly, so detection no longer needs an operating system
    // and a `no_std` build gets the same dispatch a hosted one does.
    println!("cargo:rustc-cfg=runtime_detection");
    // `if rustversion::cfg!(...)` is not supported in older Rust versions
    if avx512_stable() {
        println!("cargo:rustc-cfg=avx512_stable");
    }
}

#[rustversion::since(1.89)]
fn avx512_stable() -> bool {
    true
}

#[rustversion::before(1.89)]
fn avx512_stable() -> bool {
    false
}
