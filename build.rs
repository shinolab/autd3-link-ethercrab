fn main() {
    #[cfg(target_os = "windows")]
    let target = std::env::var("TARGET").unwrap();

    #[cfg(target_os = "windows")]
    {
        let home_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();

        if target.contains("arm") || target.contains("aarch64") {
            println!("cargo:rustc-link-search={home_dir}\\Lib\\ARM64");
        } else {
            println!("cargo:rustc-link-search={home_dir}\\Lib\\x64");
        }
        println!("cargo:rustc-link-lib=wpcap");
        println!("cargo:rustc-link-lib=Packet");
    }
}
