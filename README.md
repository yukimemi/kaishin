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
use kaishin::{KaishinOptions, run_self_update};

#[tokio::main]
async fn main() -> Result<()> {
    let opts = KaishinOptions::new(
        "yukimemi",
        "my-tool",
        "my-tool",
        env!("CARGO_PKG_VERSION")
    );

    // Run self-update command
    run_self_update(&opts, args.yes, args.check).await?;

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

## License

MIT
