# Review instructions for LANI

## Run

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- The Safari suite, `cargo test --test browser -- --ignored`, only when the human says you can. It
  drives Safari through WebDriver, so it needs Safari closed and the screen awake.

## Check

- `docs/design.md` is the specification. A change that it doesn't describe, or describes
  differently, is a finding, in the code or in the design.
- The areas the review's summary lists under "Worth particular attention".
- The tests cover what the summary claims.
