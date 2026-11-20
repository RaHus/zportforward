//! Thin wrapper around the `docker` / `podman` CLI.
//!
//! The CLI (not the Engine socket) is used on purpose: it follows the user's
//! contexts, rootless setups and remote hosts without any extra configuration.

use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Runtime {
    bin: String,
}

impl Runtime {
    /// `ZPORTFORWARD_RUNTIME` if set, otherwise the first of `docker`, `podman` that runs.
    pub async fn detect() -> Result<Self> {
        if let Ok(bin) = std::env::var("ZPORTFORWARD_RUNTIME") {
            return Ok(Self { bin });
        }
        for bin in ["docker", "podman"] {
            let ok = Command::new(bin)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                return Ok(Self { bin: bin.into() });
            }
        }
        bail!("neither docker nor podman found on PATH (set ZPORTFORWARD_RUNTIME to override)")
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.kill_on_drop(true);
        cmd
    }

    /// Run to completion and return trimmed stdout; stderr becomes the error.
    pub async fn output(&self, args: &[&str]) -> Result<String> {
        self.output_with_stdin(args, None).await
    }

    pub async fn output_with_stdin(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
        let mut child = self
            .command()
            .args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run {}", self.bin))?;
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // The command may exit without reading everything; that is its call.
            let _ = pipe.write_all(bytes).await;
            let _ = pipe.shutdown().await;
        }
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("{} {}: {}", self.bin, args.first().copied().unwrap_or(""), stderr.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}
