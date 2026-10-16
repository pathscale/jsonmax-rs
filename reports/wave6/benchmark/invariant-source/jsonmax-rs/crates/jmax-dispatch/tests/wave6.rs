use jmax_dispatch::{BackendId, DispatchPolicy, Dispatcher, ScanCarry};

#[test]
fn cached_bindings_match_checked_entry_points_and_full_blocks() {
    let mut bytes = [b'x'; 64];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = [b'"', b'\\', b' ', b'[', b'}', 0x80, b'7'][i % 7];
    }
    let automatic = Dispatcher::new(16384, DispatchPolicy::default()).unwrap();
    assert_eq!(
        automatic.backend(),
        jmax_dispatch::dispatch(16384, DispatchPolicy::default())
            .unwrap()
            .backend()
    );
    for backend in [
        BackendId::Scalar,
        BackendId::Neon,
        BackendId::Avx2,
        BackendId::Avx512,
    ] {
        let resolved = jmax_dispatch::portable::resolve(backend);
        if !jmax_dispatch::detect::is_available(backend) {
            assert!(resolved.is_err());
            continue;
        }
        let dispatcher = resolved.unwrap();
        let reference: jmax_dispatch::ClassifyFn = match backend {
            BackendId::Scalar => jmax_scan_scalar::classify,
            BackendId::Neon => jmax_scan_neon::classify,
            BackendId::Avx2 => jmax_scan_avx2::classify::classify,
            BackendId::Avx512 => jmax_scan_avx512::classify::classify,
        };
        for len in 0..=64 {
            let mut a = ScanCarry::default();
            let mut b = a;
            let actual = dispatcher.classify(&bytes[..len], &mut a).unwrap();
            assert_eq!(actual, reference(&bytes[..len], &mut b).unwrap());
            assert_eq!(a, b);
            if len == 64 {
                let mut c = ScanCarry::default();
                assert_eq!(actual, dispatcher.classify_block(&bytes, &mut c).unwrap());
                assert_eq!(a, c);
            }
        }
    }
}

// Check actual native loads at the end of readable memory, including an empty
// slice whose pointer is exactly the first byte of the inaccessible page.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn native_and_scalar_masks_agree_at_guard_page_for_every_tail() {
    use std::ffi::{c_int, c_void};
    unsafe extern "C" {
        fn getpagesize() -> c_int;
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }
    struct Pages {
        ptr: *mut u8,
        len: usize,
    }
    impl Drop for Pages {
        fn drop(&mut self) {
            // SAFETY: this mapping is owned solely by this guard.
            unsafe {
                assert_eq!(munmap(self.ptr.cast(), self.len), 0);
            }
        }
    }
    // SAFETY: allocate two pages, protect only the second, and initialize and
    // borrow only the suffix of the first. All slices die before Pages drops.
    unsafe {
        let page = usize::try_from(getpagesize()).unwrap();
        assert!(page >= 256);
        let anonymous = if cfg!(target_os = "macos") {
            0x1000
        } else {
            0x20
        };
        let ptr = mmap(std::ptr::null_mut(), page * 2, 3, 2 | anonymous, -1, 0);
        assert_ne!(ptr as isize, -1);
        let pages = Pages {
            ptr: ptr.cast(),
            len: page * 2,
        };
        assert_eq!(mprotect(pages.ptr.add(page).cast(), page, 0), 0);
        for backend in [
            BackendId::Scalar,
            BackendId::Neon,
            BackendId::Avx2,
            BackendId::Avx512,
        ] {
            let Ok(dispatch) = jmax_dispatch::portable::resolve(backend) else {
                continue;
            };
            for blocks in 0..=3 {
                for tail in 0..=64 {
                    let len = blocks * 64 + tail;
                    let start = pages.ptr.add(page - len);
                    for i in 0..len {
                        *start.add(i) = b"\\\"{}[]:, \t\n\r09\x01\xff"[i % 16];
                    }
                    let input = std::slice::from_raw_parts(start, len);
                    for incoming in 0..4 {
                        let mut native = ScanCarry {
                            prev_in_string: incoming & 1,
                            prev_escaped: incoming >> 1,
                            ..ScanCarry::default()
                        };
                        let mut scalar = native;
                        // chunks() omits the empty tail, so exercise it explicitly.
                        for chunk in input.chunks(64).chain(std::iter::once(&input[len..])) {
                            let actual = dispatch.classify(chunk, &mut native).unwrap();
                            let mut expected =
                                jmax_scan_scalar::classify(chunk, &mut scalar).unwrap();
                            expected.backend = actual.backend;
                            assert_eq!(actual, expected, "{backend:?}: len {len}");
                            assert_eq!(native, scalar);
                        }
                    }
                }
            }
        }
    }
}
