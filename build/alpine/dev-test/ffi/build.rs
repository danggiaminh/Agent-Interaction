// Compiles a C source with cc and archives it with ar (no crates needed), then links it into the
// Rust binary: the C compiler, archiver, linker and rustc working together.
use std::{env, path::PathBuf, process::Command};

fn run(c: &mut Command) {
    let status = c.status().expect("cannot start tool");
    assert!(status.success(), "{:?} failed", c);
}

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let obj = out.join("answer.o");
    let lib = out.join("libanswer.a");
    run(Command::new("cc")
        .args(["-c", "-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&obj)
        .arg("src/answer.c"));
    run(Command::new("ar").arg("crs").arg(&lib).arg(&obj));
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=answer");
    println!("cargo:rerun-if-changed=src/answer.c");
}
