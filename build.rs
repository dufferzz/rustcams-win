fn main() {
    // Rebuild when the Windows icon changes.
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=assets/alert.mp3");
    println!("cargo:rerun-if-changed=assets/kim.mp3");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Citadel CCTV");
        res.set("FileDescription", "Citadel CCTV — multi-camera RTSP viewer");
        if let Err(err) = res.compile() {
            // Don't fail Linux-hosted checks; warn so local Windows builds still surface issues.
            println!("cargo:warning=winresource failed to embed icon: {err}");
        }
    }
}
