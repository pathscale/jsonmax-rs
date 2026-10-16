/// Runtime CPU feature detection that does not need an operating system.
///
/// This used to be `std::is_x86_feature_detected!`, and that macro was the
/// only reason this crate wanted `std` on x86 at all: detection itself is a
/// `cpuid` instruction, which no kernel is involved in. `cpufeatures` issues
/// it directly and caches the answer in an atomic, so a `no_std` build gets
/// the same dispatch a hosted one does rather than falling back to scalar.
mod cpu {
    // Each `cpufeatures::new!` declares its own private module, so the checks
    // are wrapped in functions this module can hand out.
    cpufeatures::new!(avx2_cpu, "avx2");
    cpufeatures::new!(sse42_cpu, "sse4.2");
    cpufeatures::new!(avx512f_cpu, "avx512f");
    cpufeatures::new!(avx512bw_cpu, "avx512bw");
    cpufeatures::new!(avx512vbmi_cpu, "avx512vbmi");
    cpufeatures::new!(avx512vbmi2_cpu, "avx512vbmi2");

    pub(super) fn avx2() -> bool {
        avx2_cpu::get()
    }
    pub(super) fn sse42() -> bool {
        sse42_cpu::get()
    }
    pub(super) fn avx512f() -> bool {
        avx512f_cpu::get()
    }
    pub(super) fn avx512bw() -> bool {
        avx512bw_cpu::get()
    }
    pub(super) fn avx512vbmi() -> bool {
        avx512vbmi_cpu::get()
    }
    pub(super) fn avx512vbmi2() -> bool {
        avx512vbmi2_cpu::get()
    }
}

#[cfg(all(
    avx512_stable,
    any(
    feature = "public_imp",
    // always availabe, except if no-std and no avx512 support
    runtime_detection,
    all(
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vbmi",
        target_feature = "avx512vbmi2"
    )
)))]
pub(crate) mod avx512;

#[cfg(any(
    feature = "public_imp",
    // std: sse 4.2 is available for auto-selection unless avx512 is selected at compile time
    all(
        runtime_detection,
        not(all(
            avx512_stable,
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512vbmi",
            target_feature = "avx512vbmi2"
        ))
    ),
    // no-std: no avx512 -> select avx2
    all(
        not(runtime_detection),
        not(all(
            avx512_stable,
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512vbmi",
            target_feature = "avx512vbmi2"
        )),
        target_feature = "avx2"
    )
))]
pub(crate) mod avx2;

#[cfg(any(
    feature = "public_imp",
    // std: sse 4.2 is available for auto-selection unless avx512 or avx2 are selected at compile time
    all(
        runtime_detection,
            not(any(all(
                avx512_stable,
                target_feature = "avx512f",
                target_feature = "avx512bw",
                target_feature = "avx512vbmi",
                target_feature = "avx512vbmi2"
            ),all(not(avx512_stable), target_feature = "avx2"))),
    ),
    // no-std: no avx512, no avx2 -> select sse4.2
    all(
        not(runtime_detection),
        not(all(
            avx512_stable,
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512vbmi",
            target_feature = "avx512vbmi2"
        )),
        not(target_feature = "avx2"),
        target_feature = "sse4.2"
    )
))]
pub(crate) mod sse42;

// validate_utf8_basic() std: implementation auto-selection

#[cfg(all(
    runtime_detection,
    not(all(
        avx512_stable,
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vbmi",
        target_feature = "avx512vbmi2"
    )),
    not(all(not(avx512_stable), target_feature = "avx2"))
))]
#[inline]
pub(crate) unsafe fn validate_utf8_basic(
    input: &[u8],
) -> core::result::Result<(), crate::basic::Utf8Error> {
    use super::helpers::SIMD_CHUNK_SIZE;
    use core::mem;
    use core::sync::atomic::{AtomicPtr, Ordering};

    type FnRaw = *mut ();
    type ValidateUtf8Fn = unsafe fn(input: &[u8]) -> Result<(), crate::basic::Utf8Error>;

    #[cfg(avx512_stable)]
    #[inline]
    fn get_avx512_implementation() -> Option<ValidateUtf8Fn> {
        // Test for avx512vbmi2 to make sure we have a newer CPU with a non-throttling AVX-512 implementation
        if cpu::avx512f() && cpu::avx512bw() && cpu::avx512vbmi() && cpu::avx512vbmi2() {
            return Some(avx512::validate_utf8_basic);
        }
        None
    }

    #[cfg(not(avx512_stable))]
    #[inline]
    fn get_avx512_implementation() -> Option<ValidateUtf8Fn> {
        None
    }

    #[flexpect::e(clippy::option_if_let_else)]
    #[inline]
    fn get_fastest_available_implementation_basic() -> ValidateUtf8Fn {
        if let Some(fun) = get_avx512_implementation() {
            fun
        } else if cpu::avx2() {
            avx2::validate_utf8_basic
        } else if cpu::sse42() {
            sse42::validate_utf8_basic
        } else {
            super::validate_utf8_basic_fallback
        }
    }

    static FN: AtomicPtr<()> = AtomicPtr::new(get_fastest as FnRaw);

    unsafe fn get_fastest(input: &[u8]) -> core::result::Result<(), crate::basic::Utf8Error> {
        let fun = get_fastest_available_implementation_basic();
        FN.store(fun as FnRaw, Ordering::Relaxed);
        (fun)(input)
    }

    if input.len() < SIMD_CHUNK_SIZE {
        return super::validate_utf8_basic_fallback(input);
    }

    let fun = FN.load(Ordering::Relaxed);
    mem::transmute::<FnRaw, ValidateUtf8Fn>(fun)(input)
}

// validate_utf8_basic() no-std: implementation selection by config

#[cfg(all(
    avx512_stable,
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512vbmi",
    target_feature = "avx512vbmi2"
))]
#[inline]
pub(crate) unsafe fn validate_utf8_basic(
    input: &[u8],
) -> core::result::Result<(), crate::basic::Utf8Error> {
    validate_utf8_basic_avx512(input)
}

#[cfg(all(
    avx512_stable,
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512vbmi",
    target_feature = "avx512vbmi2"
))]
#[inline(never)]
unsafe fn validate_utf8_basic_avx512(
    input: &[u8],
) -> core::result::Result<(), crate::basic::Utf8Error> {
    avx512::validate_utf8_basic(input)
}

#[cfg(any(
    all(
        not(runtime_detection),
        not(all(
            avx512_stable,
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512vbmi",
            target_feature = "avx512vbmi2"
        )),
        target_feature = "avx2"
    ),
    all(target_feature = "avx2", runtime_detection, not(avx512_stable))
))]
#[inline]
pub(crate) unsafe fn validate_utf8_basic(
    input: &[u8],
) -> core::result::Result<(), crate::basic::Utf8Error> {
    if input.len() < super::helpers::SIMD_CHUNK_SIZE {
        return super::validate_utf8_basic_fallback(input);
    }

    validate_utf8_basic_avx2(input)
}

#[cfg(any(
    all(
        not(runtime_detection),
        not(all(
            avx512_stable,
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512vbmi",
            target_feature = "avx512vbmi2"
        )),
        target_feature = "avx2"
    ),
    all(target_feature = "avx2", runtime_detection, not(avx512_stable))
))]
#[inline(never)]
unsafe fn validate_utf8_basic_avx2(
    input: &[u8],
) -> core::result::Result<(), crate::basic::Utf8Error> {
    avx2::validate_utf8_basic(input)
}

#[cfg(all(
    not(runtime_detection),
    not(all(
        avx512_stable,
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vbmi",
        target_feature = "avx512vbmi2"
    )),
    not(target_feature = "avx2"),
    target_feature = "sse4.2"
))]
#[inline]
pub(crate) unsafe fn validate_utf8_basic(
    input: &[u8],
) -> core::result::Result<(), crate::basic::Utf8Error> {
    if input.len() < super::helpers::SIMD_CHUNK_SIZE {
        return super::validate_utf8_basic_fallback(input);
    }

    validate_utf8_basic_sse42(input)
}

#[cfg(all(
    not(runtime_detection),
    not(all(
        avx512_stable,
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vbmi",
        target_feature = "avx512vbmi2"
    )),
    not(target_feature = "avx2"),
    target_feature = "sse4.2"
))]
#[inline(never)]
unsafe fn validate_utf8_basic_sse42(
    input: &[u8],
) -> core::result::Result<(), crate::basic::Utf8Error> {
    sse42::validate_utf8_basic(input)
}

#[cfg(all(
    not(runtime_detection),
    not(all(
        avx512_stable,
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vbmi",
        target_feature = "avx512vbmi2"
    )),
    not(target_feature = "avx2"),
    not(target_feature = "sse4.2")
))]
pub(crate) use super::validate_utf8_basic_fallback as validate_utf8_basic;

// validate_utf8_compat() std: implementation auto-selection

#[cfg(all(
    runtime_detection,
    not(all(
        avx512_stable,
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vbmi",
        target_feature = "avx512vbmi2"
    )),
    not(all(not(avx512_stable), target_feature = "avx2"))
))]
#[inline]
pub(crate) unsafe fn validate_utf8_compat(
    input: &[u8],
) -> core::result::Result<(), crate::compat::Utf8Error> {
    use super::helpers::SIMD_CHUNK_SIZE;
    use core::mem;
    use core::sync::atomic::{AtomicPtr, Ordering};

    type FnRaw = *mut ();
    type ValidateUtf8CompatFn = unsafe fn(input: &[u8]) -> Result<(), crate::compat::Utf8Error>;

    #[cfg(avx512_stable)]
    #[inline]
    fn get_avx512_implementation() -> Option<ValidateUtf8CompatFn> {
        // Test for avx512vbmi2 to make sure we have a newer CPU with a non-throttling AVX-512 implementation
        if cpu::avx512f() && cpu::avx512bw() && cpu::avx512vbmi() && cpu::avx512vbmi2() {
            return Some(avx512::validate_utf8_compat);
        }
        None
    }

    #[cfg(not(avx512_stable))]
    #[inline]
    fn get_avx512_implementation() -> Option<ValidateUtf8CompatFn> {
        None
    }

    #[flexpect::e(clippy::option_if_let_else)]
    #[inline]
    fn get_fastest_available_implementation_compat() -> ValidateUtf8CompatFn {
        if let Some(fun) = get_avx512_implementation() {
            fun
        } else if cpu::avx2() {
            avx2::validate_utf8_compat
        } else if cpu::sse42() {
            sse42::validate_utf8_compat
        } else {
            super::validate_utf8_compat_fallback
        }
    }

    static FN: AtomicPtr<()> = AtomicPtr::new(get_fastest as FnRaw);

    unsafe fn get_fastest(input: &[u8]) -> core::result::Result<(), crate::compat::Utf8Error> {
        let fun = get_fastest_available_implementation_compat();
        FN.store(fun as FnRaw, Ordering::Relaxed);
        (fun)(input)
    }

    if input.len() < SIMD_CHUNK_SIZE {
        return super::validate_utf8_compat_fallback(input);
    }

    let fun = FN.load(Ordering::Relaxed);
    mem::transmute::<FnRaw, ValidateUtf8CompatFn>(fun)(input)
}

// validate_utf8_basic() no-std: implementation selection by config

#[cfg(all(
    avx512_stable,
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512vbmi",
    target_feature = "avx512vbmi2"
))]
#[inline]
pub(crate) unsafe fn validate_utf8_compat(
    input: &[u8],
) -> core::result::Result<(), crate::compat::Utf8Error> {
    validate_utf8_compat_avx512(input)
}

#[cfg(all(
    avx512_stable,
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512vbmi",
    target_feature = "avx512vbmi2"
))]
#[inline(never)]
unsafe fn validate_utf8_compat_avx512(
    input: &[u8],
) -> core::result::Result<(), crate::compat::Utf8Error> {
    avx512::validate_utf8_compat(input)
}

#[cfg(any(
    all(
        not(runtime_detection),
        not(all(
            avx512_stable,
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512vbmi",
            target_feature = "avx512vbmi2"
        )),
        target_feature = "avx2"
    ),
    all(target_feature = "avx2", runtime_detection, not(avx512_stable))
))]
#[inline]
pub(crate) unsafe fn validate_utf8_compat(
    input: &[u8],
) -> core::result::Result<(), crate::compat::Utf8Error> {
    if input.len() < super::helpers::SIMD_CHUNK_SIZE {
        return super::validate_utf8_compat_fallback(input);
    }

    validate_utf8_compat_avx2(input)
}

#[cfg(any(
    all(
        not(runtime_detection),
        not(all(
            avx512_stable,
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512vbmi",
            target_feature = "avx512vbmi2"
        )),
        target_feature = "avx2"
    ),
    all(target_feature = "avx2", runtime_detection, not(avx512_stable))
))]
#[inline(never)]
unsafe fn validate_utf8_compat_avx2(
    input: &[u8],
) -> core::result::Result<(), crate::compat::Utf8Error> {
    avx2::validate_utf8_compat(input)
}

#[cfg(all(
    not(runtime_detection),
    not(all(
        avx512_stable,
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vbmi",
        target_feature = "avx512vbmi2"
    )),
    not(target_feature = "avx2"),
    target_feature = "sse4.2"
))]
#[inline]
pub(crate) unsafe fn validate_utf8_compat(
    input: &[u8],
) -> core::result::Result<(), crate::compat::Utf8Error> {
    if input.len() < super::helpers::SIMD_CHUNK_SIZE {
        return super::validate_utf8_compat_fallback(input);
    }

    validate_utf8_compat_sse42(input)
}

#[cfg(all(
    not(runtime_detection),
    not(all(
        avx512_stable,
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vbmi",
        target_feature = "avx512vbmi2"
    )),
    not(target_feature = "avx2"),
    target_feature = "sse4.2"
))]
#[inline(never)]
pub(crate) unsafe fn validate_utf8_compat_sse42(
    input: &[u8],
) -> core::result::Result<(), crate::compat::Utf8Error> {
    sse42::validate_utf8_compat(input)
}

#[cfg(all(
    not(runtime_detection),
    not(all(
        avx512_stable,
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vbmi",
        target_feature = "avx512vbmi2"
    )),
    not(target_feature = "avx2"),
    not(target_feature = "sse4.2")
))]
pub(crate) use super::validate_utf8_compat_fallback as validate_utf8_compat;
