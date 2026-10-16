from pathlib import Path
root=Path('/Users/revenge/code/jsonmax-rs')
p=root/'crates/jmax-bench/Cargo.toml'
s=p.read_text().replace('"jmax-dispatch/std"]','"jmax-dispatch/std", "jmax-nagoya/nagoya-rt"]')
s=s.replace('rusty_alloc = { workspace = true }','rusty_alloc = { workspace = true }\nbytes = { workspace = true }')
p.write_text(s)
p=root/'crates/jmax-bench/src/bin/jmax_bench.rs'
s=p.read_text().replace('struct CountingAllocator;', '#[path = "support/wave6.rs"]\nmod wave6;\n\nstruct CountingAllocator;\nstatic ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);')
s=s.replace('ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);','ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);\n            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);')
s=s.replace('ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);\n        }\n        new_pointer','ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);\n        }\n        new_pointer')
s=s.replace('fn run() -> Result<(), String> {','fn run() -> Result<(), String> {\n    if env::args().nth(1).as_deref() == Some("--wave6") {\n        return wave6::run().map_err(|e| e.to_string());\n    }')
p.write_text(s)
# External counters and per-operation untimed allocation accounting.
p=Path('/tmp/jmax-vs-parsers/src/main.rs'); s=p.read_text()
s=s.replace('static ALLOC_CALLS:', 'static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);\nstatic ALLOC_CALLS:',1)
s=s.replace('ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);','ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);\n        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);',1)
s=s.replace('ZEROED_CALLS.fetch_add(1, Ordering::Relaxed);','ZEROED_CALLS.fetch_add(1, Ordering::Relaxed);\n        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);',1)
s=s.replace('REALLOC_CALLS.fetch_add(1, Ordering::Relaxed);','REALLOC_CALLS.fetch_add(1, Ordering::Relaxed);\n        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);',1)
s=s.replace('reps: usize,','reps: usize,\n    allocs_op: f64,\n    bytes_op: f64,',1).replace('reps: 1,','reps: 1,\n        allocs_op: 0.,\n        bytes_op: 0.,',1)
s=s.replace('job.samples.reserve(samples);','job.samples.reserve(samples);\n            let before = allocation_snapshot();\n            for _ in 0..5 { (job.run)()?; }\n            let after = allocation_snapshot();\n            job.allocs_op = (after.0-before.0) as f64/5.;\n            job.bytes_op = (after.1-before.1) as f64/5.;')
s=s.replace('job.samples.sort_by(f64::total_cmp);','let chronological = job.samples.clone();\n            job.samples.sort_by(f64::total_cmp);')
s=s.replace('"samples_ns":job.samples','"samples_ns":job.samples,"chronological_samples_ns":chronological,"allocs_op":job.allocs_op,"bytes_op":job.bytes_op')
s=s.replace('        output.flush()?;\n    }\n    let allocator', '        prepared_simd(name, b, samples, &mut output)?;\n        output.flush()?;\n    }\n    let allocator')
s += '''
fn allocation_snapshot() -> (u64,u64) {
    (ALLOC_CALLS.load(Ordering::Relaxed)+ZEROED_CALLS.load(Ordering::Relaxed)+REALLOC_CALLS.load(Ordering::Relaxed), ALLOC_BYTES.load(Ordering::Relaxed))
}
// Input refresh is outside each timer; allocation/destruction of the result is inside.
fn prepared_simd(name: &str, b: &[u8], samples: usize, output: &mut impl Write) -> R<()> {
    for tape in [false,true] {
        let mut pool: Vec<Vec<u8>> = (0..8).map(|_|b.to_vec()).collect();
        let mut times = Vec::with_capacity(samples);
        let mut measured_allocs=0; let mut measured_bytes=0;
        for sample in 0..samples+3 {
            for buffer in &mut pool { buffer.copy_from_slice(b); }
            let before = allocation_snapshot();
            let start = Instant::now();
            for buffer in &mut pool {
                if tape { black_box(simd_json::to_tape(black_box(buffer))?); }
                else { black_box(simd_json::to_borrowed_value(black_box(buffer))?); }
            }
            let elapsed=start.elapsed().as_nanos() as f64/8.;
            let after = allocation_snapshot();
            if sample>=3 { times.push(elapsed); measured_allocs+=after.0-before.0; measured_bytes+=after.1-before.1; }
        }
        let chronological=times.clone(); times.sort_by(f64::total_cmp);
        let med=times[(samples-1)/2]; let p95=times[(samples*95).div_ceil(100)-1];
        let parser=if tape {"simd-json-tape-prepared"} else {"simd-json-borrowed-prepared"};
        writeln!(output,"{}",serde_json::json!({"corpus":name,"category":if tape {"A-scan"} else {"B-dom"},"parser":parser,"bytes":b.len(),"median_ns":med,"p95_ns":p95,"mb_s":b.len() as f64*1000./med,"reps":8,"samples_ns":times,"chronological_samples_ns":chronological,"allocs_op":measured_allocs as f64/(samples*8) as f64,"bytes_op":measured_bytes as f64/(samples*8) as f64,"copied_input_bytes_timed":0,"prepared_input_bytes":pool.len()*b.len()}))?;
    }
    Ok(())
}
'''
p.write_text(s)
