//! Builds the in-container agent for the Linux musl targets and embeds it.
//!
//! Set `DEVFWD_AGENT_DIR` to a directory holding prebuilt `devfwd-agent-x86_64`
//! and `devfwd-agent-aarch64` to skip the nested build (used in CI).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

const ARCHES: [(&str, &str); 2] =
    [("x86_64", "x86_64-unknown-linux-musl"), ("aarch64", "aarch64-unknown-linux-musl")];

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    println!("cargo:rerun-if-env-changed=DEVFWD_AGENT_DIR");
    for dir in ["crates/agent/src", "crates/agent/Cargo.toml", "crates/proto/src", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={}", workspace.join(dir).display());
    }

    let mut generated = String::new();
    for (arch, target) in ARCHES {
        let dest = out_dir.join(format!("devfwd-agent-{arch}"));
        let found = match env::var_os("DEVFWD_AGENT_DIR") {
            Some(dir) => fs::copy(Path::new(&dir).join(format!("devfwd-agent-{arch}")), &dest).is_ok(),
            None => build_agent(&workspace, &out_dir, target, &dest),
        };
        if !found {
            println!(
                "cargo:warning=no agent for {arch}: containers of that architecture will need \
                 DEVFWD_AGENT_PATH (hint: rustup target add {target})"
            );
            fs::write(&dest, b"").unwrap();
        }
        generated.push_str(&format!(
            "pub const {}: &[u8] = include_bytes!({:?});\n",
            arch.to_uppercase(),
            dest
        ));
    }
    fs::write(out_dir.join("agents.rs"), generated).unwrap();
}

fn build_agent(workspace: &Path, out_dir: &Path, target: &str, dest: &Path) -> bool {
    let target_dir = out_dir.join("agent-target");
    let linker_var = format!("CARGO_TARGET_{}_LINKER", target.to_uppercase().replace('-', "_"));
    let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(workspace)
        .args(["build", "--release", "-p", "devfwd-agent", "--target", target, "--target-dir"])
        .arg(&target_dir)
        // rust-lld ships with rustc and links the self-contained musl targets
        // from any host, so no cross toolchain is needed.
        .env(linker_var, "rust-lld")
        // Do not leak the outer build's flags into the nested one.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET")
        .status();
    match status {
        Ok(s) if s.success() => {
            fs::copy(target_dir.join(target).join("release/devfwd-agent"), dest).is_ok()
        }
        _ => false,
    }
}
