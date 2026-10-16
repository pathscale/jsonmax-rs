use std::{alloc::{GlobalAlloc,Layout},sync::atomic::{AtomicUsize,Ordering},hint::black_box};
struct Counter;static ALLOCS:AtomicUsize=AtomicUsize::new(0);static BYTES:AtomicUsize=AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counter {
 unsafe fn alloc(&self,l:Layout)->*mut u8 {ALLOCS.fetch_add(1,Ordering::Relaxed);BYTES.fetch_add(l.size(),Ordering::Relaxed);unsafe{rusty_alloc::RustyAlloc.alloc(l)}}
 unsafe fn alloc_zeroed(&self,l:Layout)->*mut u8 {ALLOCS.fetch_add(1,Ordering::Relaxed);BYTES.fetch_add(l.size(),Ordering::Relaxed);unsafe{rusty_alloc::RustyAlloc.alloc_zeroed(l)}}
 unsafe fn realloc(&self,p:*mut u8,l:Layout,n:usize)->*mut u8{ALLOCS.fetch_add(1,Ordering::Relaxed);BYTES.fetch_add(n,Ordering::Relaxed);unsafe{rusty_alloc::RustyAlloc.realloc(p,l,n)}}
 unsafe fn dealloc(&self,p:*mut u8,l:Layout){unsafe{rusty_alloc::RustyAlloc.dealloc(p,l)}}
}
#[global_allocator]static GLOBAL:Counter=Counter;
fn snapshot()->(usize,usize){(ALLOCS.load(Ordering::Relaxed),BYTES.load(Ordering::Relaxed))}
fn print_stats(name:&str,mode:&str,len:usize){let s=jmax_syntax::wave6_stats::read();println!("{{\"category\":\"scan-invariants\",\"workload\":\"{name}\",\"mode\":\"{mode}\",\"input_bytes\":{len},\"cursor_blocks\":{},\"cursor_classified_bytes\":{},\"decode_helper_blocks\":{},\"decode_helper_classified_bytes\":{},\"query_skip_call_bytes\":{},\"strict_skip_call_bytes\":{}}}",s[0],s[1],s[2],s[3],s[4],s[5]);}
fn write_corpus(w:&jmax_bench::Workload)->Result<(),Box<dyn std::error::Error>> {
 use std::io::Write;
 let out=std::env::var("WAVE6_CORPUS_DIR")?;std::fs::create_dir_all(&out)?;
 let mut file=std::fs::File::create(format!("{out}/{}.jsonl",w.name))?;
 for p in &w.payloads {file.write_all(p.as_slice())?;file.write_all(b"\n")?;}
 Ok(())
}
fn main()->Result<(),Box<dyn std::error::Error>> {
 for w in jmax_bench::corpus::make_suite(Default::default())? {write_corpus(&w)?;}

 for w in jmax_bench::followup::make_suite(Default::default())? {
  write_corpus(&w)?;let p=&w.payloads[0];jmax_syntax::wave6_stats::reset();black_box(jsonmax::parse::parse_payload(p.clone(),Default::default())?);print_stats(&w.name,"Parse",p.len());
  jmax_syntax::wave6_stats::reset();black_box(jsonmax::endpoint::extract_payload(p.clone(),&w.compiled_schema,&w.selection,Default::default())?);print_stats(&w.name,"Extract-Strict",p.len());
  jmax_syntax::wave6_stats::reset();black_box(jsonmax::endpoint::extract_query_borrowed(&w.compiled_schema,&w.selection,p.as_slice(),Default::default())?);print_stats(&w.name,"Extract-Query-Schema",p.len());
 }
 for case in jmax_bench::wave6_stress_cases(32) {
  {use std::io::Write;let directory=std::env::var("WAVE6_CORPUS_DIR")?;let mut file=std::fs::File::create(format!("{directory}/{}.jsonl",case.name))?;for _ in 0..128 {file.write_all(case.payload.as_slice())?;file.write_all(b"\n")?;}}
  let p=case.payload; let path=match case.name {"wide-object"=>"field_0","reordered-fields"=>"head","sparse-large-subtree"=>"selected.id",_=>"s"};
  jmax_syntax::wave6_stats::reset();black_box(jsonmax::parse::parse_payload(p.clone(),Default::default())?);print_stats(case.name,"Parse",p.len());
  let q=jsonmax::query::compile_paths(&[path])?;
  for mode in [jsonmax::QueryMode::Strict,jsonmax::QueryMode::Query] {
   jmax_syntax::wave6_stats::reset();let result=jsonmax::query::execute_borrowed(&q,p.as_slice(),jsonmax::QueryOptions{mode,..Default::default()})?;assert!(!result.matches()[0].is_empty());black_box(result);
   print_stats(case.name,if mode==jsonmax::QueryMode::Strict {"Path-Strict"}else{"Path-Query"},p.len());
  }
 }
 for (case_index,name) in [(4,"wide-object"),(2,"escape-dense")] {
  let mut scratch=jsonmax::DomScratch::new();
  for scale in [1,32,32,1] {
   let case=jmax_bench::wave6_stress_cases(scale).remove(case_index);
   let before=snapshot();let d=jsonmax::parse::parse_payload_with_scratch(case.payload,Default::default(),&mut scratch)?;black_box(&d);d.recycle(&mut scratch);let after=snapshot();
   println!("{{\"category\":\"growth\",\"workload\":\"{name}\",\"scale\":{scale},\"allocs\":{},\"requested_bytes\":{},\"tape_capacity_bytes\":{},\"decoded_capacity_bytes\":{}}}",after.0-before.0,after.1-before.1,scratch.tape_capacity()*8,scratch.decoded_capacity());
  }
 }
 let raw=br#"{"id":7}"#;
 let bytes=raw.to_vec();let ptr=bytes.as_ptr();let before=snapshot();let payload=jsonmax::Payload::from_vec(bytes);let after=snapshot();assert_eq!(payload.as_slice().as_ptr(),ptr);assert_eq!(before,after);
 println!("{{\"category\":\"owner-adoption\",\"owner\":\"Vec\",\"allocs\":0,\"bytes\":0,\"pointer_identity\":true}}");
 let before=snapshot();let clone1=payload.clone();let after=snapshot();assert_eq!(clone1.as_slice().as_ptr(),ptr);
 println!("{{\"category\":\"owner-clone\",\"owner\":\"heap-Bytes-first-clone\",\"allocs\":{},\"bytes\":{},\"pointer_identity\":true}}",after.0-before.0,after.1-before.1);
 let before=snapshot();let clone2=payload.clone();let after=snapshot();assert_eq!(clone2.as_slice().as_ptr(),ptr);
 println!("{{\"category\":\"owner-clone\",\"owner\":\"heap-Bytes-subsequent-clone\",\"allocs\":{},\"bytes\":{},\"pointer_identity\":true}}",after.0-before.0,after.1-before.1);
 black_box((clone1,clone2));

 let owner=std::sync::Arc::new(raw.to_vec());let ptr=owner.as_ptr();let before=snapshot();let payload=jsonmax::Payload::from_arc_owner(owner)?;let after=snapshot();assert_eq!(payload.as_slice().as_ptr(),ptr);assert_eq!(before,after);
 println!("{{\"category\":\"owner-adoption\",\"owner\":\"Arc-T\",\"allocs\":0,\"bytes\":0,\"pointer_identity\":true}}");
 Ok(())
}
