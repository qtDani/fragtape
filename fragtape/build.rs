use std::path::PathBuf;
use std::process::Command;

fn main() {
    slint_build::compile_with_config("ui/app.slint", slint_build::CompilerConfiguration::new().with_style("fluent-dark".into()))
        .expect("Slint UI");
    embed_icon();
}

/// Put the bomb icon into the exe (Explorer, the taskbar and Alt+Tab use it).
fn embed_icon() {
    println!("cargo:rerun-if-changed=assets/fragtape.ico");
    println!("cargo:rerun-if-changed=assets/fragtape.rc");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("fragtape-res.o");
    let mut candidates = vec![std::env::var("WINDRES").unwrap_or_default(), "windres".into()];
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(format!("{local}/Microsoft/WinGet/Packages/BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe/mingw64/bin/windres.exe"));
    }
    for w in candidates.iter().filter(|w| !w.is_empty()) {
        let ok = Command::new(w).current_dir("assets").args(["fragtape.rc", "-O", "coff", "-o"]).arg(&out)
            .status().map(|s| s.success()).unwrap_or(false);
        if ok {
            println!("cargo:rustc-link-arg-bins={}", out.display());
            return;
        }
    }
    println!("cargo:warning=windres not found: the exe gets no icon");
}
