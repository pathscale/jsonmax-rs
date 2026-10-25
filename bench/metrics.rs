//! Process-wide hardware counters from libSystem. Deltas include loop and sampling
//! overhead and any other threads in this process; they are not isolated CPU events.
//! Missing/disabled counters are reported as unavailable, never as zero work.
#[derive(Clone, Copy, Debug)]
pub struct Usage {
    pub instructions: u64,
    pub cycles: u64,
}
#[cfg(target_os = "macos")]
pub fn snapshot() -> Option<Usage> {
    // Darwin rusage_info_v4: UUID followed by 35 u64 fields, as declared in
    // <sys/resource.h>. Index 29 is ri_instructions; index 30 is ri_cycles.
    #[repr(C)]
    struct RusageInfoV4 {
        uuid: [u8; 16],
        fields: [u64; 35],
    }
    unsafe extern "C" {
        fn getpid() -> i32;
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut core::ffi::c_void) -> i32;
    }
    let mut info = RusageInfoV4 {
        uuid: [0; 16],
        fields: [0; 35],
    };
    // SAFETY: the zeroed C-layout buffer has exactly the size/alignment required
    // by RUSAGE_INFO_V4 (4); libSystem writes only into it during this call.
    if unsafe { proc_pid_rusage(getpid(), 4, (&mut info as *mut RusageInfoV4).cast()) } != 0 {
        return None;
    }
    Some(Usage {
        instructions: info.fields[29],
        cycles: info.fields[30],
    })
}
#[cfg(not(target_os = "macos"))]
pub fn snapshot() -> Option<Usage> {
    None
}

pub fn per_call(
    before: Option<Usage>,
    after: Option<Usage>,
    calls: usize,
) -> (String, String, String) {
    match (before, after) {
        (Some(a), Some(b)) if calls > 0 => {
            let delta = |x: u64, y: u64| {
                if x > 0 && y > x {
                    format!("{:.1}", (y - x) as f64 / calls as f64)
                } else {
                    "n/a".into()
                }
            };
            (
                delta(a.instructions, b.instructions),
                delta(a.cycles, b.cycles),
                "n/a".into(),
            )
        }
        _ => ("n/a".into(), "n/a".into(), "n/a".into()),
    }
}
