"""Instrument the CURRENT frozen source copy only; never the timed workspace."""
from pathlib import Path
import tarfile, shutil, difflib
O=Path(__file__).resolve().parent
D=O/'invariant-source'; D.mkdir(exist_ok=True)
with tarfile.open(O/'source-snapshot.tar.gz') as t:
    t.extractall(D, filter='data')
R=D/'jsonmax-rs'
p=R/'Cargo.toml'; s=p.read_text()
for repo in ['ps-rusty_alloc','ps-simdutf8','nagoya','nago-http','nago-wss']:
    s=s.replace('path = "../'+repo+'"','path = "/Users/revenge/code/'+repo+'"')
p.write_text(s)
patch=[]
def edit(rel, transform):
    p=R/rel; before=p.read_text(); after=transform(before)
    assert after!=before, rel
    p.write_text(after)
    patch.extend(difflib.unified_diff(before.splitlines(True),after.splitlines(True),fromfile='timed/'+rel,tofile='instrumented/'+rel))
edit('crates/jmax-string/src/lib.rs', lambda s:s+'''
/// Untimed source-copy counters, absent from production and timing builds.
pub mod wave6_stats {
 use core::sync::atomic::{AtomicUsize,Ordering};
 pub static BLOCKS:AtomicUsize=AtomicUsize::new(0);
 pub static BYTES:AtomicUsize=AtomicUsize::new(0);
 pub fn reset(){BLOCKS.store(0,Ordering::Relaxed);BYTES.store(0,Ordering::Relaxed);}
 pub fn read()->[usize;2]{[BLOCKS.load(Ordering::Relaxed),BYTES.load(Ordering::Relaxed)]}
}
''')
needle='                let block = &self.bytes[self.block_base..self.block_end];'
def mask(s):
    assert s.count(needle)==1
    return s.replace(needle, needle+'''
                crate::wave6_stats::BLOCKS.fetch_add(1,core::sync::atomic::Ordering::Relaxed);
                crate::wave6_stats::BYTES.fetch_add(block.len(),core::sync::atomic::Ordering::Relaxed);''')
edit('crates/jmax-string/src/masks.rs',mask)
edit('crates/jmax-syntax/src/lib.rs', lambda s:s+'''
/// Untimed source-copy counters, absent from production and timing builds.
pub mod wave6_stats {
 use core::sync::atomic::{AtomicUsize,Ordering};
 pub static CURSOR_BLOCKS:AtomicUsize=AtomicUsize::new(0);
 pub static CURSOR_BYTES:AtomicUsize=AtomicUsize::new(0);
 pub static QUERY_SKIP_BYTES:AtomicUsize=AtomicUsize::new(0);
 pub static STRICT_SKIP_BYTES:AtomicUsize=AtomicUsize::new(0);
 pub fn reset(){for a in [&CURSOR_BLOCKS,&CURSOR_BYTES,&QUERY_SKIP_BYTES,&STRICT_SKIP_BYTES]{a.store(0,Ordering::Relaxed);}jmax_string::wave6_stats::reset();}
 pub fn read()->[usize;6]{let d=jmax_string::wave6_stats::read();[CURSOR_BLOCKS.load(Ordering::Relaxed),CURSOR_BYTES.load(Ordering::Relaxed),d[0],d[1],QUERY_SKIP_BYTES.load(Ordering::Relaxed),STRICT_SKIP_BYTES.load(Ordering::Relaxed)]}
}
''')
def cursor(s):
    a='            let masks = if bytes.len() == 64 {'
    b='            Ok(Span32::new(start, self.offset() - start))'
    assert s.count(a)==s.count(b)==1
    s=s.replace(a,'''            crate::wave6_stats::CURSOR_BLOCKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::wave6_stats::CURSOR_BYTES.fetch_add(bytes.len(), core::sync::atomic::Ordering::Relaxed);
'''+a)
    return s.replace(b,'''            let len=self.offset()-start;
            match mode {
                SkipMode::QueryOnly => {crate::wave6_stats::QUERY_SKIP_BYTES.fetch_add(len as usize,core::sync::atomic::Ordering::Relaxed);},
                SkipMode::StrictValidate => {crate::wave6_stats::STRICT_SKIP_BYTES.fetch_add(len as usize,core::sync::atomic::Ordering::Relaxed);},
                _=>{}
            }
            Ok(Span32::new(start, len))''')
edit('crates/jmax-syntax/src/block_cursor.rs',cursor)
def query(s):
    needle='    fn compile(paths: &[String]) -> Result<Self> {'
    assert s.count(needle)==1
    return s.replace(needle,needle+'\n        WAVE6_TRIE_BUILDS.fetch_add(1,core::sync::atomic::Ordering::Relaxed);')+'''
/// Untimed source-copy query-plan compilation counter.
pub static WAVE6_TRIE_BUILDS:core::sync::atomic::AtomicUsize=core::sync::atomic::AtomicUsize::new(0);
'''
edit('crates/jsonmax/src/query.rs',query)
(O/'invariant-instrumentation.patch').write_text(''.join(patch))
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
