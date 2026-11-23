fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let root = std::path::Path::new(&manifest)
        .parent()
        .unwrap()
        .parent()
        .unwrap();

    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let revision = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=OMD_BUILD_REVISION={revision}");
    if revision != "unknown" {
        let branch = git(&["symbolic-ref", "-q", "HEAD"]);
        for name in ["HEAD", "packed-refs"].into_iter().chain(branch.as_deref()) {
            if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", name]) {
                if std::path::Path::new(&path).exists() {
                    println!("cargo:rerun-if-changed={path}");
                } else if name != "packed-refs" {
                    // A loose ref can be created on top of a packed ref.
                    if let Some(parent) = std::path::Path::new(&path).parent() {
                        if parent.exists() {
                            println!("cargo:rerun-if-changed={}", parent.display());
                        }
                    }
                }
            }
        }
    }

    // libghostty-vt：Linux 用 .so，Windows 用 ghostty-vt.dll + 导入库。
    let vt = root.join("vendor/libghostty-vt");
    if target_os == "windows" {
        let dir = vt.join("windows");
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=dylib=ghostty-vt");
        println!("cargo:rerun-if-changed={}", dir.display());
        // CEF wrapper 的 C++ 异常符号默认走 VCRUNTIME140.dll；换成静态的
        // libvcruntime（xwin CRT 里有），免得用户机器还得装 VC++ 运行库。
        println!("cargo:rustc-link-arg=/nodefaultlib:vcruntime.lib");
        println!("cargo:rustc-link-arg=libvcruntime.lib");
    } else {
        let so = vt.join("lib/libghostty-vt.so");
        // rust-lld 找不到 -l 名字时，直接把绝对路径的 .so 传给链接器
        println!("cargo:rustc-link-arg={}", so.display());
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../../vendor/libghostty-vt/lib");
        println!("cargo:rerun-if-changed={}", so.display());
    }

    // CEF：libcef.so / libcef.dll 在 vendor/cef/<version>/<platform>/Release 下，
    // 用 rpath 指过去（Windows 用 DLL 搜索路径，不需要 rpath）。
    // platform 目录名和 cef-dll-sys 的 OsAndArch 命名一致（cef_linux_x86_64 等），
    // 这里不写死；只挑当前目标的平台，避免把另一个 OS 的运行时拷进 target。
    let want_windows = target_os == "windows";
    let cef_root = root.join("vendor/cef");
    if let Ok(versions) = std::fs::read_dir(&cef_root) {
        for version in versions.flatten() {
            if let Ok(platforms) = std::fs::read_dir(version.path()) {
                for platform in platforms.flatten() {
                    let name = platform.file_name().to_string_lossy().into_owned();
                    if name.contains("windows") != want_windows {
                        continue;
                    }
                    let release = platform.path().join("Release");
                    if release.join("libcef.so").exists() || release.join("libcef.dll").exists() {
                        println!("cargo:rustc-link-search=native={}", release.display());
                        if target_os != "windows" {
                            println!(
                                "cargo:rustc-link-arg=-Wl,-rpath,{}/Release",
                                platform.path().display()
                            );
                        }
                    }
                }
            }
        }
    }
    // CEF 按「可执行文件旁边」找 icudtl.dat / *.pak / locales，
    // resources_dir_path 管不到 ICU 这一步。把运行时文件拷到 target 目录。
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    if let Some(target_dir) = std::path::Path::new(&out_dir).ancestors().nth(3) {
        if let Ok(versions) = std::fs::read_dir(&cef_root) {
            for version in versions.flatten() {
                if let Ok(platforms) = std::fs::read_dir(version.path()) {
                    for platform in platforms.flatten() {
                        for sub in ["Release", "Resources"] {
                            let src = platform.path().join(sub);
                            let Ok(files) = std::fs::read_dir(&src) else {
                                continue;
                            };
                            for file in files.flatten() {
                                if !file.path().is_file() {
                                    continue;
                                }
                                let _ =
                                    std::fs::copy(file.path(), target_dir.join(file.file_name()));
                            }
                            let locales = src.join("locales");
                            if locales.is_dir() {
                                let dest = target_dir.join("locales");
                                let _ = std::fs::create_dir_all(&dest);
                                if let Ok(paks) = std::fs::read_dir(&locales) {
                                    for pak in paks.flatten() {
                                        if pak.path().is_file() {
                                            let _ = std::fs::copy(
                                                pak.path(),
                                                dest.join(pak.file_name()),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Windows 的 libghostty-vt 是运行时 DLL，也要在 exe 旁边。
        if target_os == "windows" {
            let dll = vt.join("windows/ghostty-vt.dll");
            if dll.is_file() {
                let _ = std::fs::copy(&dll, target_dir.join("ghostty-vt.dll"));
            }
        }
    }
    println!("cargo:rerun-if-changed={}", cef_root.display());
}
