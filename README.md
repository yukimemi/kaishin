# kaishin

Universal self-update library for Rust CLIs, extracted from rvpm and renri.

## Features

- GitHub Releases API integration
- Automatic installation method detection (cargo install / dev build / direct binary)
- Background update check with throttling
- Customizable update banner
- Interactive/Non-interactive update flow

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
kaishin = { git = "https://github.com/yukimemi/kaishin" }
```

In your code:

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

## License

MIT
