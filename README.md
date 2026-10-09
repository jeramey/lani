# LANI — Local Annotations, No Internet

LANI is a local code-review loop for [Claude Code](https://claude.com/claude-code), in place of a
GitHub pull request. A Claude Code *reviewer* session files issues against a git branch, a Claude
Code *implementer* session fixes the code and replies, and you read, edit and decide in a web UI
on your machine. The two sessions can take turns by themselves until the review needs you. You
open the GitHub PR once the loop has converged.

It is a single Rust binary with SQLite storage, an MCP server for Claude Code and a local web UI.
It makes no network connections, and it only reads your repositories; it never writes to them.

## What it does

- **Reviews of branches.** A review compares a branch with its parent branch, found
  automatically, or with a base you choose. A whole-tree review covers a branch entirely, with no
  base. One review is open per branch.
- **Issues on lines.** Every issue has a title, a severity (`high`, `medium`, `low` or `nit`) and
  one or more line spans, and a linear thread of comments. Issues follow their lines as the code
  changes, and say when those lines are gone.
- **Clear authorship.** It is always visible whether you, the reviewer or the implementer wrote
  something. You can edit any text, and edits are marked.
- **Who resolves.** Only the reviewer and you can resolve issues; the implementer can't.
- **Taking turns.** Sessions hand the review to each other and wait with `lani wait`. The
  reviewer's first findings come to you before the implementer sees them. After five rounds
  without you, or when a session needs a decision, the review comes to you again. A session can
  also ask you about a single issue.
- **The web UI.**
  - It lists reviews across all your repositories.
  - Each review has an overview, an issue page per issue, a highlighted file browser and a diff
    view. You can file your own issues by selecting lines.
  - Pages update live while the sessions work. Nothing you are typing is overwritten.
- **Only committed code.** Uncommitted changes are reported but can't be reviewed, as on GitHub.

## Requirements

- macOS or Linux.
- git 2.41 or later.
- Rust 1.89 or later, and a C compiler: SQLite and the regex engine used for highlighting are
  built from source.
- Claude Code.

## Install

```sh
cargo install --path . --locked                         # installs ~/.cargo/bin/lani
claude mcp add --scope user lani -- "$(command -v lani)" mcp
```

The absolute path in the second command keeps Claude Code from picking up a different `lani` from
another `PATH`. To skip a permission prompt for every LANI tool, add `"mcp__lani"` to
`permissions.allow` in your Claude Code settings.

## Using it

Run two Claude Code sessions in the repository, one as the reviewer and one as the implementer.
They can share a checkout. Tell each one what to do in plain words; LANI's MCP server explains its
tools to them. For example:

- **Implementer, to open a review like a PR:** "Open a lani review of this branch with a summary
  of what it does and why."
- **Reviewer:** "Start a lani review of this branch." On the default branch, "Start a lani review
  of all of main" gives a whole-tree review.
- **Implementer:** "Pull the open lani issues, validate them, fix the valid ones, commit, and
  reply to each issue."
- **Reviewer:** "Check the lani replies and resolve the issues that are actually fixed."
- **Either session:** "Open the lani review in my browser", or "Show me lani issue 12".

To let the sessions run the loop on their own, ask either one to "hand it to the reviewer (or the
implementer) and wait for your turn". Each session then hands the review on when it finishes a
pass. It waits with `lani wait` as a background command and continues when the review comes
back. The reviewer's first hand-off comes to you, so you read the first findings before the
implementer acts on them. After that, the review comes to you when every issue is resolved, when a
session needs your decision, or after the round limit; the session that hands it to you tells
you. You hand it on from the review page.

[`docs/reviewer-prompt.md`](docs/reviewer-prompt.md) is a ready-made prompt for the reviewer
session. It uses Claude Code's `/code-review` to find bugs and files each finding in LANI.

### The web UI

`lani open` starts the web server if it isn't running and opens LANI in your browser. Claude
can open it too, at a review, an issue, a file or a line.

There you can:
- read and edit everything, comment, and resolve or reopen issues;
- rename a review, change its base, and edit an issue's title or severity;
- hand the review on;
- close the review when you're done.

The settings page sets how long the server waits before stopping when idle (3 hours by default,
or never) and the round limit (5 by default).

The server listens only on 127.0.0.1 and asks for a random token, which `lani open` passes to your
browser. `lani open --print-url --no-browser` prints a link with the token to open by hand instead,
for example over SSH with port forwarding.

## Command line

```
lani mcp                         the MCP server; Claude Code starts it
lani open [--review ID [--view overview|changes|files] [--file PATH[:LINE]] | --issue ID]
          [--print-url] [--no-browser]
                                 open the web UI, starting the server if needed
lani serve [--port N] [--idle-timeout DURATION|never] [--detach]
                                 run the web server; --port N uses exactly port N
lani status                      whether the server is running, and its idle timeout
lani stop                        stop the server
lani wait --review ID --for reviewer|implementer [--timeout DURATION]
                                 wait until it is that role's turn
lani version
```

`lani wait` exits 0 when it is the role's turn, 3 if the review was closed and 4 on timeout. Every
command exits 1 on an error and 2 on a usage error.

## Where data lives

Everything is in one SQLite database under `$LANI_HOME`, which defaults to `$XDG_DATA_HOME/lani`,
or `~/.local/share/lani`, on macOS too. It must be on a local filesystem. The directory is private
to you (mode 700), and LANI refuses to use one that other users can access. The server's log is
`$LANI_HOME/logs/server.log`.

The environment variables are `LANI_HOME`, `BROWSER` (the command that opens LANI in your browser,
instead of `open` on macOS or `xdg-open` on Linux) and `LANI_LOG` (a `tracing` filter for logging
to stderr, such as `debug`).

## Configuration

The web server listens on port 5264 on 127.0.0.1, or the first free port above it. To use another
base port, put it in `~/.config/lani/config.toml` (`$XDG_CONFIG_HOME/lani/config.toml`):

```toml
port = 7300   # 0 lets the OS pick any free port
```

`lani serve --port N` overrides it for one run and uses exactly port N. Browsers refuse a few ports,
including the IRC ports 6665–6669, so LANI refuses them too and skips them when counting up.

## Upgrading

Reinstall with `cargo install --path . --locked`. The database is migrated forward the first time
the new version opens it. Processes that are already running keep the old version until they are
restarted, and they say so:

- **The web server** shows a banner on every page. Run `lani stop`, then `lani open`.
- **Each Claude Code session's `lani mcp`** adds a note to its tool results. Reconnect it with
  `/mcp`, then select lani and choose Reconnect. The session itself doesn't need restarting.

An older LANI never writes to a database that a newer one has migrated.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Two test suites are opt-in:

- **Browser tests** drive Safari through WebDriver: `cargo test --test browser -- --ignored`.
  Enable Safari's automation once: Safari → Settings → Advanced → "Show features for web
  developers", then Develop → "Allow Remote Automation", and run `safaridriver --enable`.
- **Benchmarks** check the performance budget against a release build:
  `cargo test --release --test bench -- --ignored --nocapture`.

The design, including the MCP interface, the security model and the performance budget, is in
[`docs/design.md`](docs/design.md).

## License

MIT; see [`LICENSE`](LICENSE).
