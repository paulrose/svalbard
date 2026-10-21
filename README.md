# Svalbard

Svalbard is an open-source Rust CLI for database backups.
This initial version provides help and version output. Backup and restore commands are not available yet.

## Build and run

Install Rust with [rustup](https://rustup.rs/).
The repository pins Rust 1.93.1, which is also the minimum supported Rust version.
Rustup selects this toolchain from `rust-toolchain.toml`.

```sh
cargo build --locked --release
./target/release/svalbard --help
./target/release/svalbard --version
```

Help and version output go to stdout with exit code `0`.
Missing, unknown, or extra arguments produce a diagnostic on stderr with exit code `2`.
The CLI does not prompt for input.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Create a branch for each change. Submit changes through a pull request.
Keep PRs focused and include relevant check results. Review the diff before a solo merge.
GitHub deletes merged head branches automatically.

## Website preview

The splash page and future documentation live in `site/`. They use plain HTML, CSS, and JavaScript, with no build step or runtime dependencies.

```sh
python3 -m http.server 4173 --bind 127.0.0.1 --directory site
```

Open <http://127.0.0.1:4173>. The backup animation is a simulation of the planned workflow.
Auto appearance follows the estimated solar elevation in Longyearbyen. Day and Night provide manual previews.
The estimate excludes weather and terrain. Reduced-motion settings show the completed example without animation.

Run the daylight checks with Node.js 18 or later:

```sh
node --test site/tests/daylight.test.mjs
```

## License

[MIT](LICENSE).
