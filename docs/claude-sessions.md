# Claude Code sessions from the web UI (M5 proposal)

**Status: a proposal for review. Nothing here is built.** Once the open questions are settled, the
decided design moves into `docs/design.md`, and this file is removed.

## Goal

From a review page, you start a Claude Code session for that review, as the reviewer or the
implementer, with instructions already given. Then you work with it in a browser tab. This should
work on your own machine and on a remote one reached over SSH (design §13.2, "Remote use over
SSH"), where opening a local terminal is not an option.

## What Claude Code already provides

Checked against `claude --help` in Claude Code 2.1.295:

- **`claude --bg [-n <name>] [prompt]`** starts a session in the background and prints its short
  id. Claude Code manages the process, not the terminal that started it.
- **`claude attach <id|name>`** opens a background session in a terminal. Part of the name works
  in place of the id. Leaving with ← (agent view) or Ctrl+Z leaves the session running.
- **`claude agents --json`** lists the active sessions, both interactive and background. For
  each one it gives `pid`, `cwd`, `kind`, `startedAt`, `sessionId`, `name` and `status`.
- **Other commands:**
  - `claude logs <id>`: the recent terminal output.
  - `claude stop <id>`: stops the session but keeps the conversation, which `attach` reopens.
  - `claude respawn <id>`: restarts it on the current Claude Code binary.
  - `claude rm <id>`: deletes it.
- **Session options:** `-n/--name`, `--session-id <uuid>`, `--resume`, `--permission-mode`,
  `--append-system-prompt`, and `-w/--worktree`, which creates a new git worktree for the
  session.

So Claude Code can already host sessions that outlive the terminal and the browser. It also
provides the command that attaches a terminal to them.

## Proposal: Claude Code hosts the sessions, and LANI starts and shows them

There are two phases. Each is useful on its own.

### Phase A: start a session, see its status, attach from a terminal

- **The buttons.** The review page gets *Start reviewer session* and *Start implementer session*
  buttons. Each opens a form that shows two things:
  - The prompt, prefilled and editable. The reviewer's comes from `docs/reviewer-prompt.md`, and
    the implementer's from a new, matching template. Both have the review number, branch, base
    and head filled in.
  - The directory the session will run in. It is the worktree where the review's branch is
    checked out, found with `git worktree list --porcelain`. Both roles need that checkout,
    because `create_issue` checks the file on disk against the commit.
- **Starting.** The server runs this in that directory:

  ```
  claude --bg -n lani-<review>-<role> --session-id <uuid> "<prompt>"
  ```

  It records the name and session id with the review.
  - LANI never passes `--dangerously-skip-permissions` or a bypass permission mode.
  - The session runs with your normal Claude Code settings and LANI MCP registration, and the
    prompt tells it its role.
- **One session per role per review.** The name `lani-<review>-<role>` lets the server find the
  session in `claude agents --json`. While one is running, the button shows its status instead of
  starting another.
- **Attaching.** The review page shows the command, `claude attach lani-3-reviewer`, for any
  terminal on the machine where LANI runs. Over SSH, that's the remote machine.
- **Size:** about the size of the port change (item 3). It needs a prompt template, a form, one
  subprocess call, one JSON parse and a status line.

### Phase B: the terminal in a browser tab

- **The page.** A session page, `/reviews/{id}/sessions/{role}`, shows a terminal drawn by
  xterm.js. It is MIT-licensed and vendored into the binary, like everything else (no CDN).
- **The connection.** The page opens a WebSocket. For each open tab, the server runs
  `claude attach <name>` in a pseudo-terminal and relays bytes in both directions. It also passes
  resize messages on as the terminal's window size. Closing the tab ends only that `attach`; the
  session keeps running.
- **New dependencies:**
  - axum's `ws` feature (tokio-tungstenite).
  - A PTY crate: `portable-pty` or `pty-process`. Setting up a controlling terminal needs
    `setsid` and an ioctl, which LANI's `unsafe_code = "forbid"` rules out in its own code.
  - xterm.js and its fit add-on.
- **Size:** about the size of M4's live updates, including Safari tests against a fake `claude`.

### Why Claude Code should host the sessions

| Option | Survives a LANI restart or upgrade | Attach from a terminal too | Extra dependency | Work in LANI |
| ------ | ---------------------------------- | -------------------------- | ---------------- | ------------ |
| **Claude Code background sessions (proposed)** | Yes | Yes, `claude attach` | None | Least: LANI starts and attaches |
| Sessions held in the LANI server's own PTYs | No; `lani stop`, every upgrade and the idle timeout end them. `--resume` can restore the conversation, but not a running `lani wait` | No | None | Most: process supervision, a scrollback buffer for reconnecting, cleanup |
| tmux sessions | Yes | Yes, `tmux attach` | tmux, which this Mac doesn't have | Medium, plus tmux's own scrolling and mouse handling inside xterm.js |

The idle timeout doesn't stop sessions in any of these options, but in the second one it would.

## Security

Today a stolen token lets someone read and edit reviews. With either phase it lets them run code
as you:
- **Phase A:** they can start a session with a prompt of their choosing. Whether that runs
  commands at once depends on your Claude Code permission rules.
- **Phase B:** they can type into a running session.

On a single-user machine this adds no exposure, because anything that could steal the token could
already run commands as you. On a shared machine, it makes the residual risk in design §15 (the
cookie reaching other loopback ports) serious. Before shipping, then:

1. **Harden the cookie.** Build the hardening deferred in design §21, the per-server secret path
   prefix with the cookie's `Path` set to it. It should work in every browser.
2. **Check `Origin` on the WebSocket upgrade.** The upgrade is a GET, which the current guard
   doesn't check for origin, and WebSockets aren't covered by CORS. A page served from another
   loopback port counts as same-site, so the browser sends the `SameSite=Strict` cookie with that
   page's WebSocket handshake. Only an `Origin` check refuses it.
3. **No permission bypass.** Never pass `--dangerously-skip-permissions` or a bypass mode.
4. **Fixed command line.** The command line is fixed apart from the prompt text and the
   directory. The prompt is passed as an argument, never through a shell.

## Lifecycle

- **Who owns the sessions.** Sessions belong to Claude Code. They survive LANI's restarts,
  upgrades and idle shutdowns, and `claude respawn` moves them to a new Claude Code version.
- **Closing a review.** It could offer to stop the review's sessions with `claude stop`; see open
  question 5.
- **Waiting.** A session waits for its turn with `lani wait` as a background command, as it does
  now, so the loop works the same in a started session as in one you opened yourself.
- **Idle timeout.** An open terminal tab, like a live-update stream, shouldn't keep the server
  alive by itself. Typing into it counts as activity.

## Open questions for you

1. **Phases.** Phase A first, then B? Or both together?
2. **Roles.** Buttons for both roles, or only the reviewer? The implementer edits the checkout of
   the branch, which may be the one you are working in.
3. **No checkout of the branch.** If the branch isn't checked out anywhere, should LANI refuse,
   or let Claude Code create a worktree with `-w`? It's not yet checked which branch a `-w`
   worktree uses. If it makes a new one, the implementer's commits would need to reach the
   review's branch somehow.
4. **Permission mode.** Claude Code's default, where prompts wait until you attach? Or
   `acceptEdits` for the implementer, so it can edit files without asking but still asks before
   running commands?
5. **Closing a review.** Should closing it stop the review's sessions, offer to, or leave them?
6. **The prompts.** Compiled into the binary, or editable, for example in the configuration file
   or on the Settings page?

## To check before building

Each of these is a short experiment or measurement:

- **How `claude --bg` behaves:**
  - Does it run in the current directory when `-w` isn't given?
  - Does the initial prompt start at once?
  - Does `--session-id` work together with `--bg`?
  - Which `status` values does `claude agents --json` report? Only `busy` has been seen so far.
  - Can LANI tell that a session is waiting for a permission answer?
- **`claude attach` in a pseudo-terminal** that LANI creates, with xterm.js as the terminal
  (`TERM=xterm-256color`).
- **Finding `claude` on `PATH`.** Can the detached server find it when `lani mcp` started the
  server from Claude Code's environment? If not, it needs a `claude` key in the configuration
  file.
- **xterm.js and LANI's CSP.** The CSP allows styles set from scripts but not `<style>` elements.
  If xterm.js inserts style elements, the terminal page needs a per-response nonce.
- **Size.** The size of the vendored xterm.js and the fit add-on.
- **Interface stability.** How stable `claude agents --json` and `claude attach` are as an
  interface, since LANI would depend on them. Background sessions are a recent Claude Code
  feature.
