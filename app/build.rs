use std::path::Path;
use std::process::Command;

fn main() {
    let ui_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui");
    let main = ui_dir.join("main.slint");
    println!("cargo:rerun-if-changed={}", ui_dir.display());
    println!("cargo:rerun-if-changed={}", main.display());
    let config = slint_build::CompilerConfiguration::new()
        .with_debug_info(std::env::var("PROFILE").as_deref() == Ok("debug"));
    slint_build::compile_with_config(&main, config).expect("failed to compile Slint UI");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_icon(&ui_dir.join("assets/gimage.ico"));
    }
}

fn embed_icon(icon: &Path) {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let rc = out.join("gimage.rc");
    let icon = icon.canonicalize().expect("missing application icon");
    std::fs::copy(icon, out.join("gimage.ico")).expect("failed to stage icon");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("gimage.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    std::fs::copy(manifest, out.join("gimage.manifest")).expect("failed to stage manifest");
    std::fs::write(&rc, "1 ICON \"gimage.ico\"\n1 24 \"gimage.manifest\"\n")
        .expect("failed to write Windows resources");

    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let (program, args, filename) = if msvc {
        (
            "rc.exe",
            &["/nologo", "/fogimage.res", "gimage.rc"][..],
            "gimage.res",
        )
    } else {
        (
            "windres",
            &["-O", "coff", "gimage.rc", "gimage-icon.o"][..],
            "gimage-icon.o",
        )
    };
    let status = Command::new(program)
        .current_dir(&out)
        .args(args)
        .status()
        .unwrap_or_else(|error| {
            panic!("cannot start {program} to embed the Windows icon: {error}")
        });
    assert!(
        status.success(),
        "{program} could not embed the Windows icon"
    );
    println!(
        "cargo:rustc-link-arg-bin=gimage={}",
        out.join(filename).display()
    );
}
