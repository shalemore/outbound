//! Gives the Windows executable its icon and its version information, so that Explorer, the task
//! manager and the notification settings show "Outbound" with its icon.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // The resource compiler only exists when the build itself runs on Windows.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/icon.ico")
            .set("ProductName", "Outbound")
            .set("FileDescription", "Outbound")
            .set("LegalCopyright", "Copyright (c) 2026 shalemore");
        if let Err(e) = resource.compile() {
            println!("cargo:warning=cannot embed the icon: {e}");
        }
    }
}
