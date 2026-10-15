fn main() {
    let src = "src/bpf/net.bpf.c";
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("net.skel.rs");
    libbpf_cargo::SkeletonBuilder::new()
        .source(src)
        .build_and_generate(&out)
        .expect("compiling the eBPF program (needs clang)");
    println!("cargo:rerun-if-changed={src}");
}
