// Embed every data/**/*.json (written by tools/dump-js-data.mjs) as a static table.
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read data dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    let mut src = String::from("pub(crate) static FILES: &[(&str, &str)] = &[\n");
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap().with_extension("");
        let key = rel.to_string_lossy().replace('\\', "/");
        src.push_str(&format!("    ({key:?}, include_str!({:?})),\n", f.display().to_string()));
    }
    src.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("files.rs");
    std::fs::write(out, src).expect("write files.rs");
}
