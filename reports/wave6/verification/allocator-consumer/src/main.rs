#[global_allocator]
static ALLOCATOR: jsonmax::RustyAlloc = jsonmax::RustyAlloc;

fn main() -> jsonmax::Result<()> {
    let raw = br#"{"text":"\u00e9","n":42}"#;
    let document = jsonmax::parse::parse_owned(raw.to_vec(), Default::default())?;
    let encoded = jsonmax::encode::document_to_vec(&document, Default::default())?;
    let borrowed = jsonmax::parse::parse_borrowed(&encoded, Default::default())?;
    assert_eq!(borrowed.node_count(), document.node_count());
    assert_eq!(borrowed.raw().as_ptr(), encoded.as_ptr());
    Ok(())
}
