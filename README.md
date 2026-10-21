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
The colours follow the estimated solar elevation in Longyearbyen. The estimate excludes weather and terrain.
Reduced-motion settings show one frame of the upload, after the snapshot has closed.
For previews, `?theme=light` or `?theme=dark` overrides daylight, and `?at=<seconds>` holds the animation at one moment.

Run the site checks with Node.js 18 or later:

```sh
node --test site/tests/*.test.mjs
```

## License

[MIT](LICENSE).
