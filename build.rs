fn main() {
    slint_build::compile("ui/app.slint").expect("Failed to compile Slint UI");

    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("Failed to embed Windows application icon");
    }
}
