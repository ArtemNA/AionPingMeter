use std::{env, fs, path::PathBuf};

fn main() {
    // Icon, version info and manifest (per-monitor DPI awareness, common controls v6).
    // app.rc is a template: the version and absolute paths are filled in here.
    let v = |k: &str| env::var(k).unwrap();
    let root = PathBuf::from(v("CARGO_MANIFEST_DIR"));
    let version = v("CARGO_PKG_VERSION");
    let num = format!("{},{},{},0", v("CARGO_PKG_VERSION_MAJOR"), v("CARGO_PKG_VERSION_MINOR"), v("CARGO_PKG_VERSION_PATCH"));
    let path = |f: &str| root.join(f).display().to_string().replace('\\', "\\\\");

    let rc = fs::read_to_string(root.join("app.rc"))
        .unwrap()
        .replace("@ICON@", &path("app.ico"))
        .replace("@MANIFEST@", &path("app.manifest"))
        .replace("@VER_NUM@", &num)
        .replace("@VER_STR@", &version);
    let out = PathBuf::from(v("OUT_DIR")).join("app.rc");
    fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_required().unwrap();

    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.ico");
}
