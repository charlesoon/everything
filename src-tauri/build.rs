fn main() {
    // GPUI supplies the native window/runtime; no WebView assets or ACL build step.
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=icons/icon.ico");
        println!("cargo:rerun-if-changed=windows/resources.rc");
        embed_resource::compile("windows/resources.rc", embed_resource::NONE)
            .manifest_required()
            .expect("compile the native executable icon");
    }
}
