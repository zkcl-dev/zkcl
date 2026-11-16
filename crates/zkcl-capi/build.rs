fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_VENDOR").as_deref() == Ok("apple") {
        // A linked host resolves its bundled copy through its own runtime path.
        println!("cargo::rustc-link-arg-cdylib=-Wl,-install_name,@rpath/libzkcl_capi.dylib");
    }
}
