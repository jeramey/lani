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
  reviewer's first findings come to you before the implementer acts on them. After five rounds
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

To start a reviewer session, copy a ready-made prompt from the review page's *Reviewer prompt*
button and paste it into the session.
- **Two versions.** One is for Claude Code: it uses `/code-review` to find bugs and waits in the
  background. The other is for other harnesses that can use MCP servers, such as OpenCode.
- **Your edits.** You can edit a prompt before copying it. The page keeps the edit you copied
  last for the next time.
- **Project instructions.** Put review instructions for your project, such as which checks to
  run, in `REVIEW.md` at the top of the repository. The prompts tell the reviewer to read it
  from the base branch, so the branch under review can't change it. Instructions your harness
  loads itself, such as `CLAUDE.md`, come from the checkout the reviewer runs in.

### The web UI

`lani open` starts the web server if it isn't running and opens LANI in your browser. Claude
can open it too, at a review, an issue, a file or a line.

There you can:
- read and edit everything, comment, and resolve or reopen issues;
- rename a review, change its base, and edit an issue's title or severity;
- hand the review on;
- copy a prompt to start a reviewer session with;
- close the review when you're done.

The settings page sets how long the server waits before stopping when idle (3 hours by default,
or never) and the round limit (5 by default).

The server listens only on 127.0.0.1 and asks for a random token, which `lani open` passes to your
browser. `lani open --print-url --no-browser` prints a link with the token to open by hand instead,
starting the server if needed. `lani status --print-url` prints the running server's link and never
starts one; it exits 1 if no server is running.

### On a remote machine

LANI can run on the machine where the repository and the Claude Code sessions are, with your local
browser reaching it over SSH port forwarding.

**Socket mode** is the safer way, and the one to use on a machine other people also use.

- **How it works.** The server listens on a Unix socket in your private LANI directory instead of a
  TCP port, and you forward a local port to that socket.
- **Why it is safer.** Nobody else on the remote machine can connect to LANI. Nobody can take the
  port your forward points at, either, which would hand them your sign-in token.
- **Turning it on.** Set it in `~/.config/lani/config.toml` on the remote machine, so that every
  server started there uses it, including those Claude starts:

  ```toml
  listen = "socket"
  ```

- **Starting it.** Run `lani socket` on the remote machine. It starts the server on the socket,
  or finds the running one. Then it prints the `ssh -L` command to run on your own machine, and
  the sign-in link to open there:

  ```
  To reach LANI from your own machine, forward a local port to it over SSH (devbox is this
  machine's name; use the name you ssh to):
  ssh -o ExitOnForwardFailure=yes -L 5264:/home/you/.local/share/lani/run/web.sock devbox
  To sign in, open this link; it contains the access token, so keep it private:
  http://127.0.0.1:5264/login?token=…&next=/
  ```

- **A forward set up once.** The socket's path never changes, so the forward can go in your
  `~/.ssh/config`:

  ```
  Host devbox
    LocalForward 5264 /home/you/.local/share/lani/run/web.sock
    ExitOnForwardFailure yes
  ```

- **If 5264 is taken on your own machine,** for example by a local LANI, the printed command's
  `ExitOnForwardFailure=yes` makes ssh stop with an error. Without it, ssh would only warn, and the
  sign-in link would reach that program instead. Set `port` to a free port in the remote machine's
  configuration file, or run `lani socket --port N`; the printed command and link follow it. A
  running server keeps the port it started with, so run `lani stop` first.
- **No browser on the remote machine.** In socket mode, `lani open` and Claude's `open_web_ui`
  never open one. They print the forward command and the page's address, and Claude never sees
  your token. To sign in, run `lani socket` on the remote machine.
- **What ssh needs.** It needs OpenSSH 6.7 or later at both ends. The remote sshd must allow
  Unix-socket forwarding, which it does by default (`AllowStreamLocalForwarding`).

**TCP forwarding** works too, with the server's default port:

```sh
ssh -L 5264:127.0.0.1:5264 remote-host
lani serve --detach        # on the remote host; prints the sign-in link
```

- **Opening it.** Open the printed link in your local browser. `lani serve` starts the server
  without opening a browser, and prints the link in the foreground too.
- **A different remote port.** If the server took a port above 5264 because 5264 was in use,
  forward that port.
- **5264 taken on your own machine,** for example by a local LANI: forward another local port, as
  in `-L 5300:127.0.0.1:5264`, and change the link's port to match.

## Command line

```
lani mcp                         the MCP server; Claude Code starts it
lani open [--review ID [--view overview|changes|files] [--file PATH[:LINE]] | --issue ID]
          [--print-url] [--no-browser]
                                 open the web UI, starting the server if needed
lani serve [--port N] [--listen tcp|socket] [--idle-timeout DURATION|never] [--detach]
                                 run the web server; --port N uses exactly port N;
                                 --listen socket listens on a Unix socket, for SSH
lani socket [--port N]           start the server on a Unix socket for use over SSH, or find
                                 it, and print the ssh -L command and the sign-in link
lani status [--print-url]        whether the server is running, and its idle timeout;
                                 --print-url prints only its sign-in URL
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

`listen = "socket"` puts the server in socket mode, for use over SSH (see "On a remote machine").
`port` is then the local port that the printed command and links suggest you forward from.
`lani serve --listen tcp|socket` overrides it for one run.

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
