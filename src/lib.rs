//! `kaishin` is a universal self-update library for Rust CLIs, extracted from `rvpm` and `renri`.
//!
//! It provides utilities to:
//! 1. Fetch the latest release information from GitHub.
//! 2. Detect how the current executable was installed (e.g., via `cargo install`).
//! 3. Perform a self-update by replacing the current binary.
//! 4. Manage background update check intervals to avoid frequent API calls via [`Checker`].

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    /// Installed via `cargo install`. By default, updated by downloading the
    /// GitHub release binary; falls back to re-running `cargo install` if the
    /// download fails. See [`UpdateOptions::prefer_github_release`].
    CargoInstall,
    /// A development build found under a `target/` directory.
    DevBuild,
    /// A standalone binary.
    DirectBinary,
}

/// Persistent state for background update checks, used for throttling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateCheckState {
    /// Unix timestamp of the last time a check was performed.
    pub last_checked_unix: u64,
    /// The tag name of the latest version found in the last check.
    pub last_known_latest: Option<String>,
    /// The URL of the latest version found in the last check.
    pub last_known_url: Option<String>,
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
    pub fn new(owner: &str, repo: &str, bin_name: &str, current_version: &str) -> Self {
        Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            bin_name: bin_name.to_string(),
            current_version: current_version.to_string(),
        }
    }
}

/// Options for the self-update process.
#[derive(Debug, Clone)]
pub struct UpdateOptions {
    /// Automatically answer "yes" to all prompts.
    pub yes: bool,
    /// Only check for updates and print status, don't perform the update.
    pub check_only: bool,
    /// Run in non-interactive mode. Bail if a prompt would be required and `yes` is false.
    pub non_interactive: bool,
    /// When the binary was installed via `cargo install`, prefer downloading the
    /// release binary from GitHub instead of running `cargo install` (which
    /// rebuilds from source and is slow). Defaults to `true`.
    ///
    /// If the GitHub release download fails (e.g., no matching asset for the
    /// current platform), the update falls back to `cargo install`. Set this
    /// to `false` to skip the GitHub release attempt entirely.
    pub prefer_github_release: bool,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            yes: false,
            check_only: false,
            non_interactive: false,
            prefer_github_release: true,
        }
    }
}

impl UpdateOptions {
    /// Creates a new instance of `UpdateOptions` with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the `yes` flag.
    pub fn yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }

    /// Sets the `check_only` flag.
    pub fn check_only(mut self, check_only: bool) -> Self {
        self.check_only = check_only;
        self
    }

    /// Sets the `non_interactive` flag.
    pub fn non_interactive(mut self, non_interactive: bool) -> Self {
        self.non_interactive = non_interactive;
        self
    }

    /// Sets the `prefer_github_release` flag.
    pub fn prefer_github_release(mut self, prefer_github_release: bool) -> Self {
        self.prefer_github_release = prefer_github_release;
        self
    }
}

/// A high-level handler for managing background update checks.
///
/// It encapsulates JSON state persistence and update logic.
#[derive(Debug, Clone)]
pub struct Checker {
    opts: KaishinOptions,
    interval: Duration,
    state_path: PathBuf,
}

impl Checker {
    /// Creates a new `Checker` for the given application.
    ///
    /// By default, the state is stored in the system's data directory under `app_name`.
    pub fn new(app_name: &str, opts: KaishinOptions) -> Self {
        let state_path = default_state_path(app_name)
            .expect("failed to resolve default state path (dirs::data_dir() failed)");
        Self {
            opts,
            interval: Duration::from_secs(86400),
            state_path,
        }
    }

    /// Sets the interval between background update checks.
    pub fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Sets a custom path for the persistent state file.
    pub fn state_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.state_path = path.into();
        self
    }

    /// Determines if a background check should be performed now.
    pub fn should_check(&self) -> bool {
        let state = self.load_state();
        should_auto_check(state.as_ref(), self.interval, SystemTime::now())
    }

    /// Fetches the latest release, saves it to the state file, and returns the
    /// result *only when it actually outranks the running version*.
    ///
    /// The state file is updated regardless of the comparison result so the
    /// next [`Checker::should_check`] call throttles correctly. The return
    /// value mirrors [`Checker::cached_update`]: `Ok(Some(_))` if a newer
    /// release exists, `Ok(None)` if the running version is already up to date
    /// (or ahead), and `Err(_)` only if the GitHub request itself failed.
    ///
    /// Callers can therefore feed the return value straight into
    /// [`Checker::format_banner`] without a separate
    /// [`is_update_available`] check — the asymmetry that bit
    /// `renri`/`yui` in kaishin 0.3.x is gone.
    pub async fn check_and_save(&self) -> Result<Option<LatestRelease>> {
        let latest = check_latest_release(&self.opts).await?;
        let now_unix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let state = UpdateCheckState {
            last_checked_unix: now_unix,
            last_known_latest: Some(latest.tag_name.clone()),
            last_known_url: Some(latest.html_url.clone()),
        };
        let _ = self.save_state(&state);
        if is_update_available(&self.opts.current_version, &latest.tag_name).unwrap_or(false) {
            Ok(Some(latest))
        } else {
            Ok(None)
        }
    }

    /// Returns the cached latest release from the state file, if available and newer.
    pub fn cached_update(&self) -> Option<LatestRelease> {
        let state = self.load_state()?;
        let latest_tag = state.last_known_latest?;
        if is_update_available(&self.opts.current_version, &latest_tag).unwrap_or(false) {
            Some(LatestRelease {
                tag_name: latest_tag,
                html_url: state.last_known_url.unwrap_or_default(),
            })
        } else {
            None
        }
    }

    /// Formats an update banner for the given release.
    pub fn format_banner(&self, latest: &LatestRelease) -> String {
        format_update_banner(&self.opts, latest)
    }

    fn load_state(&self) -> Option<UpdateCheckState> {
        load_check_state(&self.state_path)
    }

    fn save_state(&self, state: &UpdateCheckState) -> Result<()> {
        save_check_state(&self.state_path, state)
    }
}

/// Returns the default interval between background update checks (24 hours).
pub fn default_interval() -> Duration {
    Duration::from_secs(86400)
}

/// Parses a duration string (e.g., "24h", "1d", "30m") into a `Duration`.
pub fn parse_interval(s: &str) -> Result<Duration> {
    Ok(humantime::parse_duration(s)?)
}

/// Detects the installation method of the executable at the given path.
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
pub fn is_update_available(current: &str, latest_tag: &str) -> Result<bool> {
    let cur = semver::Version::parse(current)
        .map_err(|e| anyhow!("invalid current version `{}`: {}", current, e))?;
    let lat_str = latest_tag.trim_start_matches('v');
    let lat = semver::Version::parse(lat_str)
        .map_err(|e| anyhow!("invalid latest tag `{}`: {}", latest_tag, e))?;
    Ok(lat > cur)
}

/// Fetches the latest release information for the repository specified in `opts` from GitHub.
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

/// Returns the default path for the state file under the system's data directory.
pub fn default_state_path(app_name: &str) -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(app_name).join("last_update_check.json"))
}

/// Loads the persistent update check state from the given path.
pub fn load_check_state(path: &Path) -> Option<UpdateCheckState> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Saves the persistent update check state to the given path.
pub fn save_check_state(path: &Path, state: &UpdateCheckState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let json = serde_json::to_string(state)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        use std::io::Write;
        tmp.write_all(json.as_bytes())?;
        tmp.persist(path)?;
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
/// 3. If [`UpdateOptions::check_only`] is true, prints status and returns.
/// 4. If [`UpdateOptions::non_interactive`] is true and [`UpdateOptions::yes`] is false, bails if an update is available.
/// 5. Prompts the user (if [`UpdateOptions::yes`] is false and terminal is interactive).
/// 6. Detects the installation method and performs the update accordingly:
///    - `DevBuild`: errors out (refuses to overwrite a development binary).
///    - `CargoInstall`: if [`UpdateOptions::prefer_github_release`] is `true`
///      (the default), downloads the release binary from GitHub and only falls
///      back to `cargo install` when the GitHub release path fails. If `false`,
///      runs `cargo install` directly.
///    - `DirectBinary`: downloads the matching binary from the GitHub release.
pub async fn run_self_update(opts: &KaishinOptions, upd_opts: UpdateOptions) -> Result<()> {
    let latest = check_latest_release(opts)
        .await
        .context("failed to fetch latest release from GitHub")?;

    let available = is_update_available(&opts.current_version, &latest.tag_name)?;
    if !available {
        println!(
            "\u{2713} {} {} is already up to date.",
            opts.bin_name, opts.current_version
        );
        return Ok(());
    }

    let latest_clean = latest.tag_name.trim_start_matches('v');
    if upd_opts.check_only {
        println!(
            "\u{2699} {} {} available (current {}). Run `{} self-update` to install.",
            opts.bin_name, latest_clean, opts.current_version, opts.bin_name
        );
        if !latest.html_url.is_empty() {
            println!("  release notes: {}", latest.html_url);
        }
        return Ok(());
    }

    if !upd_opts.yes {
        use std::io::IsTerminal;
        if upd_opts.non_interactive || !std::io::stdin().is_terminal() {
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
            if upd_opts.prefer_github_release {
                match update_via_github_release(opts, &latest) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        eprintln!(
                            "GitHub release download failed: {e:#}. Falling back to `cargo install`."
                        );
                    }
                }
            }
            update_via_cargo_install(opts, latest_clean)?;
        }
        InstallMethod::DirectBinary => {
            update_via_github_release(opts, &latest)?;
        }
    }
    Ok(())
}

fn update_via_cargo_install(opts: &KaishinOptions, latest_clean: &str) -> Result<()> {
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
    Ok(())
}

fn update_via_github_release(opts: &KaishinOptions, latest: &LatestRelease) -> Result<()> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner(&opts.owner)
        .repo_name(&opts.repo)
        .bin_name(&opts.bin_name)
        .show_download_progress(true)
        .current_version(&opts.current_version)
        .target_version_tag(&latest.tag_name)
        .build()
        .context("build")?
        .update()
        .context("update")?;
    match status {
        self_update::Status::UpToDate(v) => {
            println!("\u{2713} {} {} is already up to date.", opts.bin_name, v)
        }
        self_update::Status::Updated(v) => {
            println!("\u{2713} {} v{} installed.", opts.bin_name, v)
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        // No 'v' prefix
        assert!(is_update_available("0.1.0", "0.1.1").unwrap());
    }

    #[test]
    fn test_should_auto_check() {
        let now = SystemTime::now();
        let now_unix = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // No state
        assert!(should_auto_check(None, Duration::from_secs(86400), now));

        // Recent state
        let state = UpdateCheckState {
            last_checked_unix: now_unix - 3600,
            last_known_latest: None,
            last_known_url: None,
        };
        assert!(!should_auto_check(
            Some(&state),
            Duration::from_secs(86400),
            now
        ));

        // Old state
        let state = UpdateCheckState {
            last_checked_unix: now_unix - 100000,
            last_known_latest: None,
            last_known_url: None,
        };
        assert!(should_auto_check(
            Some(&state),
            Duration::from_secs(86400),
            now
        ));
    }

    #[test]
    fn test_update_options_defaults() {
        let opts = UpdateOptions::new();
        assert!(!opts.yes);
        assert!(!opts.check_only);
        assert!(!opts.non_interactive);
        assert!(opts.prefer_github_release);
    }

    #[test]
    fn test_update_options_prefer_github_release_builder() {
        let opts = UpdateOptions::new().prefer_github_release(false);
        assert!(!opts.prefer_github_release);
        let opts = opts.prefer_github_release(true);
        assert!(opts.prefer_github_release);
    }

    #[test]
    fn test_format_update_banner() {
        let opts = KaishinOptions::new("u", "r", "app", "1.0.0");
        let release = LatestRelease {
            tag_name: "v1.1.0".to_string(),
            html_url: "https://example.com".to_string(),
        };
        let banner = format_update_banner(&opts, &release);
        assert!(banner.contains("app 1.1.0 available"));
        assert!(banner.contains("(current 1.0.0)"));
        assert!(banner.contains("run `app self-update`"));
        assert!(banner.contains("https://example.com"));
    }

    #[test]
    fn test_checker_state_management() {
        let tmp = tempdir().unwrap();
        let state_path = tmp.path().join("state.json");
        let opts = KaishinOptions::new("u", "r", "app", "1.0.0");
        let checker = Checker::new("app", opts).state_path(&state_path);

        // Initial state
        assert!(checker.should_check());
        assert!(checker.cached_update().is_none());

        // Save state manually for testing
        let now_unix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let state = UpdateCheckState {
            last_checked_unix: now_unix,
            last_known_latest: Some("v1.2.0".to_string()),
            last_known_url: Some("https://rel".to_string()),
        };
        checker.save_state(&state).unwrap();

        // Check again
        assert!(!checker.should_check()); // within 24h
        let cached = checker.cached_update().unwrap();
        assert_eq!(cached.tag_name, "v1.2.0");
        assert_eq!(cached.html_url, "https://rel");

        // Test banner from checker
        let banner = checker.format_banner(&cached);
        assert!(banner.contains("app 1.2.0 available"));
    }
}
