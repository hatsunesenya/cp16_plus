//! 构建脚本：找到 libhamlib-4.dll，把它的目录加进链接搜索路径，并置 `has_hamlib` cfg。
//!
//! 为什么直接链 DLL 而不生成导入库：GNU ld / lld 都能直接对 PE DLL 建导入项，
//! 这样不用 dlltool，也不用把 11 MB 的库压进 git 之外的地方——DLL 就放在
//! `rust/third_party/hamlib/`（可用环境变量 CP16P_HAMLIB_DIR 指到别处）。
//! 找不到 DLL 时只告警不失败：电台功能编译成"未安装 Hamlib"的桩，其余功能照常。

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CP16P_HAMLIB_DIR");

    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("CP16P_HAMLIB_DIR") {
        cands.push(PathBuf::from(dir));
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    // rust/crates/rig -> rust/third_party/hamlib
    cands.push(manifest.join("../../third_party/hamlib"));

    for dir in cands {
        let dll = dir.join("libhamlib-4.dll");
        if dll.is_file() {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=dylib=hamlib-4");
            println!("cargo:rustc-cfg=has_hamlib");
            println!("cargo:rustc-env=CP16P_HAMLIB_AT={}", dll.display());
            return;
        }
    }
    println!(
        "cargo:warning=没找到 libhamlib-4.dll（放 rust/third_party/hamlib/ 或用 CP16P_HAMLIB_DIR 指定），电台功能将不可用"
    );
}
