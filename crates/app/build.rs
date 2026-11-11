fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let dir = std::path::Path::new(&manifest)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("vendor/libghostty-vt");
    let so = dir.join("lib/libghostty-vt.so");
    // rust-lld 找不到 -l 名字时，直接把绝对路径的 .so 传给链接器
    println!("cargo:rustc-link-arg={}", so.display());
    println!("cargo:rerun-if-changed={}", so.display());
}
