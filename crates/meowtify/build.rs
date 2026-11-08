fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../../assets/icon/meowtify-orange.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon/meowtify-orange.ico");
        res.compile().expect("embedding icon resource");
    }
}
