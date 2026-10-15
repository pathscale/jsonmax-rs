use std::io::{self, BufRead};
fn main() -> io::Result<()> {
    for line in io::stdin().lock().lines() {
        let line=line?;
        if let Some(start)=line.find("_R") {
            let end=line[start..].find(' ').map_or(line.len(),|i|start+i);
            println!("{}{}{}",&line[..start],format_args!("{:#}",rustc_demangle::demangle(&line[start..end])),&line[end..]);
        } else {println!("{line}");}
    }
    Ok(())
}
