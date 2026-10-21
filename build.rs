fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/nietono.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico")
        .set_manifest_file("assets/nietono.manifest")
        .set("FileDescription", "Nietono")
        .set("ProductName", "Nietono")
        .set("LegalCopyright", "GPL-3.0-or-later");
    res.compile().expect("compile Windows resources");
}
