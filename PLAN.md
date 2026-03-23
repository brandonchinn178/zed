# Sidebar thread grouping — worktree path canonicalization

## Problem

Threads in the sidebar are grouped by their `folder_paths` (a `PathList` stored
in the thread metadata database). When a thread is created from a git worktree
checkout (e.g. `/Users/eric/repo/worktrees/zed/lasalle-lceljoj7/zed`), its
`folder_paths` records the worktree path. But the sidebar computes workspace
groups from `visible_worktrees().abs_path()`, which returns the checkout path.
Threads from different checkouts of the same repos (different branches) have
different raw paths and don't match.

## What we've done

### 1. `PathList` equality fix (PR #52052 — merged)

**File:** `crates/util/src/path_list.rs`

Manual `PartialEq`/`Eq`/`Hash` impls that only compare the sorted `paths`
field, ignoring display order.

### 2. Worktree path canonicalization + historical groups (this branch)

**Files:** `crates/sidebar/src/sidebar.rs`, `crates/agent_ui/src/thread_metadata_store.rs`

#### Core changes:

- **`build_worktree_root_mapping()`** — iterates ALL repos from all workspaces
  (not just root repos) to build a `HashMap<PathBuf, Arc<Path>>` mapping every
  known worktree checkout path to its root repo path. Robust against snapshot
  timing where linked-worktree lists may be temporarily incomplete.

- **`canonicalize_path_list()`** — maps each path in a `PathList` through the
  worktree root mapping.

- **`rebuild_contents()` three-tier thread lookup:**
  1. **Raw lookup** (`entries_for_path`) — exact match by workspace's raw paths
  2. **Linked worktree loop** (canonical lookup per repo) — finds threads from
     absorbed worktree checkouts, assigns correct worktree chips
  3. **Canonical lookup** — catches threads from different checkouts of the same
     repos (e.g. thread saved in branch-a, workspace is branch-b)

- **Historical groups** — after the workspace loop, iterates all unclaimed
  threads (tracked via `claimed_session_ids`) and creates `Closed` project
  group sections. These appear at the bottom of the sidebar.

- **`ProjectHeader.workspace`** is now `Option<Entity<Workspace>>` to support
  closed historical group headers.

- **`find_current_workspace_for_path_list` / `find_open_workspace_for_path_list`**
  — canonicalize both sides (thread path and workspace path) before comparing.

- **`activate_archived_thread`** — when no matching workspace is found, saves
  metadata and sets `focused_thread` instead of opening a new workspace (which
  would get absorbed via `find_existing_workspace`).

- **`prune_stale_worktree_workspaces`** — doesn't prune a worktree workspace
  when its main repo workspace is still open (linked-worktree list may be
  temporarily incomplete during re-scans).

- **`thread_entry_from_metadata`** — extracted helper for building ThreadEntry
  from ThreadMetadata.

- **`SidebarThreadMetadataStore::all_entries()`** — new method returning
  `&[ThreadMetadata]` for reference-based iteration.

## Remaining issues

### Canonical lookup assigns threads to wrong workspace (next up)

When multiple workspaces share the same canonical path (e.g. main repo + worktree
checkout of the same repos), the canonical lookup assigns threads to whichever
workspace processes first in the loop. This causes threads to open in the wrong
workspace context.

**Fix needed:** Two-pass approach in `rebuild_contents`:
- **Pass 1:** Raw lookups across all workspaces (priority claims, correct
  workspace assignment)
- **Pass 2:** Canonical lookups only for threads not claimed in pass 1

### Click-to-open from Closed groups bypasses `find_existing_workspace`

When a user clicks a thread under a `Closed` historical group header,
`open_workspace_and_activate_thread` goes through `open_paths` →
`find_existing_workspace`, which routes to an existing workspace that contains
the path instead of creating a new workspace tab. Need to either:
- Pass `open_new_workspace: Some(true)` through the call chain
- Or use a direct workspace creation path

### Path set mutation (adding/removing folders)

When you add a folder to a project (e.g. adding `ex` to a `zed` workspace),
existing threads saved with `[zed]` don't match the new `[ex, zed]` path list.
This is a design decision still being discussed.

### Pre-existing test failure

`test_two_worktree_workspaces_absorbed_when_main_added` fails on `origin/main`
before our changes. Root cause is a git snapshot timing issue where linked
worktrees temporarily disappear during re-scans, causing the prune function
to remove workspaces prematurely.

## Key code locations

- **Thread metadata storage:** `crates/agent_ui/src/thread_metadata_store.rs`
  - `SidebarThreadMetadataStore` — in-memory cache + SQLite DB
  - `threads_by_paths: HashMap<PathList, Vec<ThreadMetadata>>` — index by literal paths
- **Sidebar rebuild:** `crates/sidebar/src/sidebar.rs`
  - `rebuild_contents()` — three-tier lookup + historical groups
  - `build_worktree_root_mapping()` — worktree→root path map
  - `canonicalize_path_list()` — maps a PathList through the root mapping
  - `thread_entry_from_metadata()` — helper for building ThreadEntry
- **Thread saving:** `crates/agent/src/agent.rs`
  - `NativeAgent::save_thread()` — snapshots `folder_paths` from visible worktrees
- **PathList:** `crates/util/src/path_list.rs`
  - Equality compares only sorted paths, not display order
- **Archive restore:** `crates/sidebar/src/sidebar.rs`
  - `activate_archived_thread()` — saves metadata + focuses thread (no workspace open)

## Useful debugging queries

```sql
-- All distinct folder_paths in the sidebar metadata store (nightly)
sqlite3 ~/Library/Application\ Support/Zed/db/0-nightly/db.sqlite \
  "SELECT folder_paths, COUNT(*) FROM sidebar_threads GROUP BY folder_paths ORDER BY COUNT(*) DESC"

-- Find a specific thread
sqlite3 ~/Library/Application\ Support/Zed/db/0-nightly/db.sqlite \
  "SELECT session_id, title, folder_paths FROM sidebar_threads WHERE title LIKE '%search term%'"
```
