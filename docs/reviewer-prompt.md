# Reviewer prompt with `/code-review`

A prompt for the Claude Code session that reviews a LANI review. It uses Claude Code's
`/code-review` skill to find bugs, adds the checks LANI reviews need, and files every finding as a
LANI issue, so the turn-taking loop (design §11.5) stays the same.

## Notes

- `/code-review` finds correctness bugs, plus some reuse and simplification cleanups, and lists
  them in the session. It does not check that `docs/design.md` matches the code, or the review
  summary's "Worth particular attention" list, so the prompt keeps a separate pass for those.
- Its findings have no severity, and each is anchored to a single line. The reviewer assigns the
  severity and the line ranges when filing them with `create_issue`.
- Don't pass `--fix` (it edits code, which is the implementer's job) or `--comment` (it posts to a
  GitHub PR, which LANI branches don't have).
- `ultra`, the multi-agent cloud review, is billed and only the human can start it, so the prompt
  uses a local level such as `high` or `xhigh`.
- Not yet confirmed: which base `/code-review` compares a branch against, and whether it accepts a
  commit range. The prompt has the reviewer check the range it reviewed. Rerunning it in later
  rounds probably re-reviews the whole branch, not just the fixes.

## Prompt

Replace the `<…>` values.

```
You are the reviewer for LANI review <N> in this repository. Use the lani MCP tools with author "reviewer".

1. Call get_review for review <N> and read the summary and the hand-off note. The branch is <branch>, reviewed against <base>; note the head commit.
2. Run the code-review skill on the branch: invoke it with args "xhigh <branch>". Do not pass --fix or --comment. Check that what it reviewed matches `git diff <base>...<branch>`; if it covered a different range, say so.
3. Do LANI's own pass, which code-review doesn't cover:
   - the areas the summary lists under "Worth particular attention";
   - whether docs/design.md still matches the code;
   - whether the tests cover what the summary claims.
   Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. If Safari's remote automation is available, also run `cargo test --test browser -- --ignored`.
4. Verify every finding against the code before filing it, whether it came from code-review or your own pass. Reproduce it where you can, and don't file findings you can't support. File each one with create_issue:
   - a one-line title and a severity (high, medium, low or nit), which you assign;
   - one span per affected range, with the right start_line_text;
   - commit = the review's head SHA and worktree_path = this repository's path;
   - a body that explains the failure scenario. Say if the finding came from code-review and whether it was confirmed or only plausible.
5. Call hand_off for review <N> to the implementer with a note summarizing your findings. LANI gives your first hand-off to me instead, so I read the findings first; tell me when it does. Then run `lani wait --review <N> --for reviewer` as a background command.
6. When it is your turn again, verify each reply against the code with `git diff <previous head> <new head>`. Resolve the issues you're satisfied with, reply on the others, and file new findings. Rerun code-review only if the fixes are large. When everything is resolved, hand the review to the human and tell me.
```
