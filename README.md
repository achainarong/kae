# kae

**แก้ — fix, resolve.** A Rust Git TUI with a dedicated merge-resolution workspace.

**Status: MVP / alpha.** Real Git operations, not placeholder screens. Try it on a disposable repository first; this is not a production-hardened replacement for every Git command.

## Install and run

Requires Git 2.38+ and an interactive terminal. Published binaries target **macOS ARM64**, **Linux AMD64**, and **Linux ARM64**. Linux builds use static musl binaries rather than depending on a particular glibc version. Windows and Intel macOS are not supported.

### Quick install (prebuilt release)

Once the first tagged release has been published, no Rust compiler is needed:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/achainarong/kae/main/scripts/install.sh -o install-kae.sh
sh install-kae.sh
export PATH="$HOME/.local/bin:$PATH"
kae -C /path/to/repository
```

The saved script can be inspected before running. It detects your platform, downloads the latest stable release, verifies SHA-256, and installs into `~/.local/bin` without sudo. **Replacing an existing binary always requires terminal confirmation.** Symlink destinations are refused. `KAE_NONINTERACTIVE=1` forbids replacements rather than bypassing confirmation. A checksum proves download integrity, not an independent publisher signature; macOS releases are not notarized.

To pin a release or select another directory:

```sh
KAE_INSTALL_DIR="$HOME/.local/bin" sh install-kae.sh v0.3.1
```

### Build from source

Requires Rust 1.88+ (CI uses 1.96.0). Make is optional.

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
| `1` | Files | Expandable folder tree, staged/unstaged diffs, file/folder/hunk staging, commit, amend, external editor, confirmed discard |
| `2` | History | Compact commit list, optional graph, current/all-branch scope, expandable file tree, searchable unified/split diffs and full contents, compare, cherry-pick, revert, interactive rebase |
| `3` | Branches | Create, switch, rename, safe deletion, compare, merge preview, merge |
| `4` | Remotes | Expandable remote/branch tree, cached branch comparisons, fetch, explicit pull strategy, non-forced push, upstream setup |
| `5` | Stashes | Save including untracked files, inspect, apply, pop, drop |
| `6` | Tags | Inspect, create a lightweight tag at HEAD, delete a local tag |
| `7` | Worktrees | List, add a detached worktree, open, remove a clean inactive worktree |

Press **`:`** for searchable, context-aware commands. **`/` never opens commands:** with a list/tree focused it filters that view; with a preview focused it searches the loaded text. Some operations are intentionally available only through command search to keep the default shortcuts small.

## Keyboard navigation

| Key | Action |
| --- | --- |
| `1`–`7`, `[` / `]` | Select workspace, previous/next workspace |
| `j` / `k`, arrows | Move selection or scroll focused preview |
| `Tab` | Switch list/preview focus |
| `h` / `l`, left/right | Scroll horizontally |
| `PageUp` / `PageDown`, `Home` | Scroll preview, return to the top |
| `s` / `u`, `S` / `U` | Stage/unstage selected file or folder, or all files |
| `Space` in the Files tree | Immediately stage remaining unstaged changes; otherwise unstage |
| `w` in Files | Show/hide the selected full path (also available on mouse hover) |
| `T` in Files / commit browser | Toggle the file tree (hidden by default for one changed file) |
| `Ctrl+Left` / `Ctrl+Right` | Resize side-by-side panes |
| `Ctrl+Up` / `Ctrl+Down` | Resize stacked panes |
| `g` / `B` in History | Toggle graph / current-branch versus all-branch history |
| `v` | Open hunk staging for the selected file |
| `c` / `a` | Commit staged changes / amend HEAD |
| `e` | Open selected file in `$VISUAL`, `$EDITOR`, or `vi` |
| `b` | Create and switch to a branch |
| `m` / `M` | Preview / perform merge from selected revision |
| `i` | Edit rebase sequence after the selected base commit |
| `f` / `p` / `P` | Fetch / pull fast-forward only / push |
| `C` / `A` | Review and confirm continue / abort of current operation |
| `r` | Refresh now; automatically refreshed every two seconds outside dialogs |
| `~` / `!` | Last operation/error details |
| `?` | Help; `j`/`k` scroll it |
| `Esc`, `q` | Cancel/close overlay; `q` quits from the main view |

In prompts: type, Backspace, **Ctrl-U** to clear, Enter to accept, Esc to cancel. Destructive actions use a centered confirmation dialog over the existing workspace: only **typed `y`** proceeds; `n` or Esc cancels. Pasted text never confirms or executes actions. Staging/unstaging files, folders, all files, and hunks is immediate, without confirmation. A `q` typed into a prompt or inline editor is input, not quit.

History starts with one row per commit reachable from HEAD (the current branch, or detached HEAD). Press `g` for the full graph and `B` to include all local branches, remote branches, and tags. Both choices are retained during refreshes for the current session. An unborn branch has no current-branch history; `B` can reveal other branches.

Click a file, commit, folder name, remote, or branch to select it and update its preview. Click the **arrow** beside a folder/remote to expand/collapse it; clicking its name does not change expansion. Enter toggles folders; Right expands/enters and Left collapses/moves to the parent. Workspace tabs are clickable too. Click the preview to focus it; wheel scrolling targets the pane under the pointer. Clicking never stages files or executes destructive actions. Mouse input is blocked by confirmation/input dialogs.

List filters are literal and case-insensitive. Type or paste after `/`, Enter keeps the filter without executing anything, and Esc clears it. The active query and match count remain visible. Trees retain matching ancestors and reveal matches inside collapsed folders; clearing the filter restores the previous expansion state. Filters and expansion survive refreshes for the current session. Main-view preview search highlights matches and uses `n` / `N` to move between them; Esc clears it. Search/filter input treats pasted shortcuts as text, never actions.

Drag the border between panes to resize them, or use Ctrl + arrow keys. Files, History, and the commit file browser remember separate widths and stacked heights as percentages in `config.toml` under `[panes]` (`files_width`, `history_width`, `commit_width`, and matching `_height` keys). Drag release saves immediately; keyboard resizing saves after a short pause or on normal exit. Other config settings and comments are preserved. Sizes are clamped for usable panes and adapt to a different terminal size next session; a save failure keeps the new layout for this session and reports the error. Terminals that reserve Ctrl + arrows can use dragging instead.

Wide terminals use side-by-side panes; narrower terminals stack them. Main navigation needs 44 × 12; conflict, hunk, rebase, and confirmation screens need 60 × 24. Editing/actions pause when the terminal is too small. Recommended: 120 × 36 or larger.

## File explorer

The **Files** workspace starts with the detected **Git repository root** (for example, `yoda-3/`), even when Kae is launched from a subdirectory. Changed files are grouped underneath it into expandable folders. Shared single-child folder chains are compacted (for example, `apps/portal/src`), but never merged into the project-root row. The root remains visible when the working tree is clean. File rows show Git's two-column status: **index first, working tree second**; `??` is untracked. Folder rows show only their name; fully staged folders have a soft green indicator.

When there is only one changed file, Files opens its diff directly with a wrapped full path instead of spending space on folders. The commit file browser does the same for single-file commits; the main History commit list is not hidden. Press `T` to reveal the tree, or use **Toggle file tree** in Files command search. File staging shortcuts, including Space, still work with the tree hidden. Multiple changed files show the tree by default; a clean working tree retains its root row.

- `j` / `k` moves through the tree. Enter toggles a folder or opens a file's diff; conflicts open the resolver.
- Selecting the repository root previews all changed files; selecting a folder previews all its changed descendants with filename headings and separate staged/working sections. Selecting a file previews only that file. Parent previews include all descendants even when a filter hides some rows. Combined previews load asynchronously, retain diff colors, and stop at 256 KiB with a truncation notice; select an individual file to inspect more.
- Right / `l` expands or enters a folder; Left / `h` collapses it or moves to the nearest displayed parent. `Tab` switches tree/diff focus; in the diff, `h` / `l` scroll horizontally.
- `s` on a folder immediately stages its changed files; `u` immediately unstages them. Rename sources are included when needed. Working files are retained; staged-only versions can be replaced or removed without a prompt.
- **Space toggles staging** for the selected file, folder, or project root. If the selection has unstaged changes (including partial staging), Space stages them; otherwise it unstages them. Space is not a staging shortcut in previews, History, prompts, or pasted input.
- The root row supports the same expand/collapse and immediate staging/unstaging controls, scoped to all changed files in that repository. It is a navigation row, not a file path passed to Git.
- Bulk folder staging refuses unresolved conflicts: resolve and stage those files individually. Editing, hunk operations, discard, and conflict choices only work on file rows, never on an invisible previously selected file.
- Folder expansion and selection are retained across refreshes. The selected path appears above the diff; long tree labels retain the filename end rather than repeating an unreadable path prefix. Hover over a tree row to see its full wrapped path without changing selection, or press `w` for the selected row. Another key or Esc dismisses it. Mouse hover requires a terminal with mouse-motion reporting; `w` works without it.
- Staged filenames use the theme's green; unresolved conflicts use its red. Unstaged/untracked filenames stay neutral. Partially staged files retain separate index/worktree status markers, so a green filename does not imply all its changes are staged.
- Staging/unstaging updates the filename color and folder indicators immediately, with a `·` pending marker in the index column. With the tree hidden, the path heading shows the pending operation. This is a visual prediction only: the confirmed Git snapshot and safety guard are never edited. Once Git and status refresh finish, actual status replaces the prediction; failures clear the prediction and show an error/output. Kae does not run inverse Git commands to roll back a failed prediction. Further mutations remain locked while Git runs.
- Added patch lines and `+` bars in commit diffstats use the theme's green; removed lines and `-` bars use its red. Context and binary diffstat metadata stay neutral.
- While Git runs, tree/workspace navigation, scrolling, previews, and help remain available. Further mutations are locked, **not queued**, until the operation and refreshed status complete. Quit waits safely for the running Git command.

After an index-only operation, Kae refreshes file status and the full safety guard but reuses history/branch metadata when HEAD, refs, operation, and upstream configuration are unchanged. External metadata changes trigger a full reload instead; manual and periodic refreshes still load the complete snapshot. Stale-state and changed-file-content checks are not skipped for speed.

This is a **changed-files explorer**, not a filesystem editor: unchanged/ignored files and empty folders are not listed. It does not create files or folders.

## Remote browser

Each configured remote is a separate expandable root (`origin`, `upstream`, etc.); Git remote names are unique. Remote branch names containing `/` appear under branch folders. Remotes with no cached branches remain visible, and symbolic aliases such as `origin/HEAD` are omitted from branch rows.

Selecting a remote/group shows its cached branches and local tracking summary, **not local file changes**. Selecting a branch shows recent commits and its comparison with the current HEAD. Browsing only reads local remote-tracking refs; it never fetches or connects automatically, and URLs are omitted to avoid exposing credentials. Use `f` to review a fetch for the selected remote. Pull/push on a selected remote branch name that exact source/destination in their confirmation; selecting a group uses the owning remote and the current upstream/local branch fallback. Fetch does not prune, and push never forces.

## Appearance and themes

Kae defaults to **Catppuccin Mocha** with soft diff colors, neutral metadata, lavender focus, and your terminal's background and opacity. Other bundled themes are **Catppuccin Macchiato**, **Catppuccin Latte** (light), **Nord**, and **terminal** (your terminal's ANSI palette).

From the main view, press `:` and choose **Change theme**. Up/down or `j`/`k` previews immediately, Enter saves, and Esc cancels without writing. Invalid themes show an error and cannot be saved. Browsing History remains Git-read-only.

On interactive startup, Kae creates missing configuration/templates under `$XDG_CONFIG_HOME/kae`, or **`~/.config/kae`** when XDG_CONFIG_HOME is unset. Existing files are never replaced by initialization:

```text
~/.config/kae/
├── config.toml
└── themes/
    ├── catppuccin-mocha.toml
    ├── catppuccin-macchiato.toml
    ├── catppuccin-latte.toml
    ├── nord.toml
    └── terminal.toml
```

```toml
# config.toml
theme = "catppuccin-mocha"
transparent = true # default: inherit the terminal background and opacity
```

Set `transparent = false` to opt into a solid theme background. Existing explicit settings are preserved; if an earlier version created `transparent = false`, change it to `true` and restart to inherit terminal opacity. Selection and search highlights still use their theme colors.

Copy a template to `themes/my-theme.toml`, edit it, then select **my-theme** in the picker. For example:

```toml
base = "catppuccin-mocha"
accent = "#b4befe"
added = "#a6e3a1"
deleted = "#f38ba8"
```

Available color keys are `background`, `text`, `muted`, `border`, `accent`, `selection`, `added`, `deleted`, `warning`, `hunk`, `search`, `search_active`, and `search_text`. Colors accept `#RRGGBB` or `terminal`. Base names refer to bundled palettes; custom theme names allow letters, digits, hyphens, and underscores. Files named after a built-in override that theme's colors. Changes are read when opening/previewing the picker or restarting Kae.

Invalid startup configuration falls back to Mocha with a visible error. Accepted theme changes are saved atomically, preserving other settings and comments; malformed config files are not overwritten. No repository configuration is changed.

## Browse a commit by file

In **History**, select a commit and press **Enter** to open its changed files as a folder tree rather than navigating one combined patch.

- In the tree, `j` / `k` moves between folders/files. Enter toggles a folder or focuses the selected file's diff. Right/`l` expands a folder; Left/`h` collapses it or moves to its parent.
- The `●` marks the active file; collapsing its folder does not change the preview or lose its position.
- `Tab` switches tree/diff focus. **`[` / `]` selects the previous/next file without leaving the diff**, revealing collapsed ancestors as needed. It stops at the first/last file rather than wrapping.
- In the diff, `j` / `k`, PageUp/PageDown, Home/End, and `h` / `l` scroll. **`{` / `}` jumps between hunks** (replacing the old `n` / `p` shortcuts in this view).
- **`d` toggles unified and side-by-side diffs.** Split view aligns old/new lines and shows their actual line numbers, with blank cells for missing lines. A terminal narrower than 120 columns falls back to unified without changing the preference. Binary, metadata-only, or incomplete/truncated patches also stay unified; Kae does not invent missing content.
- **`/` filters with the file tree focused, or searches the current diff/full file with the preview focused.** For text search, Enter finds literal, case-insensitive text and focuses the preview; `n` / `N` moves forward/backward with wraparound. Matches are highlighted, the current one has a distinct color, and the search status shows its position/count. Search includes loaded patch metadata, not line-number gutters. Unicode and regex punctuation are treated literally.
- While entering a query, Backspace edits, Ctrl-U clears, and Esc cancels without changing the existing search or scroll position. An empty accepted query clears highlights. Pasted shortcuts are text, never actions. Queries are limited to 1,024 bytes and search only the loaded preview, not undisplayed portions of a large file or other files.
- `t` toggles the diff and full historical file with line numbers. Search and layout preferences persist between files/modes; scroll positions are remembered separately per file and mode while the commit is open. An active search jumps to a match in the newly loaded content.
- Merge commits initially compare against the first parent; `P` cycles parents, retaining the active path when present, folder state, focus, layout, full-file mode, and query. Positions reset because the comparison may have changed.
- Root commits compare against the empty tree. Deleted files show their parent version in full-file mode; renames include both paths.
- Esc / `q` returns to history, retaining the selected commit.

Historical browsing is read-only: it never checks out a file, changes the index, or replaces working content. Large file previews are capped at 256 KiB. Diff alignment is prepared by the background loader and only visible rows are rendered, including on long Unicode lines.

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
   | `s` | Review/confirm saving over the working file, **without staging** |
   | `a` | Explicitly stage a fully resolved, saved result |
   | `e` | Edit the result directly inside Kae |
   | `E` | Open the saved file in your external editor |

   Current/incoming panes show word differences; the result shows the complete combined file. Missing final newlines and CRLF text are preserved. In the inline editor, arrows/Home/End move, Ctrl-Z/Ctrl-Y undo/redo, **Esc keeps the draft without writing**, and **Ctrl-S opens a save confirmation**. It does not stage or continue Git. Mixed line endings or terminal control characters require the external editor to avoid unintended conversion. Unsaved drafts require confirmation before being discarded; background results never replace them. Conflict-level undo also restores prior manual edits (up to 16 manual-edit snapshots).

6. Repeat for other files. Use `:` → **Review full integration diff** to inspect automatically merged changes too.
7. Press `C` and confirm to continue. Press `A` for a confirmed abort instead.

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
- Destructive actions require explicit confirmation: discard/whole-file replacement, saves over working files, branch rename/deletion, stash save/pop/drop, tag/worktree deletion, history rewriting, and operation continue/skip/abort. Merge/fetch/pull/push also have review dialogs; direct fetch disables pruning. Staging/unstaging, including hunks and replacing staged-only content, is intentionally immediate and retains working files. Unmerged files cannot be staged while standard conflict markers remain. Undo covers in-memory conflict choices/edits, **not** discard, commits, or other Git operations.
- Actions recheck HEAD/branch identity, refs, index, changed-file contents, and remote/upstream configuration. Stale views fail rather than applying an action to an unexpected state. Avoid concurrently editing a file while saving its resolution; these are optimistic checks, not a repository-wide lock.
- Push never uses force, does not implicitly push tags/submodules, and names its destination explicitly. A selected remote branch takes precedence; otherwise it follows the current upstream's branch on that remote when configured, then the current branch name. Command search includes **Push and set upstream**.
- The Remotes workspace selects a remote explicitly. Elsewhere Kae uses the upstream remote, then `origin`, then the first configured remote; pull/push review shows the choice. Pull defaults to fast-forward-only; merge/rebase strategies are separate commands. Other branches are not automatically updated by rebase.
- Configure credentials, SSH/GPG agents, and remotes using Git externally. Git terminal credential prompts are disabled; configured hooks/helpers still run and may block. URLs are omitted from remote details, and common URL credentials/query tokens are redacted from operation logs. Arbitrary hook output is not guaranteed secret-free.
- Repository discovery, previews, file lists, refreshes, and plan loading run in a coalescing read worker. Old navigation results are ignored. Automatic refresh preserves selection/scroll and is deferred during dialogs/drafts; mutations invalidate pending read results. Git mutations use a separate worker. During an operation, `q` / Ctrl-C requests a quit after completion. Termination signals follow the same orderly shutdown; Kae does not kill a possibly writing Git process.
- History and rebase plans are limited to 300 commits. No graph pagination, root-commit rebase, merge-preserving rebase, remote configuration editor, remote branch deletion, or submodule-management UI yet.
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

## CI and tagged releases

`.github/workflows/ci.yml` runs formatting, Clippy, tests, release builds, installer checks, and CLI smoke tests on native macOS ARM64, Linux AMD64, and Linux ARM64 runners for pull requests and pushes to `main`.

`.github/workflows/release.yml` runs that CI first, then tests/builds the actual targets and publishes a GitHub Release when a **`v*` tag** is pushed:

| Platform | Asset |
| --- | --- |
| macOS ARM64 (11+) | `kae-aarch64-apple-darwin.tar.gz` |
| Linux AMD64 | `kae-x86_64-unknown-linux-musl.tar.gz` |
| Linux ARM64 | `kae-aarch64-unknown-linux-musl.tar.gz` |

Each archive contains `kae` and has a matching `.sha256`; releases also include `SHA256SUMS`. A tag must exactly match `v` plus `Cargo.toml`'s version. Tags containing a prerelease suffix create prereleases rather than replacing the latest stable release. Uploads complete in a draft before the release is published, so the installer cannot see a half-uploaded release. Existing releases/assets are never silently overwritten; retrying publication for an existing release fails safely.

For a maintainer-triggered release, first review/commit/push the intended changes and set the Cargo version. For the current version, the release commands are:

```sh
git tag -a v0.3.1 -m "Kae 0.3.1"
git push origin v0.3.1
```

Only tag reviewed code. The workflow files must already exist in that tagged commit, and GitHub Actions must be enabled. These commands are instructions, not actions performed by Kae. Hosted CI/release execution requires pushing the workflow; local validation cannot substitute for the first three-platform run.
