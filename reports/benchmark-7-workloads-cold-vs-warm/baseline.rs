//! External baseline runner: dependencies deliberately stay outside the workspace.
use std::{alloc::{GlobalAlloc, Layout, System}, sync::atomic::{AtomicBool, AtomicU64, Ordering}, time::Instant, hint::black_box};
use serde::{Deserialize, Serialize};
use jmax_bench::{followup, CorpusConfig, Payload, WorkloadKind as K};
type E = Box<dyn std::error::Error>;
struct CountAlloc;
static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
fn count(bytes: usize) { if TRACK.load(Ordering::Relaxed) { ALLOCS.fetch_add(1, Ordering::Relaxed); BYTES.fetch_add(bytes as u64, Ordering::Relaxed); } }
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 { count(layout.size()); unsafe { System.alloc(layout) } }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 { count(layout.size()); unsafe { System.alloc_zeroed(layout) } }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) { unsafe { System.dealloc(p, layout) } }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 { count(size); unsafe { System.realloc(p, layout, size) } }
}
#[global_allocator] static ALLOCATOR: CountAlloc = CountAlloc;
#[derive(Debug, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
struct Auth { user: String, token: String, expires: u64, admin: bool }
#[derive(Debug, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
struct Order { symbol: String, side: String, price: String, quantity: String, id: u64 }
#[derive(Debug, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
struct Rpc { jsonrpc: String, id: u64, method: String, params: Params }
#[derive(Debug, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
struct Params { account: String, limit: u64, active: bool }
#[derive(Debug, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
struct Catalog { catalog: Vec<Product>, version: u64 }
#[derive(Debug, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
struct Product { id: u64, name: String, price: f64, active: bool }
#[derive(Debug, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
struct Escaped { text: String, id: u64 }
#[derive(Debug, Deserialize)] #[serde(deny_unknown_fields)]
struct Coordinates { coordinates: Vec<f64> }
#[derive(Debug, Deserialize)] #[serde(deny_unknown_fields)]
struct Nested { root: Deep }
#[derive(Debug)] enum Deep { Number(u64), Array(Box<Deep>), Object(Box<Deep>) }
impl<'de> Deserialize<'de> for Deep {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self,D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value=Deep;
            fn expecting(&self,f:&mut std::fmt::Formatter)->std::fmt::Result { f.write_str("a number, singleton array, or v object") }
            fn visit_u64<E:serde::de::Error>(self,v:u64)->Result<Deep,E>{Ok(Deep::Number(v))}
            fn visit_seq<A:serde::de::SeqAccess<'de>>(self,mut a:A)->Result<Deep,A::Error>{
                let v=a.next_element()?.ok_or_else(||serde::de::Error::custom("empty array"))?;
                if a.next_element::<serde::de::IgnoredAny>()?.is_some(){return Err(serde::de::Error::custom("excess element"))}
                Ok(Deep::Array(Box::new(v)))
            }
            fn visit_map<A:serde::de::MapAccess<'de>>(self,mut a:A)->Result<Deep,A::Error>{
                let k:String=a.next_key()?.ok_or_else(||serde::de::Error::custom("empty object"))?;
                if k!="v" {return Err(serde::de::Error::custom("unknown key"))}
                let v=a.next_value()?;
                if a.next_key::<String>()?.is_some(){return Err(serde::de::Error::custom("excess key"))}
                Ok(Deep::Object(Box::new(v)))
            }
        }
        d.deserialize_any(V)
    }
}
macro_rules! typed {
    ($kind:expr,$parser:ident,$input:expr) => {match $kind {
        K::NagoyaAuth=>{black_box($parser::from_slice::<Auth>($input)?);},
        K::ExchangeOrder=>{black_box($parser::from_slice::<Order>($input)?);},
        K::RpcEnvelope=>{black_box($parser::from_slice::<Rpc>($input)?);},
        K::LargeCatalog=>{black_box($parser::from_slice::<Catalog>($input)?);},
        K::VectorCoordinates=>{black_box($parser::from_slice::<Coordinates>($input)?);},
        K::DeepNesting=>{black_box($parser::from_slice::<Nested>($input)?);},
        K::EscapedUnicode=>{black_box($parser::from_slice::<Escaped>($input)?);},
        _=>unreachable!(),
    }}
}
fn tape_typed(kind:K,tape:simd_json::Tape<'_>)->Result<(),E>{
    match kind {
        K::NagoyaAuth=>{black_box(tape.deserialize::<Auth>()?);},
        K::ExchangeOrder=>{black_box(tape.deserialize::<Order>()?);},
        K::RpcEnvelope=>{black_box(tape.deserialize::<Rpc>()?);},
        K::LargeCatalog=>{black_box(tape.deserialize::<Catalog>()?);},
        K::VectorCoordinates=>{black_box(tape.deserialize::<Coordinates>()?);},
        K::DeepNesting=>{black_box(tape.deserialize::<Nested>()?);},
        K::EscapedUnicode=>{black_box(tape.deserialize::<Escaped>()?);}, _=>unreachable!(),
    };Ok(())
}
fn measure<F:FnMut(&Payload)->Result<(),E>>(name:&str, mode:&str, engine:&str, api:&str, regime:&str,
    payloads:&[Payload], zero:bool, mut f:F)->Result<(),E>{
    for _ in 0..3 {for p in payloads {f(black_box(p))?;}}
    ALLOCS.store(0,Ordering::Relaxed);BYTES.store(0,Ordering::Relaxed);TRACK.store(true,Ordering::Relaxed);
    let allocation_result=(||{for p in payloads {f(black_box(p))?;}Ok::<_,E>(())})();
    TRACK.store(false,Ordering::Relaxed);allocation_result?;
    let allocs=ALLOCS.load(Ordering::Relaxed);let allocated=BYTES.load(Ordering::Relaxed);
    if zero && allocs!=0 {return Err(format!("{name}/{mode}: warm allocations = {allocs}").into())}
    let mut times=Vec::with_capacity(20);
    for _ in 0..20 {
        let start=Instant::now();
        for p in payloads {f(black_box(p))?;}
        times.push(start.elapsed().as_nanos() as u64);
    }
    if let Ok(path)=std::env::var("JMAX_RAW") {
        use std::io::Write;
        let mut file=std::fs::OpenOptions::new().append(true).open(path)?;
        for (index,ns) in times.iter().enumerate() {
            writeln!(file,"{name},{mode},{engine},{api},{regime},{index},{ns},{},{},{allocs},{allocated}",payloads.len(),payloads.iter().map(|p|p.len()).sum::<usize>())?;
        }
    }
    let total: u64=times.iter().sum();times.sort_unstable();
    let docs=payloads.len() as f64;let bytes=payloads.iter().map(|p|p.len()).sum::<usize>() as f64;
    let mean=total as f64/(20.0*docs);let median=times[9] as f64/docs;
    println!("| {name} | {mode} | {engine} | {api} | {regime} | {:.1} | {:.1} | {:.1} | {:.1} | {:.2} | {:.3} | {:.1} |",mean,median,times[0] as f64/docs,times[18] as f64/docs,bytes/docs*1000.0/mean,allocs as f64/docs,allocated as f64/docs);
    Ok(())
}
fn main()->Result<(),E>{
    let config=CorpusConfig::default();let suite=followup::make_suite(config)?;
    if let Ok(spec)=std::env::var("JMAX_PROFILE") { return profile(&suite,&spec); }
    if let Ok(path)=std::env::var("JMAX_RAW") {
        std::fs::write(path,"workload,mode,engine,api,regime,pass,elapsed_ns,documents,input_bytes,allocations_per_pass,allocated_bytes_per_pass\n")?;
    }
    if let Ok(path)=std::env::var("JMAX_CORPUS") {
        use std::io::Write;
        std::fs::create_dir_all(&path)?;
        for w in &suite {
            let mut file=std::fs::File::create(std::path::Path::new(&path).join(format!("{}.jsonl",w.name)))?;
            for p in &w.payloads {file.write_all(p.as_slice())?;file.write_all(b"\n")?;}
        }
    }
    preflight(&suite)?;
    println!("128 documents/workload; items=32; seed={}; 3 prewarm passes, 20 measured passes. Allocation counts are a separate untimed pass. ns/doc and decimal MB/s. simd mutable input copying is timed.",config.seed);
    println!("| Workload | Mode | Engine | API / semantics | Regime | Mean ns/doc | Median ns/doc | Min ns/doc | P95 ns/doc | MB/s | allocs/doc | allocated bytes/doc |\n|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|");
    for w in &suite {
        let name=followup::name(w.kind);let ps=&w.payloads;let opts=jsonmax::ParseOptions::default();
        measure(name,"Parse","jmax","DOM","Cold-Alloc",ps,false,|p|{black_box(jsonmax::parse::parse_payload(p.clone(),opts)?);Ok(())})?;
        let mut dom=jsonmax::parse::ParseScratch::default();
        measure(name,"Parse","jmax","DOM","Warm-Scratch Zero-Alloc",ps,true,|p|{black_box(jsonmax::parse::parse_payload_into(p,opts,&mut dom)?);Ok(())})?;
        for extract in [false,true] {
            let mode=if extract{"Extract"}else{"Validate"};
            measure(name,mode,"jmax","strict schema + slots","Cold-Alloc",ps,false,|p|{
                if extract {black_box(jsonmax::endpoint::extract_payload(p.clone(),&w.compiled_schema,&w.selection,opts)?);}
                else {black_box(jsonmax::endpoint::validate_payload(p.clone(),&w.compiled_schema,opts)?);}Ok(())})?;
            let mut scratch=jsonmax::endpoint::ValidationScratch::default();
            measure(name,mode,"jmax","strict schema + slots","Warm-Scratch Zero-Alloc",ps,true,|p|{
                if extract {black_box(jsonmax::endpoint::extract_payload_into(p,&w.compiled_schema,&w.selection,opts,&mut scratch)?);}
                else {black_box(jsonmax::endpoint::validate_payload_into(p,&w.compiled_schema,opts,&mut scratch)?);}Ok(())})?;
        }
        measure(name,"Parse","serde_json","Value","Cold-Alloc",ps,false,|p|{black_box(serde_json::from_slice::<serde_json::Value>(p.as_slice())?);Ok(())})?;
        measure(name,"Validate","serde_json","IgnoredAny (syntax only)","per-call",ps,false,|p|{black_box(serde_json::from_slice::<serde::de::IgnoredAny>(p.as_slice())?);Ok(())})?;
        for mode in ["Validate","Extract"] {
            measure(name,mode,"serde_json","typed Deserialize (shape)","Cold-Alloc",ps,false,|p|{typed!(w.kind,serde_json,p.as_slice());Ok(())})?;
        }
        measure(name,"Parse","simd-json","BorrowedValue + copy","Cold-Alloc",ps,false,|p|{let mut b=p.as_slice().to_vec();black_box(simd_json::to_borrowed_value(&mut b)?);Ok(())})?;
        let mut b=Vec::new();let mut buffers=simd_json::Buffers::default();
        measure(name,"Parse","simd-json","BorrowedValue + copy + Buffers","Warm buffers (DOM allocates)",ps,false,|p|{b.clear();b.extend_from_slice(p.as_slice());black_box(simd_json::to_borrowed_value_with_buffers(&mut b,&mut buffers)?);Ok(())})?;
        measure(name,"Validate","simd-json","Tape (syntax only) + copy","Cold-Alloc",ps,false,|p|{let mut b=p.as_slice().to_vec();black_box(simd_json::to_tape(&mut b)?);Ok(())})?;
        let mut b=Vec::new();let mut buffers=simd_json::Buffers::default();let mut tape=simd_json::Tape::null();
        measure(name,"Validate","simd-json","fill_tape (syntax only) + copy","Warm buffers + tape",ps,false,|p|{
            // reset drops all references before mutating the reused input.
            let mut t=std::mem::replace(&mut tape,simd_json::Tape(Vec::new())).reset();
            b.clear();b.extend_from_slice(p.as_slice());simd_json::fill_tape(&mut b,&mut buffers,&mut t)?;
            black_box(&t);tape=t.reset();Ok(())})?;
        for mode in ["Validate","Extract"] {
            measure(name,mode,"simd-json","Tape::deserialize typed (shape) + copy","Cold-Alloc",ps,false,|p|{
                let mut b=p.as_slice().to_vec();tape_typed(w.kind,simd_json::to_tape(&mut b)?)?;Ok(())})?;
        }
        measure(name,"Parse","sonic-rs","Value","per-call (library TLS)",ps,false,|p|{black_box(sonic_rs::from_slice::<sonic_rs::Value>(p.as_slice())?);Ok(())})?;
        for mode in ["Validate","Extract"] {
            measure(name,mode,"sonic-rs","LazyValue (syntax only, raw root)","per-call (library TLS)",ps,false,|p|{black_box(sonic_rs::from_slice::<sonic_rs::LazyValue>(p.as_slice())?);Ok(())})?;
            measure(name,mode,"sonic-rs","typed Deserialize (shape)","Cold-Alloc",ps,false,|p|{typed!(w.kind,sonic_rs,p.as_slice());Ok(())})?;
        }
        let docs:Vec<_>=ps.iter().map(|p|jsonmax::parse::parse_payload(p.clone(),opts)).collect::<Result<_,_>>()?;
        let mut i=0;
        measure(name,"Serialize","jmax","native DOM emission","Cold-Alloc",ps,false,|_|{black_box(jsonmax::encode::document_to_vec(&docs[i],Default::default())?);i=(i+1)%docs.len();Ok(())})?;
        let mut output=jsonmax::OutputBuffer::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);let mut frames=Default::default();let mut i=0;
        measure(name,"Serialize","jmax","native DOM emission","Warm-Scratch Zero-Alloc",ps,true,|_|{jsonmax::encode::document_to_buffer(&docs[i],&mut output,&mut frames)?;black_box(output.as_bytes());i=(i+1)%docs.len();Ok(())})?;
        let values:Vec<_>=ps.iter().map(|p|serde_json::from_slice::<serde_json::Value>(p.as_slice())).collect::<Result<_,_>>()?;let mut i=0;
        measure(name,"Serialize","serde_json","Value to_vec","Cold-Alloc",ps,false,|_|{black_box(serde_json::to_vec(&values[i])?);i=(i+1)%values.len();Ok(())})?;
        let values:Vec<_>=ps.iter().map(|p|{let mut b=p.as_slice().to_vec();Ok::<_,E>(simd_json::to_borrowed_value(&mut b)?.into_static())}).collect::<Result<_,_>>()?;let mut i=0;
        measure(name,"Serialize","simd-json","BorrowedValue to_vec","Cold-Alloc",ps,false,|_|{black_box(simd_json::to_vec(&values[i])?);i=(i+1)%values.len();Ok(())})?;
        let values:Vec<_>=ps.iter().map(|p|sonic_rs::from_slice::<sonic_rs::Value>(p.as_slice())).collect::<Result<_,_>>()?;let mut i=0;
        measure(name,"Serialize","sonic-rs","Value to_vec","Cold-Alloc",ps,false,|_|{black_box(sonic_rs::to_vec(&values[i])?);i=(i+1)%values.len();Ok(())})?;
    }
    Ok(())
}

fn preflight(suite:&[jmax_bench::Workload])->Result<(),E>{
    for w in suite { for p in &w.payloads {
        let expected:serde_json::Value=serde_json::from_slice(p.as_slice())?;
        let sonic:sonic_rs::Value=sonic_rs::from_slice(p.as_slice())?;
        let mut b=p.as_slice().to_vec();let simd=simd_json::to_borrowed_value(&mut b)?;
        let doc=jsonmax::parse::parse_payload(p.clone(),Default::default())?;
        for serialized in [sonic_rs::to_vec(&sonic)?,simd_json::to_vec(&simd)?,
            jsonmax::encode::document_to_vec(&doc,Default::default())?] {
            let actual:serde_json::Value=serde_json::from_slice(&serialized)?;
            if actual!=expected {return Err(format!("{}/semantic round trip mismatch",w.name).into())}
        }
    }}
    Ok(())
}
fn profile(suite:&[jmax_bench::Workload],spec:&str)->Result<(),E>{
    let parts:Vec<_>=spec.split('/').collect();
    if parts.len()!=3 {return Err("profile needs Workload/Mode/Cold-or-Warm".into())}
    let w=suite.iter().find(|w|followup::name(w.kind)==parts[0]).ok_or("unknown profile workload")?;
    let warm=parts[2]=="Warm";let opts=Default::default();
    let mut dom=Default::default();let mut slots=Default::default();
    let docs:Vec<_>=w.payloads.iter().map(|p|jsonmax::parse::parse_payload(p.clone(),opts)).collect::<Result<_,_>>()?;
    let mut output=jsonmax::OutputBuffer::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);let mut frames=Default::default();
    let start=Instant::now();let mut passes=0;
    while start.elapsed().as_secs()<8 {for (i,p) in w.payloads.iter().enumerate(){
        match (parts[1],warm) {
            ("Parse",true)=>{black_box(jsonmax::parse::parse_payload_into(p,opts,&mut dom)?);},
            ("Parse",false)=>{black_box(jsonmax::parse::parse_payload(p.clone(),opts)?);},
            ("Validate",true)=>{black_box(jsonmax::endpoint::validate_payload_into(p,&w.compiled_schema,opts,&mut slots)?);},
            ("Validate",false)=>{black_box(jsonmax::endpoint::validate_payload(p.clone(),&w.compiled_schema,opts)?);},
            ("Extract",true)=>{black_box(jsonmax::endpoint::extract_payload_into(p,&w.compiled_schema,&w.selection,opts,&mut slots)?);},
            ("Extract",false)=>{black_box(jsonmax::endpoint::extract_payload(p.clone(),&w.compiled_schema,&w.selection,opts)?);},
            ("Serialize",true)=>{jsonmax::encode::document_to_buffer(&docs[i],&mut output,&mut frames)?;black_box(output.as_bytes());},
            ("Serialize",false)=>{black_box(jsonmax::encode::document_to_vec(&docs[i],opts_encode())?);},
            _=>return Err("unknown profile mode".into()),
        }
    }passes+=1;}
    eprintln!("profile {spec}: {passes} passes");Ok(())
}
fn opts_encode()->jsonmax::EncodeOptions {Default::default()}
