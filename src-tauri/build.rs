fn main() {
    tauri_build::build();
    forward_dotenv_to_cargo();
}

/// Read the project-root `.env` and forward any `MARGIN_*` keys to
/// cargo via `cargo:rustc-env=...`, so `option_env!("MARGIN_*")` calls
/// in the source compile with the right values.
///
/// Why we need this: `bun run tauri dev` (and `tauri build`) loads
/// `.env` into Bun's JS runtime but does NOT propagate it to spawned
/// binaries — cargo never sees those vars. So OAuth client IDs
/// declared via `option_env!` would always be `None` unless the user
/// remembered to manually `export` them on every shell.
///
/// `cargo:rerun-if-changed` ensures this re-runs whenever `.env`
/// changes — values flow into the binary on the next build.
fn forward_dotenv_to_cargo() {
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    let manifest_dir = match env::var("CARGO_MANIFEST_DIR") {
        Ok(v) => v,
        Err(_) => return,
    };
    let env_path = PathBuf::from(&manifest_dir).join("..").join(".env");
    println!("cargo:rerun-if-changed={}", env_path.display());

    let content = match fs::read_to_string(&env_path) {
        Ok(s) => s,
        Err(_) => return, // no .env — nothing to forward
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else { continue };
        let key = key.trim();
        if !key.starts_with("MARGIN_") {
            continue;
        }
        // Strip surrounding quotes (single or double) — same as POSIX
        // shell's `set -a; . file; set +a` parsing in our cargo runner.
        let value = value.trim();
        let value = value
            .strip_prefix('"').and_then(|s| s.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(value);
        println!("cargo:rustc-env={key}={value}");
    }
}
