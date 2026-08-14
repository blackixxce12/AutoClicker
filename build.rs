//! Embeds the application icon and version metadata into the Windows executable.
//!
//! Note: `#[cfg(target_os = "windows")]` is wrong here - a build script runs on the
//! *host*, so the target OS has to come from Cargo's environment instead.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "Auto Clicker");
        res.set("ProductName", "Auto Clicker");
        res.set("OriginalFilename", "AutoClicker.exe");
        res.set("LegalCopyright", "MIT License");

        // A missing resource compiler should cost you the Explorer icon, not the
        // whole build.
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the executable icon: {e}");
        }
    }
}
