use std::env;
use std::fs;
use std::path::Path;

fn file_defines_rustflags(path: &Path) -> bool {
    if let Ok(content) = fs::read_to_string(path) {
        content.contains("[target.thumbv6m-none-eabi]") && content.contains("rustflags")
    } else {
        false
    }
}

fn main() {
    println!("cargo:rerun-if-changed=memory.x");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let manifest_path = Path::new(&manifest_dir);

    // Search up parent directories for a .cargo/config.toml or .cargo/config that defines rustflags
    let mut has_parent_config = false;
    let mut current = manifest_path.parent();
    while let Some(path) = current {
        let config_toml = path.join(".cargo/config.toml");
        let config_no_ext = path.join(".cargo/config");
        if file_defines_rustflags(&config_toml) || file_defines_rustflags(&config_no_ext) {
            has_parent_config = true;
            break;
        }
        current = path.parent();
    }

    if !has_parent_config {
        println!("cargo:rustc-link-arg=--nmagic");
        println!("cargo:rustc-link-arg=-Tlink.x");
    }
}