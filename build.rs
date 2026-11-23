use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_layer_shell)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");

    let has_layer_shell = Command::new("pkg-config")
        .args(["--exists", "gtk4-layer-shell-0"])
        .status()
        .is_ok_and(|s| s.success());

    if has_layer_shell {
        println!("cargo::rustc-link-lib=gtk4-layer-shell");
        println!("cargo::rustc-cfg=has_layer_shell");
    }
}
