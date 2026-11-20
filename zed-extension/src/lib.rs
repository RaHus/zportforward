//! Zed extension for devfwd.
//!
//! * Downloads the `devfwd` binary for this machine from GitHub releases and
//!   keeps it up to date; nothing has to be installed by hand.
//! * Registers it as a context server, so the agent gets `list_ports`,
//!   `forward_port`, `stop_forward` and `open_ports_page`. Zed runs extension
//!   context servers on the local machine even when the project is open in a
//!   dev container, which is exactly where devfwd must run.
//! * Adds the `/ports` slash command, which opens the ports page.

use std::fs;

use zed_extension_api::{
    self as zed, settings::ContextServerSettings, Architecture, Command, ContextServerConfiguration,
    ContextServerId, DownloadedFileType, GithubReleaseOptions, Os, Project, Result, SlashCommand,
    SlashCommandOutput, SlashCommandOutputSection, Worktree,
};

/// GitHub repository whose releases carry the `devfwd` binaries.
const REPO: &str = "RaHus/devfwd";
/// Where the daemon serves the ports page unless that port was taken.
const PORTS_PAGE: &str = "http://127.0.0.1:4480/";
/// Downloaded versions live in `<work dir>/devfwd-<version>/`.
const DIR_PREFIX: &str = "devfwd-";

struct DevfwdExtension {
    /// Path of the downloaded binary, relative to the extension's work
    /// directory. Set once per session, after checking for a newer release.
    downloaded: Option<String>,
}

/// Release asset and binary name for a platform.
struct Platform {
    target: &'static str,
    archive: &'static str,
    file_type: DownloadedFileType,
    binary: &'static str,
}

fn platform() -> Result<Platform> {
    let tar = |target| Platform { target, archive: "tar.gz", file_type: DownloadedFileType::GzipTar, binary: "devfwd" };
    Ok(match zed::current_platform() {
        (Os::Linux, Architecture::X8664) => tar("x86_64-unknown-linux-musl"),
        (Os::Linux, Architecture::Aarch64) => tar("aarch64-unknown-linux-musl"),
        (Os::Mac, Architecture::X8664) => tar("x86_64-apple-darwin"),
        (Os::Mac, Architecture::Aarch64) => tar("aarch64-apple-darwin"),
        (Os::Windows, Architecture::X8664) => Platform {
            target: "x86_64-pc-windows-msvc",
            archive: "zip",
            file_type: DownloadedFileType::Zip,
            binary: "devfwd.exe",
        },
        (os, arch) => return Err(format!("devfwd has no prebuilt binary for {os:?} {arch:?}; set devfwd_path")),
    })
}

fn is_file(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// Version directories already in the work directory.
fn version_dirs() -> Vec<String> {
    let Ok(entries) = fs::read_dir(".") else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(DIR_PREFIX))
        .collect()
}

/// A binary downloaded earlier, for when GitHub cannot be reached.
fn installed(binary: &str) -> Option<String> {
    let mut found: Vec<String> =
        version_dirs().into_iter().map(|dir| format!("{dir}/{binary}")).filter(|path| is_file(path)).collect();
    found.sort();
    found.pop()
}

/// Make sure the latest release is in the work directory; returns its path
/// relative to that directory.
fn download(platform: &Platform) -> Result<String> {
    let release =
        zed::latest_github_release(REPO, GithubReleaseOptions { require_assets: true, pre_release: false })?;
    let asset_name = format!("devfwd-{}.{}", platform.target, platform.archive);
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| format!("release {} has no asset {asset_name}", release.version))?;
    let dir = format!("{DIR_PREFIX}{}", release.version);
    let path = format!("{dir}/{}", platform.binary);
    if !is_file(&path) {
        zed::download_file(&asset.download_url, &dir, platform.file_type)?;
        zed::make_file_executable(&path)?;
        // Older versions are no longer needed. A binary that is still running
        // may refuse to go on Windows; it is tried again after the next update.
        for old in version_dirs().into_iter().filter(|name| *name != dir) {
            let _ = fs::remove_dir_all(old);
        }
    }
    Ok(path)
}

impl DevfwdExtension {
    /// The downloaded binary, relative to the work directory.
    fn binary(&mut self) -> Result<String> {
        if let Some(path) = self.downloaded.as_ref().filter(|path| is_file(path)) {
            return Ok(path.clone());
        }
        let platform = platform()?;
        let path = match download(&platform) {
            Ok(path) => path,
            Err(error) => installed(platform.binary).ok_or_else(|| {
                format!(
                    "could not download devfwd from github.com/{REPO}: {error}. \
                     Install devfwd yourself and set `devfwd_path` in this server's settings."
                )
            })?,
        };
        self.downloaded = Some(path.clone());
        Ok(path)
    }
}

/// Zed joins a relative command onto the extension's work directory instead
/// of searching `PATH`, so only an absolute path can be returned as is. A bare
/// name is handed to a launcher that lives at a fixed absolute path and does
/// the `PATH` lookup.
fn launch_command(path: &str, os: Os) -> (String, Vec<String>) {
    let is_absolute = path.starts_with('/')
        || path.starts_with("\\\\")
        || path.as_bytes().get(1..3).is_some_and(|b| b == b":\\" || b == b":/");
    if is_absolute {
        return (path.to_string(), vec!["mcp".to_string()]);
    }
    match os {
        Os::Windows => (
            "C:\\Windows\\System32\\cmd.exe".to_string(),
            vec!["/d".to_string(), "/c".to_string(), path.to_string(), "mcp".to_string()],
        ),
        _ => ("/usr/bin/env".to_string(), vec![path.to_string(), "mcp".to_string()]),
    }
}

impl zed::Extension for DevfwdExtension {
    fn new() -> Self {
        Self { downloaded: None }
    }

    fn context_server_command(
        &mut self,
        context_server_id: &ContextServerId,
        project: &Project,
    ) -> Result<Command> {
        let settings = ContextServerSettings::for_project(context_server_id.as_ref(), project)?;
        let custom = settings.settings.as_ref();
        let custom_path =
            custom.and_then(|s| s.get("devfwd_path")).and_then(|v| v.as_str()).filter(|p| !p.is_empty());
        let auto = custom.and_then(|s| s.get("auto")).and_then(|v| v.as_bool()).unwrap_or(false);

        let mut env = Vec::new();
        if auto {
            // Tells devfwd to turn auto-forwarding on in the daemon.
            env.push(("DEVFWD_AUTO".to_string(), "1".to_string()));
        }
        let (command, args) = match custom_path {
            Some(path) => launch_command(path, zed::current_platform().0),
            // A path relative to the work directory, which is where Zed looks.
            None => (self.binary()?, vec!["mcp".to_string()]),
        };
        Ok(Command { command, args, env })
    }

    fn context_server_configuration(
        &mut self,
        _context_server_id: &ContextServerId,
        _project: &Project,
    ) -> Result<Option<ContextServerConfiguration>> {
        Ok(Some(ContextServerConfiguration {
            installation_instructions: include_str!("../configuration/installation_instructions.md")
                .to_string(),
            default_settings: include_str!("../configuration/default_settings.jsonc").to_string(),
            settings_schema: include_str!("../configuration/settings_schema.json").to_string(),
        }))
    }

    /// `/ports`: open the ports page in the browser and leave its address in the thread.
    fn run_slash_command(
        &self,
        command: SlashCommand,
        _args: Vec<String>,
        _worktree: Option<&Worktree>,
    ) -> Result<SlashCommandOutput, String> {
        if command.name != "ports" {
            return Err(format!("unknown slash command: {}", command.name));
        }
        // `devfwd ui` starts the daemon if needed, opens the browser and
        // prints the page's real address. Processes need an absolute path.
        let opened = platform().ok().and_then(|p| installed(p.binary)).and_then(|relative| {
            let binary = std::env::current_dir().ok()?.join(relative);
            let output = zed::process::Command::new(binary.to_string_lossy()).arg("ui").output().ok()?;
            let address = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (output.status == Some(0) && address.starts_with("http")).then_some(address)
        });
        let text = match opened {
            Some(address) => format!("Opened the devfwd ports page: {address}\n"),
            None => format!("devfwd ports page: {PORTS_PAGE}\n"),
        };
        Ok(SlashCommandOutput {
            sections: vec![SlashCommandOutputSection {
                range: (0..text.len()).into(),
                label: "devfwd ports".to_string(),
            }],
            text,
        })
    }
}

zed::register_extension!(DevfwdExtension);
