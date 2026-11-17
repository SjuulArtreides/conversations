//! Embed the short git SHA (and a `-dirty` marker) into the binary so
//! `conversations --version` is self-identifying — you can tell at a
//! glance whether a reinstall actually picked up new code. Falls back to
//! `unknown` when git isn't available (e.g. a source tarball build).

use std::process::Command;

fn main() {
    embed_frontend();
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let suffix = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=GIT_SHA={sha}{suffix}");

    // Rebuild when the checkout moves so the embedded SHA stays accurate.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}

fn embed_frontend() {
    use std::{env, fs, path::Path};
    fn collect(dir: &Path, root: &Path, files: &mut Vec<(String, std::path::PathBuf)>) {
        for item in fs::read_dir(dir).expect("read frontend assets") {
            let path = item.expect("frontend asset").path();
            if path.is_dir() {
                collect(&path, root, files);
            } else {
                files.push((
                    format!(
                        "/{}",
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/")
                    ),
                    path.canonicalize().unwrap(),
                ));
            }
        }
    }
    println!("cargo:rerun-if-env-changed=CONVERSATIONS_REQUIRE_FRONTEND");
    let dir = Path::new("../../frontend/dist");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files = Vec::new();
    if dir.join("index.html").is_file() {
        collect(dir, dir, &mut files);
    } else if env::var_os("CONVERSATIONS_REQUIRE_FRONTEND").is_some() {
        panic!("Build the shared frontend first: cd frontend && npm ci && npm run build");
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut output = String::from("pub const SPA_ASSETS: &[(&str, &[u8])] = &[\n");
    for (url, path) in files {
        output.push_str(&format!(
            "({url:?}, include_bytes!({:?})),\n",
            path.to_string_lossy()
        ));
    }
    output.push_str("];\n");
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("frontend_assets.rs"),
        output,
    )
    .expect("write frontend asset table");
}
