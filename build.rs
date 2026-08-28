use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    // Re-sync git hooks whenever the lefthook config changes.
    println!("cargo:rerun-if-changed=lefthook.yml");

    // Hooks are a local-dev convenience: skip in CI and non-git checkouts.
    if env::var_os("CI").is_some() {
        return;
    }
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    if !Path::new(&manifest_dir).join(".git").exists() {
        return;
    }

    match Command::new("lefthook").arg("install").status() {
        Ok(status) if status.success() => {}
        Ok(_) => {
            println!(
                "cargo:warning=`lefthook install` failed; git hooks are stale. Run `lefthook install` manually."
            );
        }
        Err(_) => {
            println!(
                "cargo:warning=lefthook not found; git hooks not installed. Install it (`npm install -g lefthook` or `go install github.com/evilmartians/lefthook@latest`), then run `lefthook install`."
            );
        }
    }
}
