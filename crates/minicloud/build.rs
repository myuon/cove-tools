//! Records what `minicloud --version` reports: the Cove commit this build
//! links (from the workspace's `Cargo.lock`, the full hash the pin resolved
//! to), and the minicloud commit when the build is told it
//! (`MINICLOUD_COMMIT`, which the release workflow sets).

fn main() {
    let lock = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock");
    println!("cargo:rerun-if-changed={lock}");
    println!("cargo:rerun-if-env-changed=MINICLOUD_COMMIT");
    let cove = std::fs::read_to_string(lock)
        .ok()
        .and_then(|text| cove_commit(&text))
        .unwrap_or_else(|| "unknown".to_string());
    let mut version = format!("{} (cove {cove}", env!("CARGO_PKG_VERSION"));
    if let Ok(commit) = std::env::var("MINICLOUD_COMMIT") {
        let commit = commit.trim();
        if !commit.is_empty() {
            version.push_str(&format!(", minicloud {commit}"));
        }
    }
    version.push(')');
    println!("cargo:rustc-env=MINICLOUD_VERSION={version}");
    println!("cargo:rustc-env=MINICLOUD_COVE_COMMIT={cove}");
}

/// The commit after `#` in `cove-runtime`'s `source` line.
fn cove_commit(lock: &str) -> Option<String> {
    let mut packages = lock.split("[[package]]");
    packages
        .find(|package| package.contains("name = \"cove-runtime\""))?
        .lines()
        .find_map(|line| line.trim().strip_prefix("source = \""))?
        .split_once('#')
        .map(|(_, commit)| commit.trim_end_matches('"').to_string())
}
