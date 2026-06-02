//! `kaishin` is a universal self-update library for Rust CLIs, extracted from `rvpm` and `renri`.
//!
//! It provides utilities to:
//! 1. Fetch the latest release information from GitHub.
//! 2. Detect how the current executable was installed (e.g., via `cargo install`).
//! 3. Perform a self-update by replacing the current binary.
//! 4. Manage background update check intervals to avoid frequent API calls via [`Checker`].
//! 5. Silently auto-update in the background — check, download, and replace the
//!    binary without prompting via [`Checker::auto_update`] /
//!    [`Checker::spawn_auto_update`]. The running process keeps the old binary;
//!    the new version takes effect on the next launch.

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
        if is_update_available(&self.opts.current_version, &latest.tag_name)? {
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

    /// Silently checks for and installs a newer release in the background.
    ///
    /// This is the Claude-Code-style "auto-update" path: no prompts, no
    /// download progress, no stdout chatter. It is throttled by the same
    /// [`Checker::interval`] as [`Checker::should_check`], so it is safe to
    /// call unconditionally on every startup — it hits GitHub at most once per
    /// interval.
    ///
    /// Behaviour:
    /// - Returns `Ok(None)` immediately if the check interval hasn't elapsed,
    ///   or if another process already holds the update lock (see below).
    /// - Fetches the latest release and persists the check timestamp (so the
    ///   throttle advances even when nothing is installed).
    /// - If a newer release exists, replaces the binary **silently**:
    ///   - [`InstallMethod::DevBuild`] → skipped (returns `Ok(None)`); a dev
    ///     build is never clobbered.
    ///   - [`InstallMethod::CargoInstall`] and [`InstallMethod::DirectBinary`]
    ///     → download the matching GitHub release asset and self-replace. A
    ///     `cargo install` rebuild is **never** triggered on this path (it's
    ///     slow and noisy); if no release asset matches, the update simply
    ///     fails and is reported as `Err`.
    /// - On success returns `Ok(Some(release))` with the installed release.
    ///   The **running** process keeps the old binary in memory; the new
    ///   version takes effect on the next launch.
    ///
    /// Opt-out is intentionally left to the caller — decide whether to call
    /// this at all (e.g. gated behind your own env var or config) rather than
    /// relying on the library to read the environment.
    ///
    /// ## Concurrency
    ///
    /// An OS advisory lock on a lock file next to the state file serialises
    /// updates across processes so two concurrently-starting instances don't
    /// both self-replace the same binary (which races on Windows in
    /// particular). The loser returns `Ok(None)`. The kernel releases the lock
    /// when the holder exits — including on a crash — so a failed update can't
    /// disable auto-update permanently.
    ///
    /// ## Runtime
    ///
    /// The actual install runs on a detached OS thread with no Tokio context,
    /// for the same reason as [`run_self_update`] — `self_update`'s backend
    /// spins up (and drops) its own blocking runtime, which deadlocks inside an
    /// async executor.
    pub async fn auto_update(&self) -> Result<Option<LatestRelease>> {
        if !self.should_check() {
            return Ok(None);
        }

        // Serialise across processes; bail (not error) if someone else holds it.
        let lock_path = self.state_path.with_extension("lock");
        let Some(_lock) = UpdateLock::acquire(&lock_path) else {
            return Ok(None);
        };

        // Advances the throttle even when there's no newer release.
        let Some(latest) = self.check_and_save().await? else {
            return Ok(None);
        };

        let opts = self.opts.clone();
        let latest_for_thread = latest.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name(format!("{}-auto-update", opts.bin_name))
            .spawn(move || {
                let _ = tx.send(run_silent_update_blocking(&opts, &latest_for_thread));
            })
            .context("failed to spawn auto-update worker thread")?;
        let installed = rx
            .await
            .context("auto-update worker thread exited without reporting a result")??;

        Ok(installed.then_some(latest))
    }

    /// Fire-and-forget wrapper around [`Checker::auto_update`].
    ///
    /// Spawns a detached Tokio task and returns immediately so application
    /// startup isn't blocked on a network round-trip or a download. Any error
    /// (and the installed-version result) is discarded — use
    /// [`Checker::auto_update`] directly if you need to react to either.
    ///
    /// Must be called from within a Tokio runtime.
    pub fn spawn_auto_update(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let _ = this.auto_update().await;
        });
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

    // The confirmation prompt and the install step both perform synchronous,
    // blocking I/O. In particular the `self_update` backend creates — and then
    // drops — its own blocking Tokio runtime. Doing that anywhere inside the
    // caller's async runtime deadlocks ("Cannot start a runtime from within a
    // runtime"); even a `spawn_blocking` pool thread is still runtime-attached
    // and instead trips "Cannot drop a runtime in a context where blocking is
    // not allowed". This is exactly what froze `shoka`/`renri` mid-update.
    //
    // Run the whole blocking tail on a detached OS thread that has *no* Tokio
    // context at all, and await its result over a oneshot channel. Works on
    // both multi-thread and current-thread runtimes.
    let opts = opts.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(format!("{}-self-update", opts.bin_name))
        .spawn(move || {
            let _ = tx.send(run_self_update_blocking(&opts, &latest, upd_opts));
        })
        .context("failed to spawn self-update worker thread")?;
    rx.await
        .context("self-update worker thread exited without reporting a result")?
}

/// Windows console-mode guard for the confirmation prompt.
///
/// `read_line` on Windows reads the console via `ReadConsoleW`, which only
/// returns on Enter (and only echoes / honours Ctrl-C as a signal) when the
/// console input is in cooked mode — `ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT |
/// ENABLE_PROCESSED_INPUT`. A program that put the console into raw mode
/// (e.g. a crossterm/ratatui TUI) and exited without restoring it leaves
/// those flags cleared *for the whole console*, so a later `self-update` in
/// the same terminal hangs at `[y/N]` with no echo and an inert Ctrl-C. We
/// can't trust the inherited mode, so set cooked input for the read and
/// restore the previous mode on drop.
#[cfg(windows)]
mod cooked_input {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, GetConsoleMode,
        GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };

    const COOKED: CONSOLE_MODE = ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT;

    /// Restores the previous console input mode on drop, but only if `guard`
    /// actually changed it.
    pub(crate) struct CookedInput {
        handle: HANDLE,
        prev: CONSOLE_MODE,
        restore: bool,
    }

    impl CookedInput {
        pub(crate) fn guard() -> Self {
            // SAFETY: plain FFI calls into the Win32 console API with a
            // borrowed std handle; no memory is aliased or freed.
            unsafe {
                let handle = GetStdHandle(STD_INPUT_HANDLE);
                let mut prev: CONSOLE_MODE = 0;
                // `GetConsoleMode` fails when stdin is redirected (a pipe or
                // file) — there's no console mode to repair, and the
                // non-interactive guard upstream already covers that path.
                if handle.is_null() || GetConsoleMode(handle, &mut prev) == 0 {
                    return Self {
                        handle,
                        prev: 0,
                        restore: false,
                    };
                }
                let cooked = prev | COOKED;
                // Only touch (and later restore) the mode when something was
                // actually cleared, so the common healthy case is a no-op.
                if cooked != prev && SetConsoleMode(handle, cooked) != 0 {
                    Self {
                        handle,
                        prev,
                        restore: true,
                    }
                } else {
                    Self {
                        handle,
                        prev,
                        restore: false,
                    }
                }
            }
        }
    }

    impl Drop for CookedInput {
        fn drop(&mut self) {
            if self.restore {
                // Best-effort: nothing actionable if the restore fails.
                // SAFETY: same borrowed console handle as in `guard`.
                unsafe {
                    SetConsoleMode(self.handle, self.prev);
                }
            }
        }
    }
}

/// The synchronous, blocking tail of [`run_self_update`]: prompt the user and
/// perform the actual install. Must run off the async executor (see the call
/// site) because `self_update`'s backend blocks on its own runtime.
fn run_self_update_blocking(
    opts: &KaishinOptions,
    latest: &LatestRelease,
    upd_opts: UpdateOptions,
) -> Result<()> {
    let latest_clean = latest.tag_name.trim_start_matches('v');

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
        // A full-screen TUI (ratatui/crossterm, …) run earlier in the same
        // console can exit without restoring cooked mode, leaving stdin's
        // console with ENABLE_LINE_INPUT / ENABLE_ECHO_INPUT /
        // ENABLE_PROCESSED_INPUT cleared. `read_line` then blocks forever on
        // an Enter that never terminates the line, and Ctrl-C arrives as a
        // raw 0x03 byte instead of a signal — the "frozen at [y/N]" hang.
        // Force a sane mode for the read and restore it after; no-op off
        // Windows and when stdin isn't a console.
        #[cfg(windows)]
        let _cooked_input = cooked_input::CookedInput::guard();
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
                match update_via_github_release(opts, latest, true) {
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
            update_via_github_release(opts, latest, true)?;
        }
    }
    Ok(())
}

/// The silent, blocking tail of [`Checker::auto_update`]: install the newer
/// release without any prompt or stdout output. Must run off the async
/// executor (see the call site) for the same reason as
/// [`run_self_update_blocking`].
///
/// Returns `Ok(true)` if the binary was replaced, `Ok(false)` if the install
/// was skipped because the running binary is a development build. Unlike the
/// interactive [`InstallMethod::CargoInstall`] path, a `cargo install` rebuild
/// is never attempted here: auto-update sticks to the GitHub release asset and
/// reports `Err` if none matches.
fn run_silent_update_blocking(opts: &KaishinOptions, latest: &LatestRelease) -> Result<bool> {
    let exe = std::env::current_exe().context("failed to resolve current_exe()")?;
    match detect_install_method(&exe) {
        // Never clobber a dev build, and never surface it as an error on the
        // background path — just decline.
        InstallMethod::DevBuild => Ok(false),
        InstallMethod::CargoInstall | InstallMethod::DirectBinary => {
            update_via_github_release(opts, latest, false)?;
            Ok(true)
        }
    }
}

/// Cross-process guard so two concurrently-starting instances don't both
/// self-replace the same binary. Acquisition is fail-open: if the lock can't
/// be taken, the caller simply skips the update this round.
///
/// Backed by an OS advisory file lock (`fs2`) on the lock file rather than a
/// hand-rolled "create_new + timestamp" scheme. The kernel releases the lock
/// automatically when the holder's file handle is closed —
/// including on a crash or kill — so there is no orphaned-lock state to reclaim
/// and no time-of-check/time-of-use window between a staleness check and the
/// acquire. The lock file itself is left in place (empty); its mere existence
/// means nothing, only the live advisory lock does.
struct UpdateLock {
    // Held only for its `Drop`, which closes the handle and releases the OS
    // lock. Never read directly.
    _file: std::fs::File,
}

impl UpdateLock {
    /// Tries to take the advisory lock at `path`. Returns `None` if another
    /// process currently holds it (the caller skips the update this round) or
    /// if the lock file can't be opened.
    fn acquire(path: &Path) -> Option<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .ok()?;
        // `try_lock_exclusive` is non-blocking: `Ok(())` means we hold the
        // exclusive advisory lock; any error (would-block or otherwise) means
        // we don't, so we decline rather than wait.
        use fs2::FileExt;
        match file.try_lock_exclusive() {
            Ok(()) => Some(Self { _file: file }),
            Err(_) => None,
        }
    }
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

fn update_via_github_release(
    opts: &KaishinOptions,
    latest: &LatestRelease,
    show_progress: bool,
) -> Result<()> {
    // Try the compiled-in target first (self_update picks it up automatically).
    let err = match try_github_release_with_target(opts, latest, None, show_progress) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };

    // On Linux x86_64, try the alternate libc ABI (musl↔gnu) so that releases
    // can migrate between them without breaking self-update.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if cfg!(target_env = "musl") {
            // musl binary falling back to gnu: only safe when glibc is present
            // on this system. On musl-only hosts (Alpine, etc.) a gnu binary
            // requires the glibc dynamic linker and won't execute.
            let has_glibc = std::path::Path::new("/lib/x86_64-linux-gnu/libc.so.6").exists()
                || std::path::Path::new("/lib64/ld-linux-x86-64.so.2").exists()
                || std::path::Path::new("/lib/ld-linux-x86-64.so.2").exists();
            if has_glibc {
                if let Ok(()) = try_github_release_with_target(
                    opts,
                    latest,
                    Some("x86_64-unknown-linux-gnu"),
                    show_progress,
                ) {
                    return Ok(());
                }
            }
        } else {
            // gnu binary falling back to musl: always safe — musl static
            // binaries carry their own libc and run on any Linux kernel.
            if let Ok(()) = try_github_release_with_target(
                opts,
                latest,
                Some("x86_64-unknown-linux-musl"),
                show_progress,
            ) {
                return Ok(());
            }
        }
    }

    // On Windows x86_64, try the alternate ABI (gnu↔msvc) for the same reason.
    // Most releases ship msvc; users who installed via cargo with the GNU
    // toolchain get a gnu binary that can still upgrade to an msvc release.
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        let alt = if cfg!(target_env = "gnu") {
            "x86_64-pc-windows-msvc"
        } else {
            "x86_64-pc-windows-gnu"
        };
        if let Ok(()) = try_github_release_with_target(opts, latest, Some(alt), show_progress) {
            return Ok(());
        }
    }

    Err(err)
}

fn try_github_release_with_target(
    opts: &KaishinOptions,
    latest: &LatestRelease,
    target_override: Option<&str>,
    show_progress: bool,
) -> Result<()> {
    let mut builder = self_update::backends::github::Update::configure();
    builder
        .repo_owner(&opts.owner)
        .repo_name(&opts.repo)
        .bin_name(&opts.bin_name)
        .show_download_progress(show_progress)
        .current_version(&opts.current_version)
        .target_version_tag(&latest.tag_name)
        .no_confirm(true);
    if let Some(t) = target_override {
        builder.target(t);
    }
    let status = builder
        .build()
        .context("build")?
        .update()
        .context("update")?;
    // Stay silent on the background auto-update path (`show_progress == false`);
    // the interactive flows keep their confirmation output.
    if show_progress {
        match status {
            self_update::Status::UpToDate(v) => {
                println!("\u{2713} {} {} is already up to date.", opts.bin_name, v)
            }
            self_update::Status::Updated(v) => {
                println!("\u{2713} {} v{} installed.", opts.bin_name, v)
            }
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

    #[test]
    fn test_update_lock_mutual_exclusion() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("update.lock");

        // First acquire wins and creates the (nested) lock file.
        let lock = UpdateLock::acquire(&path).expect("first acquire should win");
        assert!(path.exists());

        // A second acquire from an independent handle is refused while the
        // advisory lock is held (flock/LockFileEx conflict across handles).
        assert!(UpdateLock::acquire(&path).is_none());

        // Dropping the guard closes the handle, so the OS releases the lock...
        drop(lock);

        // ...and a later acquire succeeds again. The lock file itself is left
        // in place; only the live advisory lock gates acquisition.
        let lock = UpdateLock::acquire(&path).expect("acquire after release");
        drop(lock);
    }
}
