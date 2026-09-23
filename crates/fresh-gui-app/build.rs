fn main() {
    #[cfg(target_os = "windows")]
    {
        // GPUI loads icon resource 1 (`LoadImageW` with id 1) for the taskbar.
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/fresh-gui.ico");
        res.compile()
            .unwrap_or_else(|err| panic!("embed fresh-gui.ico as icon resource 1: {err}"));
    }
}
