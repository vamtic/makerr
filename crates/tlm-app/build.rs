fn main() {
    slint_build::compile("ui/appwindow.slint").unwrap();

    // Embed the icon (and product strings) into the Windows executable. The
    // installer only assigns the .ico to shortcuts, so without this Explorer,
    // Alt-Tab and the taskbar fell back to the generic Windows icon for the
    // program itself.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../../assets/timelapsemaker.ico");
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/timelapsemaker.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon.to_str().expect("icon path is valid UTF-8"));
        res.set("FileDescription", "TimelapseMaker");
        res.set("ProductName", "TimelapseMaker");
        res.compile().expect("embed the Windows application icon");
    }
}
