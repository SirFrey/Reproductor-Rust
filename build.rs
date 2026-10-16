fn main() {
    slint_build::compile("ui/app.slint").expect("Slint build failed");

    // Embed the app icon (and version info) into the Windows exe, so Explorer,
    // shortcuts and the taskbar show the logo. Regenerate assets/icon.ico with
    // `cargo run --example make_icon` after editing assets/logo.svg.
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Reproductor");
        res.set("FileDescription", "Reproductor — YouTube Music player");
        res.compile().expect("embedding the Windows icon (needs the Windows SDK's rc.exe)");
    }
}
