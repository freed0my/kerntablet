//! Compiles the HID-BPF program; the object is embedded into the loader.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let bpf_dir = manifest.join("../../bpf");
    let src = bpf_dir.join("kerntablet.bpf.c");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("kerntablet.bpf.o");

    println!("cargo:rerun-if-changed={}", src.display());
    for h in ["kerntablet.h", "kerntablet-plugin.h", "kt-hid.h"] {
        println!("cargo:rerun-if-changed={}", bpf_dir.join(h).display());
    }

    let clang = env::var("CLANG").unwrap_or_else(|_| "clang".into());
    let status = Command::new(&clang)
        .args(["-O2", "-g", "-target", "bpf", "-D__TARGET_ARCH_x86"])
        .args(["-Wall", "-Wextra", "-Wno-unused-parameter", "-Werror"])
        .arg("-I")
        .arg(&bpf_dir)
        .arg("-c")
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .unwrap_or_else(|e| panic!("failed to run {clang}: {e} (clang is needed to build the BPF program)"));
    assert!(status.success(), "clang failed to compile {}", src.display());

    // Drop DWARF, keep .BTF/.BTF.ext which libbpf needs.
    let _ = Command::new("llvm-strip").arg("-g").arg(&out).status();
}
