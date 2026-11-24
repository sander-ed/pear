fn main() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_DIR");

    if std::env::var_os("CARGO_FEATURE_GHOSTTY").is_none() {
        return;
    }

    // Explicit override: GHOSTTY_VT_DIR=/path/to/prefix (contains lib/libghostty-vt.a)
    if let Some(dir) = std::env::var_os("GHOSTTY_VT_DIR") {
        let lib = std::path::PathBuf::from(dir).join("lib");
        link_static(&lib);
        return;
    }

    // Prefer the static library so the resulting binary is self-contained.
    if let Ok(lib) = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libghostty-vt-static")
    {
        let libdir = lib.link_paths.first().cloned().or_else(|| {
            pkg_config::get_variable("libghostty-vt-static", "libdir")
                .ok()
                .map(Into::into)
        });
        if let Some(libdir) = libdir.filter(|d| d.join("libghostty-vt.a").exists()) {
            link_static(&libdir);
            return;
        }
    }
    if pkg_config::Config::new().probe("libghostty-vt").is_ok() {
        return;
    }

    // Last resort: the default prefix suggested by the libghostty-vt docs.
    if let Some(home) = std::env::var_os("HOME") {
        let lib = std::path::PathBuf::from(home).join(".local/lib");
        if lib.join("libghostty-vt.a").exists() {
            link_static(&lib);
            return;
        }
    }

    panic!(
        "libghostty-vt not found.\n\
         Install it (needs zig 0.17):\n\
           curl -LO https://release.files.ghostty.org/libghostty-vt/1.0.1/libghostty-vt-1.0.1.tar.gz\n\
           tar xzf libghostty-vt-1.0.1.tar.gz && cd libghostty-vt-1.0.1\n\
           zig build -Doptimize=ReleaseFast --prefix \"$HOME/.local\"\n\
         then build with PKG_CONFIG_PATH=\"$HOME/.local/lib/pkgconfig\" cargo build --release\n\
         or set GHOSTTY_VT_DIR=\"$HOME/.local\".\n\
         Alternatively build without it: cargo build --release --no-default-features"
    );
}

fn link_static(libdir: &std::path::Path) {
    // Pass the archive by full path: on macOS `-lghostty-vt` would prefer the
    // .dylib sitting next to the .a, and we want a self-contained binary.
    println!(
        "cargo:rustc-link-arg={}",
        libdir.join("libghostty-vt.a").display()
    );
}
