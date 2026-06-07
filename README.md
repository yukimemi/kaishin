# kaishin

<p align center>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/kaishin/main/assets/logo-dark.svg">
    <img alt="kaishin logo" src="https://raw.githubusercontent.com/yukimemi/kaishin/main/assets/logo.svg" width="500">
  </picture>
</p>

Universal self-update library for Rust CLIs, extracted from rvpm and renri.

## Features

- GitHub Releases API integration
- Automatic installation method detection (cargo install / dev build / direct binary)
- Background update check with throttling via [`Checker`]
- Silent background auto-update (Claude-Code style) via [`Checker::auto_update`] / [`Checker::spawn_auto_update`]
- Customizable update banner
- Interactive/Non-interactive update flow

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
kaishin = "0.1"
```

### Self-Update Command

```rust
use kaishin::{KaishinOptions, UpdateOptions, run_self_update};

#[tokio::main]
async fn main() -> Result<()> {
    let opts = KaishinOptions::new(
        "yukimemi",
        "my-tool",
        "my-tool",
        env!("CARGO_PKG_VERSION")
    );
    // If the crates.io package name differs from the binary name
    // (e.g. package `my-tool-cli` ships the binary `my-tool`), tell
    // the `cargo install` fallback which package to build:
    // let opts = opts.crate_name("my-tool-cli");
    let upd_opts = UpdateOptions::new()
        .yes(args.yes)
        .check_only(args.check)
        .non_interactive(args.non_interactive);

    // Run self-update command
    run_self_update(&opts, upd_opts).await?;
    // Note: In non-interactive mode (non_interactive: true), the updater will exit 
    // unless --yes (yes: true) is provided.

    Ok(())
}
```

### Background Update Check

Using `Checker` to handle state and throttling automatically:

```rust
use kaishin::{KaishinOptions, Checker};

#[tokio::main]
async fn main() -> Result<()> {
    let opts = KaishinOptions::new("yukimemi", "rvpm", "rvpm", env!("CARGO_PKG_VERSION"));
    let checker = Checker::new("rvpm", opts);

    // 1. Check in background (with 24h throttle)
    if checker.should_check() {
        let checker_clone = checker.clone();
        tokio::spawn(async move {
            let _ = checker_clone.check_and_save().await;
        });
    }

    // ... run app ...

    // 2. Show banner at the end if update is available
    if let Some(latest) = checker.cached_update() {
        eprintln!("\n{}", checker.format_banner(&latest));
    }

    Ok(())
}
```

### Silent Background Auto-Update

Like Claude Code, you can update silently in the background instead of just
notifying. `spawn_auto_update` fires a detached task that checks, downloads,
and replaces the binary with no prompts and no output. The running process
keeps the old binary; the new version takes effect on the next launch.

```rust
use kaishin::{KaishinOptions, Checker};

#[tokio::main]
async fn main() -> Result<()> {
    let opts = KaishinOptions::new("yukimemi", "rvpm", "rvpm", env!("CARGO_PKG_VERSION"));
    let checker = Checker::new("rvpm", opts);

    // Fire-and-forget: self-throttled (24h by default), so it's safe to call
    // on every startup. Opt-out is up to you — gate this behind your own env
    // var or config if you want users to be able to disable it.
    if std::env::var_os("RVPM_NO_AUTOUPDATE").is_none() {
        checker.spawn_auto_update();
    }

    // ... run app ...
    Ok(())
}
```

Notes:

- A dev build (under `target/`) is never overwritten — the call is a no-op.
- For a `cargo install`-managed binary, auto-update only swaps the GitHub
  release asset; it never triggers a (slow, noisy) `cargo install` rebuild on
  this path. If no matching asset exists, the update is skipped.
- Updates are serialised across processes by an OS advisory lock, so two
  instances starting at once won't both self-replace (and the lock is released
  automatically if a process exits or crashes).
- Use [`Checker::auto_update`] directly (instead of `spawn_auto_update`) if you
  want to await the result and learn which version was installed.

## License

MIT
