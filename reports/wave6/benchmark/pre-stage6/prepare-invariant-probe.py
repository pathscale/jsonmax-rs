"""Creates an instrumented SOURCE COPY for untimed invariants; timed sources stay frozen."""
from pathlib import Path
import tarfile,shutil,difflib
O=Path(__file__).resolve().parent; D=O/'invariant-source';D.mkdir(exist_ok=True)
with tarfile.open(O/'source-snapshot.tar.gz') as t:t.extractall(D,filter='data')
R=D/'jsonmax-rs'
p=R/'Cargo.toml';s=p.read_text()
for repo in ['ps-rusty_alloc','ps-simdutf8','nagoya','nago-http','nago-wss']:
 s=s.replace('path = "../'+repo+'"','path = "/Users/revenge/code/'+repo+'"')
p.write_text(s)
p=R/'crates/jmax-syntax/src/lib.rs';p.write_text(p.read_text()+'''\n/// Untimed Stage 5 instrumentation, present only in this archived source copy.
pub mod wave6_stats {
 use core::sync::atomic::{AtomicUsize,Ordering};
 pub static CURSOR_BLOCKS:AtomicUsize=AtomicUsize::new(0);
 pub static CURSOR_BYTES:AtomicUsize=AtomicUsize::new(0);
 pub static DECODE_BLOCKS:AtomicUsize=AtomicUsize::new(0);
 pub static DECODE_BYTES:AtomicUsize=AtomicUsize::new(0);
 pub static QUERY_SKIP_BYTES:AtomicUsize=AtomicUsize::new(0);
 pub static STRICT_SKIP_BYTES:AtomicUsize=AtomicUsize::new(0);
 pub fn reset(){for a in [&CURSOR_BLOCKS,&CURSOR_BYTES,&DECODE_BLOCKS,&DECODE_BYTES,&QUERY_SKIP_BYTES,&STRICT_SKIP_BYTES]{a.store(0,Ordering::Relaxed);}}
 pub fn read()->[usize;6]{[CURSOR_BLOCKS.load(Ordering::Relaxed),CURSOR_BYTES.load(Ordering::Relaxed),DECODE_BLOCKS.load(Ordering::Relaxed),DECODE_BYTES.load(Ordering::Relaxed),QUERY_SKIP_BYTES.load(Ordering::Relaxed),STRICT_SKIP_BYTES.load(Ordering::Relaxed)]}
}
''')
p=R/'crates/jmax-syntax/src/block_cursor.rs';before=p.read_text();s=before
s=s.replace('            let masks = if bytes.len() == 64 {','            crate::wave6_stats::CURSOR_BLOCKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);\n            crate::wave6_stats::CURSOR_BYTES.fetch_add(bytes.len(), core::sync::atomic::Ordering::Relaxed);\n            let masks = if bytes.len() == 64 {')
s=s.replace('        let masks = if block.len() == 64 {','        crate::wave6_stats::DECODE_BLOCKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);\n        crate::wave6_stats::DECODE_BYTES.fetch_add(block.len(), core::sync::atomic::Ordering::Relaxed);\n        let masks = if block.len() == 64 {')
s=s.replace('            Ok(Span32::new(start, self.offset() - start))','''            let len=self.offset()-start;
            match mode {
                SkipMode::QueryOnly => {crate::wave6_stats::QUERY_SKIP_BYTES.fetch_add(len as usize,core::sync::atomic::Ordering::Relaxed);},
                SkipMode::StrictValidate => {crate::wave6_stats::STRICT_SKIP_BYTES.fetch_add(len as usize,core::sync::atomic::Ordering::Relaxed);},
                _=>{}
            }
            Ok(Span32::new(start, len))''',1)
assert s!=before;p.write_text(s)
(O/'invariant-instrumentation.patch').write_text(''.join(difflib.unified_diff(before.splitlines(True),s.splitlines(True),fromfile='timed/block_cursor.rs',tofile='instrumented/block_cursor.rs')))
P=O/'invariant-probe';(P/'src').mkdir(parents=True,exist_ok=True)
shutil.copy2(O/'workspace.Cargo.lock',P/'Cargo.lock')
(P/'Cargo.toml').write_text(f'''[package]
name="wave6-invariant-probe"
version="0.1.0"
edition="2024"
[workspace]
[dependencies]
jmax-bench={{path="{R}/crates/jmax-bench"}}
jsonmax={{path="{R}/crates/jsonmax"}}
jmax-syntax={{path="{R}/crates/jmax-syntax"}}
rusty_alloc={{package="ps-rusty_alloc",path="/Users/revenge/code/ps-rusty_alloc"}}
''')
