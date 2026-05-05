//! `kaishin` is a universal self-update library for Rust CLIs, extracted from `rvpm` and `renri`.
//!
//! It provides utilities to:
//! 1. Fetch the latest release information from GitHub.
//! 2. Detect how the current executable was installed (e.g., via `cargo install`).
//! 3. Perform a self-update by replacing the current binary.
//! 4. Manage background update check intervals to avoid frequent API calls.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Information about the latest release fetched from the GitHub API.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LatestRelease {
    /// The tag name of the release (e.g., `v3.31.4`).
    pub tag_name: String,
    /// The human-readable URL of the release page on GitHub.
    #[serde(default)]
    pub html_url: String,
}

/// The method by which the current executable was installed.
///
/// This is used to determine how to perform the update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    /// Installed via `cargo install`. Update is performed by running `cargo install` again.
    CargoInstall,
    /// A development build found under a `target/` directory. Updates are usually refused.
    DevBuild,
    /// A standalone binary. Update is performed by downloading and replacing the binary.
    DirectBinary,
}

/// Persistent state for background update checks, used for throttling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateCheckState {
    /// Unix timestamp of the last time a check was performed.
    pub last_checked_unix: u64,
    /// The tag name of the latest version found in the last check.
    pub last_known_latest: Option<String>,
}

/// Configuration options for `kaishin`.
#[derive(Debug, Clone)]
pub struct KaishinOptions {
    /// The GitHub owner (e.g., `yukimemi`).
    pub owner: String,
    /// The GitHub repository name (e.g., `rvpm`).
    pub repo: String,
    /// The binary name of the application (e.g., `rvpm`).
    pub bin_name: String,
    /// The current version of the application (usually `env!("CARGO_PKG_VERSION")`).
    pub current_version: String,
}

impl KaishinOptions {
    /// Creates a new instance of `KaishinOptions`.
    ///
    /// # Example
    /// ```
    /// use kaishin::KaishinOptions;
    /// let opts = KaishinOptions::new("yukimemi", "kaishin", "kaishin", "0.1.0");
    /// ```
    pub fn new(owner: &str, repo: &str, bin_name: &str, current_version: &str) -> Self {
        Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            bin_name: bin_name.to_string(),
            current_version: current_version.to_string(),
        }
    }
}

/// Returns the default interval between background update checks (24 hours).
pub fn default_interval() -> Duration {
    Duration::from_secs(86400)
}

/// Parses a duration string (e.g., "24h", "1d", "30m") into a `Duration`.
///
/// Uses the `humantime` crate for parsing.
pub fn parse_interval(s: &str) -> Result<Duration> {
    Ok(humantime::parse_duration(s)?)
}

/// Detects the installation method of the executable at the given path.
///
/// It checks if the path is under a `target/` directory or within a Cargo binary directory.
pub fn detect_install_method(exe: &Path) -> InstallMethod {
    let s = exe.to_string_lossy().replace('\\', "/").to_lowercase();
    if s.contains("/target/debug/") || s.contains("/target/release/") {
        return InstallMethod::DevBuild;
    }
    let cargo_bin = std::env::var("CARGO_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
        .map(|p| {
            p.join("bin")
                .to_string_lossy()
                .replace('\\', "/")
                .to_lowercase()
        });
    if let Some(bin) = cargo_bin {
        if s.starts_with(&format!("{}/", bin)) {
            return InstallMethod::CargoInstall;
        }
    }
    if s.contains("/.cargo/bin/") || s.contains("/cargo/bin/") {
        return InstallMethod::CargoInstall;
    }
    InstallMethod::DirectBinary
}

/// Compares the current version with a latest tag and returns `true` if an update is available.
///
/// # Errors
/// Returns an error if either version string cannot be parsed as a valid semver.
pub fn is_update_available(current: &str, latest_tag: &str) -> Result<bool> {
    let cur = semver::Version::parse(current)
        .map_err(|e| anyhow!("invalid current version `{}`: {}", current, e))?;
    let lat_str = latest_tag.trim_start_matches('v');
    let lat = semver::Version::parse(lat_str)
        .map_err(|e| anyhow!("invalid latest tag `{}`: {}", latest_tag, e))?;
    Ok(lat > cur)
}

/// Fetches the latest release information for the repository specified in `opts` from GitHub.
///
/// This is an asynchronous function that uses `reqwest`.
pub async fn check_latest_release(opts: &KaishinOptions) -> Result<LatestRelease> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        opts.owner, opts.repo
    );
    let client = reqwest::Client::builder()
        .user_agent(format!("{}/{}", opts.bin_name, opts.current_version))
        .timeout(Duration::from_secs(5))
        .build()?;
    let res = client.get(url).send().await?;
    if !res.status().is_success() {
        return Err(anyhow!("GitHub releases API returned {}", res.status()));
    }
    let release: LatestRelease = res.json().await?;
    Ok(release)
}

fn state_path(app_name: &str) -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(app_name).join("last_update_check.json"))
}

/// Loads the persistent update check state for the given application name.
pub fn load_check_state(app_name: &str) -> Option<UpdateCheckState> {
    let p = state_path(app_name)?;
    let content = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&content).ok()
}

/// Saves the persistent update check state for the given application name.
///
/// The save is performed atomically using a temporary file.
pub fn save_check_state(app_name: &str, state: &UpdateCheckState) -> Result<()> {
    if let Some(p) = state_path(app_name) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
            let json = serde_json::to_string(state)?;
            let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
            use std::io::Write;
            tmp.write_all(json.as_bytes())?;
            tmp.persist(&p)?;
        }
    }
    Ok(())
}

/// Determines whether an automatic update check should be performed based on the interval.
pub fn should_auto_check(
    state: Option<&UpdateCheckState>,
    interval: Duration,
    now: SystemTime,
) -> bool {
    let Some(state) = state else {
        return true;
    };
    let Ok(now_unix) = now.duration_since(SystemTime::UNIX_EPOCH) else {
        return true;
    };
    let elapsed = now_unix.as_secs().saturating_sub(state.last_checked_unix);
    elapsed >= interval.as_secs()
}

/// Formats a banner message intended for display when an update is available.
pub fn format_update_banner(opts: &KaishinOptions, latest: &LatestRelease) -> String {
    let tag = latest.tag_name.trim_start_matches('v');
    let mut s = format!(
        "\u{2699} {} {} available (current {}) — run `{} self-update` to upgrade",
        opts.bin_name, tag, opts.current_version, opts.bin_name
    );
    if !latest.html_url.is_empty() {
        s.push_str(&format!("\n  release notes: {}", latest.html_url));
    }
    s
}

/// Executes the self-update flow.
///
/// 1. Fetches the latest release from GitHub.
/// 2. Compares versions.
/// 3. If `check_only` is true, prints status and returns.
/// 4. Prompts the user (if `yes` is false and terminal is interactive).
/// 5. Detects the installation method and performs the update accordingly.
pub async fn run_self_update(opts: &KaishinOptions, yes: bool, check_only: bool) -> Result<()> {
    let latest = check_latest_release(opts)
        .await
        .context("failed to fetch latest release from GitHub")?;

    let available = is_update_available(&opts.current_version, &latest.tag_name)?;
    if !available {
        println!("\u{2713} {} {} is already up to date.", opts.bin_name, opts.current_version);
        return Ok(());
    }

    let latest_clean = latest.tag_name.trim_start_matches('v');
    if check_only {
        println!(
            "\u{2699} {} {} available (current {}). Run `{} self-update` to install.",
            opts.bin_name, latest_clean, opts.current_version, opts.bin_name
        );
        if !latest.html_url.is_empty() {
            println!("  release notes: {}", latest.html_url);
        }
        return Ok(());
    }

    if !yes {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "non-interactive mode: use `--yes` to proceed with update to v{}",
                latest_clean
            );
        }

        eprint!("Update to v{}? [y/N] ", latest_clean);
        use std::io::Write;
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            eprintln!("aborted.");
            return Ok(());
        }
    }

    let exe = std::env::current_exe().context("failed to resolve current_exe()")?;
    let method = detect_install_method(&exe);
    match method {
        InstallMethod::DevBuild => {
            return Err(anyhow!(
                "\u{26a0} `{}` looks like a development build. Refusing to self-update.",
                exe.display()
            ));
        }
        InstallMethod::CargoInstall => {
            let tmp = tempfile::Builder::new()
                .prefix(&format!("{}-self-update-", opts.bin_name))
                .tempdir()?;
            let tmp_root = tmp.path().to_path_buf();
            println!(
                "running: cargo install {} --version {} --locked --force --root {}",
                opts.bin_name,
                latest_clean,
                tmp_root.display()
            );
            let status = std::process::Command::new("cargo")
                .arg("install")
                .arg(&opts.bin_name)
                .arg("--version")
                .arg(latest_clean)
                .arg("--locked")
                .arg("--force")
                .arg("--root")
                .arg(&tmp_root)
                .status()?;
            if !status.success() {
                anyhow::bail!("cargo install failed");
            }
            let bin_exe_name = if cfg!(windows) {
                format!("{}.exe", opts.bin_name)
            } else {
                opts.bin_name.clone()
            };
            let new_exe = tmp_root.join("bin").join(bin_exe_name);
            self_update::self_replace::self_replace(&new_exe)?;
            println!("\u{2713} {} v{} installed.", opts.bin_name, latest_clean);
        }
        InstallMethod::DirectBinary => {
            let status = self_update::backends::github::Update::configure()
                .repo_owner(&opts.owner)
                .repo_name(&opts.repo)
                .bin_name(&opts.bin_name)
                .show_download_progress(true)
                .current_version(&opts.current_version)
                .target_version_tag(&latest.tag_name)
                .build()
                .map_err(|e| anyhow!("build: {}", e))?
                .update()
                .map_err(|e| anyhow!("update: {}", e))?;
            match status {
                self_update::Status::UpToDate(v) => {
                    println!("\u{2713} {} {} is already up to date.", opts.bin_name, v)
                }
                self_update::Status::Updated(v) => {
                    println!("\u{2713} {} v{} installed.", opts.bin_name, v)
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_install_method() {
        let p = PathBuf::from("/home/u/.cargo/bin/kaishin");
        assert_eq!(detect_install_method(&p), InstallMethod::CargoInstall);

        let p = PathBuf::from(
            r"C:\Users\yukimemi\src\github.com\yukimemi\kaishin\target\debug\kaishin.exe",
        );
        assert_eq!(detect_install_method(&p), InstallMethod::DevBuild);

        let p = PathBuf::from("/opt/kaishin-bin/kaishin");
        assert_eq!(detect_install_method(&p), InstallMethod::DirectBinary);
    }

    #[test]
    fn test_is_update_available() {
        assert!(is_update_available("0.1.0", "v0.1.1").unwrap());
        assert!(!is_update_available("0.1.1", "v0.1.1").unwrap());
        assert!(!is_update_available("0.1.2", "v0.1.1").unwrap());
    }

    #[test]
    fn test_should_auto_check() {
        let now = SystemTime::now();
        let now_unix = now.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();

        // No state
        assert!(should_auto_check(None, Duration::from_secs(86400), now));

        // Recent state
        let state = UpdateCheckState {
            last_checked_unix: now_unix - 3600,
            last_known_latest: None,
        };
        assert!(!should_auto_check(Some(&state), Duration::from_secs(86400), now));

        // Old state
        let state = UpdateCheckState {
            last_checked_unix: now_unix - 100000,
            last_known_latest: None,
        };
        assert!(should_auto_check(Some(&state), Duration::from_secs(86400), now));
    }
}
