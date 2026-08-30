use std::env;
use std::fs;
use std::path::Path;

fn main() {
    // 图标变更时重新编译资源
    println!("cargo:rerun-if-changed=winsnap.ico");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();

    let src = Path::new(&manifest_dir).join("winsnap.ico");
    let dst = Path::new(&out_dir).join("winsnap.ico");
    let _ = fs::copy(&src, &dst);

    let mut res = winres::WindowsResource::new();
    res.set_icon("winsnap.ico");
    res.compile().unwrap();
}
