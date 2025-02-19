fn main() {
    #[cfg(target_os = "windows")]
    {
        let home_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-search={home_dir}\\Lib\\x64");
        println!("cargo:rustc-link-lib=wpcap");
        println!("cargo:rustc-link-lib=Packet");
    }
}
