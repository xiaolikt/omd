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
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../../vendor/libghostty-vt/lib");
    println!("cargo:rerun-if-changed={}", so.display());

    // CEF：libcef.so 在 vendor/cef/<version>/<platform>/Release 下，用 rpath 指过去，
    // 免得每次启动都要手动设 LD_LIBRARY_PATH。platform 目录名和 cef-dll-sys
    // 的 OsAndArch 命名一致（cef_linux_x86_64 等），这里不写死。
    let cef_root = std::path::Path::new(&manifest)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("vendor/cef");
    if let Ok(versions) = std::fs::read_dir(&cef_root) {
        for version in versions.flatten() {
            if let Ok(platforms) = std::fs::read_dir(version.path()) {
                for platform in platforms.flatten() {
                    let release = platform.path().join("Release");
                    if release.join("libcef.so").exists() || release.join("libcef.dll").exists() {
                        println!("cargo:rustc-link-search=native={}", release.display());
                        println!(
                            "cargo:rustc-link-arg=-Wl,-rpath,{}/Release",
                            platform.path().display()
                        );
                    }
                }
            }
        }
    }
    // CEF 在 Linux 上按「可执行文件旁边」找 icudtl.dat / *.pak / locales，
    // resources_dir_path 管不到 ICU 这一步。把运行时文件拷到 target 目录。
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    if let Some(target_dir) = std::path::Path::new(&out_dir).ancestors().nth(3) {
        if let Ok(versions) = std::fs::read_dir(&cef_root) {
            for version in versions.flatten() {
                if let Ok(platforms) = std::fs::read_dir(version.path()) {
                    for platform in platforms.flatten() {
                        for sub in ["Release", "Resources"] {
                            let src = platform.path().join(sub);
                            let Ok(files) = std::fs::read_dir(&src) else { continue };
                            for file in files.flatten() {
                                if !file.path().is_file() {
                                    continue;
                                }
                                let _ = std::fs::copy(file.path(), target_dir.join(file.file_name()));
                            }
                            let locales = src.join("locales");
                            if locales.is_dir() {
                                let dest = target_dir.join("locales");
                                let _ = std::fs::create_dir_all(&dest);
                                if let Ok(paks) = std::fs::read_dir(&locales) {
                                    for pak in paks.flatten() {
                                        if pak.path().is_file() {
                                            let _ = std::fs::copy(pak.path(), dest.join(pak.file_name()));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    println!("cargo:rerun-if-changed={}", cef_root.display());
}
