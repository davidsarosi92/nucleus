//! Embeds every `slt/*.slt` script (see `core_scripts`).

use std::fmt::Write;
use std::path::Path;

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("slt");
    println!("cargo::rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("slt/")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".slt"))
        .collect();
    names.sort();
    let mut out = String::from("&[\n");
    for n in &names {
        let path = dir.join(n);
        writeln!(
            out,
            "    ({n:?}, include_str!({:?})),",
            path.display().to_string()
        )
        .unwrap();
    }
    out.push_str("]\n");
    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("scripts.rs");
    std::fs::write(dest, out).unwrap();
}
