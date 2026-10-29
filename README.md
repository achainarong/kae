# kae

**แก้ — fix, resolve.** A Rust Git TUI with a dedicated merge-resolution workspace.

**Status: MVP / alpha.** Real Git operations, not placeholder screens. Try it on a disposable repository first; this is not a production-hardened replacement for every Git command.

## Install and run

Requires Rust 1.88+, Git 2.38+, and an interactive terminal. Make is optional. Development and terminal checks currently run on macOS; Windows is not supported in this MVP.

From this directory:

```sh
make install
kae -C /path/to/repository
```

Cargo installs into its bin directory, usually `~/.cargo/bin`; put that directory on `PATH`. Without Make, use `cargo install --path . --locked`.

To run without installing:

```sh
make run ARGS="-C /path/to/repository"
# or
cargo run --locked -- -C /path/to/repository
```

Launch `kae` inside a repository or use `-C` / `--path`. Subdirectories and linked worktrees are supported. `--help` and `--version` work without a terminal.

## Workspaces

| Key | Workspace | Features |
| --- | --- | --- |
| `1` | Files | Status, staged/unstaged diffs, text previews, file/hunk staging, commit, amend, external editor, confirmed discard |
| `2` | History | Commit graph with branch/tag labels, commit diffs, compare, cherry-pick, revert, interactive rebase |
| `3` | Branches | Create, switch, rename, safe deletion, compare, merge preview, merge |
| `4` | Remotes | Fetch, explicit pull strategy, non-forced push, upstream setup, ahead/behind status |
| `5` | Stashes | Save including untracked files, inspect, apply, pop, drop |
| `6` | Tags | Inspect, create a lightweight tag at HEAD, delete a local tag |
| `7` | Worktrees | List, add a detached worktree, open, remove a clean inactive worktree |

Press **`:`** or **`/`** for searchable, context-aware commands. Some operations are intentionally available only through command search to keep the default shortcuts small.

## Keyboard navigation

| Key | Action |
| --- | --- |
| `1`–`7`, `[` / `]` | Select workspace, previous/next workspace |
| `j` / `k`, arrows | Move selection or scroll focused preview |
| `Tab` | Switch list/preview focus |
| `h` / `l`, left/right | Scroll horizontally |
| `PageUp` / `PageDown`, `Home` | Scroll preview, return to the top |
| `s` / `u`, `S` / `U` | Stage/unstage selected file, or all files |
| `v` | Open hunk staging for the selected file |
| `c` / `a` | Commit staged changes / amend HEAD |
| `e` | Open selected file in `$VISUAL`, `$EDITOR`, or `vi` |
| `b` | Create and switch to a branch |
| `m` / `M` | Preview / perform merge from selected revision |
| `i` | Edit rebase sequence after the selected base commit |
| `f` / `p` / `P` | Fetch / pull fast-forward only / push |
| `C` / `A` | Continue / confirm abort of current operation |
| `r` | Refresh (changes made outside Kae are not watched automatically) |
| `~` / `!` | Last operation/error details |
| `?` | Help; `j`/`k` scroll it |
| `Esc`, `q` | Cancel/close overlay; `q` quits from the main view |

In prompts: type, Backspace, **Ctrl-U** to clear, Enter to accept, Esc to cancel. In confirmations, only **`y`** proceeds; `n` or Esc cancels. A `q` typed into a prompt is input, not quit.

Wide terminals use side-by-side panes; narrower terminals stack them. Main navigation needs 44 × 12; conflict, hunk, rebase, and confirmation screens need 60 × 24. Editing/actions pause when the terminal is too small. Recommended: 120 × 36 or larger.

## Merge and resolve

1. Open **Branches**, select the source branch, and press `m`.
2. Review incoming commits, conflict analysis, and the proposed merged diff. The preview may write unreachable Git objects, but does not change HEAD, the index, or working files.
3. Close the preview, press `M`, and confirm the source/destination.
4. If Git stops with conflicts, open **Files** and press Enter on a conflicted file.
5. Choose resolutions region by region:

   | Key | Conflict action |
   | --- | --- |
   | `1` / `2` | Take current / incoming region |
   | `3` / `4` | Combine current then incoming / incoming then current |
   | `n` / `p` | Next/previous region |
   | `u` | Undo a resolution choice (up to 256 choices, in this session) |
   | `b` | Show common ancestor; region context for diff3, otherwise the base file |
   | `j` / `k`, `h` / `l` | Synchronized vertical/horizontal scrolling |
   | `s` | Save the result to the working file, **without staging** |
   | `a` | Explicitly stage a fully resolved, saved result |
   | `e` | Open the saved file in your external editor |

   Current/incoming panes show word differences; the result shows the complete combined file. Missing final newlines and CRLF text are preserved by the resolution logic. Unsaved choices require confirmation before being discarded.

6. Repeat for other files. Use `:` → **Review full integration diff** to inspect automatically merged changes too.
7. Press `C` to continue. Press `A` for a confirmed abort instead.

Binary, symlink/type, deleted/modified, and non-text conflicts use **Conflict: take whole current file / incoming file / delete file** in command search. Whole-file side choices replace the working file but do not stage it; deletion explicitly removes the file and stages the deletion. Git refuses unsafe deletion without force. For rename conflicts, resolve and stage each path Git reports.

During rebases, **current is the onto/current tree**, and **incoming is the replayed commit**. Kae shows this distinction and the relevant commit rather than presenting unexplained “ours/theirs.” Conflicts from stash application can also be resolved, but there is no separate stash “continue” operation.

## Interactive rebase

In **History**, select an ancestor as the **base**, then press `i`. The sequence contains commits **after** that base, oldest first.

- `j` / `k`: select; **`J` / `K`**: reorder.
- `p`: pick; `r`: enter a replacement message; `s`: squash; `f`: fixup; `d`: drop; `e`: stop for editing.
- Enter validates the sequence and opens a final confirmation. Esc cancels without rewriting anything.
- For `edit`, amend with `a`, then press `C` to resume.
- For conflicts, use the same resolution workspace, explicitly stage, then `C`.
- Command search provides skip; `A` offers abort.

Kae warns if selected commits are reachable from remote-tracking refs. Fetch first if those refs may be stale. Before starting, it preserves original HEAD under **`refs/kae/backup/<timestamp>`**; the reference is shown in operation output. Backups are retained and excluded from the normal graph.

The rebase plan persists in the worktree's Git metadata so reopening Kae can continue a paused rebase with the chosen messages. A completed/aborted Kae rebase removes that plan, not its backup. Inspect backups with `git for-each-ref refs/kae/backup`; recover non-destructively by creating a branch at the desired backup reference.

## Safety and current limits

- Switch, merge, pull, cherry-pick, revert, rebase, and stash application require a clean working tree/index. Commit or stash first; Kae does not silently autostash.
- Destructive actions, history rewrites, merges, pulls, and pushes have explicit review dialogs. Unmerged files cannot be staged while standard conflict markers remain. Undo applies to conflict choices, **not** discard, commits, or other Git operations.
- Actions recheck HEAD/branch identity, refs, index, changed-file contents, and remote/upstream configuration. Stale views fail rather than applying an action to an unexpected state. Avoid concurrently editing a file while saving its resolution; these are optimistic checks, not a repository-wide lock.
- Push never uses force, does not implicitly push tags/submodules, and names its destination explicitly. It follows the current upstream's branch on that remote when configured; otherwise it targets the current branch name. Command search includes **Push and set upstream**.
- The Remotes workspace selects a remote explicitly. Elsewhere Kae uses the upstream remote, then `origin`, then the first configured remote; pull/push review shows the choice. Pull defaults to fast-forward-only; merge/rebase strategies are separate commands. Other branches are not automatically updated by rebase.
- Configure credentials, SSH/GPG agents, and remotes using Git externally. Git terminal credential prompts are disabled; configured hooks/helpers still run and may block. URLs are omitted from remote details, and common URL credentials/query tokens are redacted from operation logs. Arbitrary hook output is not guaranteed secret-free.
- Git mutations run in a worker, so the UI stays available for help. During an operation, `q` / Ctrl-C requests a quit after completion. Termination signals follow the same orderly shutdown; Kae does not kill a possibly writing Git process. File previews and refreshes are synchronous and can pause on large repositories.
- History and rebase plans are limited to 300 commits. No graph pagination, root-commit rebase, merge-preserving rebase, remote configuration editor, remote branch deletion, submodule-management UI, or built-in text editor yet.
- Hunks support modified text files. Use whole-file staging for additions, deletions, renames, binary files, and conflicts. Text conflict editing is limited to 256 KiB; use whole-file choices or your editor for larger files. No built-in mouse interaction yet.
- Git output capture is bounded to 2 MiB per stream while still draining the process. Oversized metadata/diffs produce an actionable error instead of silently using partial data; displayed file previews are capped at 256 KiB. The index guard fingerprints its complete stream even for large indexes.
- Do not automatically trust a repository from an unknown source: Git may execute configured hooks, filters, credential helpers, merge drivers, and SSH commands. Kae does not sandbox Git.

## Development

```sh
make              # Available commands
make build        # target/debug/kae
make release      # target/release/kae
make install      # Install locally through Cargo
make fmt          # Format Rust
make fmt-check    # Check formatting, without edits
make lint         # Clippy, warnings are errors
make test         # Unit + disposable-repository integration tests
make check        # Formatting, Clippy, all tests
```

Build/install/run/lint/test use `--locked`. Override Cargo with `make CARGO=/path/to/cargo build`. Integration tests use local temporary repositories and bare remotes; they do not push to real hosted repositories. Coverage includes staged/unstaged changes, literal/rename paths, hunk staging, stale-state guards, conflict choices and save/stage separation, binary/deletion conflicts, graph/stash/tag/worktree actions, pull strategies, non-forced push, every rebase action, and recovery. Rendering tests cover wide, narrow, and undersized terminals.
