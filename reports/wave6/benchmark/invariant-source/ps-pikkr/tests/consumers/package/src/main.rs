use pikkr::{Pikkr, Result};

fn main() -> Result<()> {
    let mut parser = Pikkr::new(&["$.a"], 1)?;
    assert_eq!(parser.parse(r#"{"a":7}"#)?, vec![Some(&b"7"[..])]);
    Ok(())
}
