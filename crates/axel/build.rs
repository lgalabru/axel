//! Compiles `syntaxes/` (the languages syntect's bundled set predates: Swift,
//! TypeScript, TOML, Kotlin…) together with syntect's own set into one dump,
//! embedded in the binary. Parsing the grammars needs a YAML reader, which
//! only this build script links: the binary just loads the dump.

use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("syntaxes");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut builder = syntect::parsing::SyntaxSet::load_defaults_newlines().into_builder();
    builder.add_from_folder(&dir, true).unwrap_or_else(|e| panic!("loading {}: {e}", dir.display()));
    let set = builder.build();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("syntaxes.packdump");
    syntect::dumps::dump_to_file(&set, &out).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
}
