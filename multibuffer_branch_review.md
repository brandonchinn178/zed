# Branch review findings

- Branch: `excerpt-id--`
- Merge base with `origin/main`: `da2bed1930d1e0b3bfaa7b27a96170543f9629c4`

## Scope notes

- Reviewing the branch by diffing against the merge base with `origin/main`.
- Focus areas: multibuffer/path_key invariants, removal of `ExcerptId`, `text::Anchor` and `multi_buffer::Anchor` semantics, downstream anchor handling, and performance regressions.

## Findings

### 1. High: `buffer_range_to_excerpt_ranges` can loop forever and over-expand highlight ranges

- **Where:** `crates/multi_buffer/src/multi_buffer.rs#L6662-L6690`
- **Why:** Inside the iterator closure, the cursor skips deleted-hunk regions with `cursor.next()`, but once it reaches a main-buffer region it returns `Some(multibuffer_range)` without ever advancing the cursor. The next poll sees the same region again, so the iterator can yield the same range forever.
- **Also wrong:** it computes `region.buffer.anchor_range_inside(region.buffer_range.clone())`, which lifts the entire visible region instead of intersecting with the requested input `range`.
- **Concrete downstream impact:** `crates/editor/src/editor.rs#L7483-L7491` iterates this function directly when painting document highlights. A highlight that lands in a shown region can therefore either hang the loop / allocate unboundedly, or paint the whole excerpt-visible slice instead of just the LSP-provided range.
- **Merge-base comparison:** this helper is new on the branch, so the regression is introduced here.

### 2. High: `anchor_range_in_buffer` no longer enforces the contract it documents, and several downstream regressions stem from that

- **Where:** `crates/multi_buffer/src/multi_buffer.rs#L5200-L5209`
- **Why:** The doc comment still says it should succeed only when “any excerpt contains both endpoints and there are no intervening deleted hunks”, but the implementation now only checks that both endpoints belong to the same buffer and that the buffer has a `PathKeyIndex`. It then blindly returns `Anchor::range_in_buffer(path_key_index, text_anchor)`.
- **Why this matters:** this turns `anchor_range_in_buffer` into a broad lifting API, not an excerpt-aware one. Several migrated callers now succeed for ranges that are only partially shown, span gaps between excerpts, or cross deleted-hunk boundaries.
- **Contrast with excerpt-aware API:** `buffer_anchor_range_to_anchor_range` right below it in `crates/multi_buffer/src/multi_buffer.rs#L5250-L5279` still performs the expected containment check by iterating excerpts and requiring both endpoints to be contained in the same excerpt.
- **Result:** this is the root cause behind multiple containment-vs-overlap regressions below.

### 3. Medium: diagnostics links stop working for partially visible diagnostics

- **Where:** `crates/diagnostics/src/diagnostic_renderer.rs#L286-L297`
- **Merge-base behavior:** the old code iterated `multibuffer.excerpts_for_buffer(buffer_id, cx)` and jumped when `range.context.overlaps(&diagnostic.range, &snapshot)`.
- **New behavior:** it now calls `multibuffer.snapshot(cx).anchor_range_in_buffer(diagnostic.range)` and jumps only if that returns `Some(...)`.
- **Why this is a regression:** because `anchor_range_in_buffer` is no longer excerpt-aware, this call no longer implements the old “jump when any shown excerpt overlaps the diagnostic” behavior. In practice, diagnostics that extend outside the shown excerpt can fail to jump to a visible location at all.

### 4. Medium: conflict highlighting and cleanup can disappear for conflicts split across excerpts of the same buffer

- **Where:** `crates/git_ui/src/conflict_view.rs#L173-L181` and `crates/git_ui/src/conflict_view.rs#L242-L252`
- **Merge-base behavior:** this code first found the containing excerpt, then used `anchor_range_in_excerpt(excerpt_id, ...)` for the conflict, `ours`, and `theirs` ranges.
- **New behavior:** it now directly uses `snapshot.anchor_range_in_buffer(...)` / `buffer.anchor_range_in_buffer(...)` for all of those ranges.
- **Why this is a regression:** when a conflict region or one of its subranges is split across multiple excerpts, the old code could still work relative to the specific excerpt it chose. The new code assumes whole-range lifting is valid buffer-wide, so highlight placement/removal can silently skip work or target ranges that are not excerpt-local.

### 5. Medium: `outline_panel` selection lookup is no longer excerpt-scoped, so the wrong outline can win when a buffer appears in multiple excerpts

- **Where:** `crates/outline_panel/src/outline_panel.rs#L3261-L3411`
- **Merge-base behavior:** `outline_location` took both `buffer_id` and `excerpt_id`, looked up the exact excerpt in `self.excerpts`, and mapped outline ranges with `anchor_range_in_excerpt(excerpt_id, ...)`.
- **New behavior:** it now takes only a `selection_anchor`, looks up outlines for the whole buffer via `self.buffers.get(&selection_anchor.buffer_id)`, and maps every outline with `multi_buffer_snapshot.anchor_range_in_buffer(outline.range.clone())`.
- **Why this is a regression:** if the same buffer is shown in multiple excerpts, symbols from a different excerpt of that buffer are now eligible for the current selection. The nearest-container heuristic can therefore attach the selection to the wrong symbol tree.

### 6. Medium: revealing an outline entry can uncollapse unrelated excerpts from the same buffer

- **Where:** `crates/outline_panel/src/outline_panel.rs#L2083-L2101`
- **Merge-base behavior:** collapsed excerpt state was keyed by `(buffer_id, excerpt_id)`, so revealing one outline removed only the matching collapsed excerpt entry.
- **New behavior:** collapsed state is keyed by `CollapsedEntry::Excerpt(excerpt_range)`, and reveal now removes every collapsed excerpt for the same buffer whose stored range contains either endpoint of the outline range.
- **Why this is a regression:** when the same buffer has multiple excerpts, revealing one outline can uncollapse additional excerpts for that buffer if they happen to contain the same outline endpoints or overlapping boundary anchors.

### 7. Medium: LSP folding ranges now disappear unless the full fold range fits in one excerpt

- **Where:** `crates/editor/src/display_map.rs#L958-L987`
- **Merge-base behavior:** `set_lsp_folding_ranges` enumerated excerpt ids for the target buffer and kept the first `snapshot.anchor_range_in_excerpt(id, folding_range.range.clone())` that succeeded.
- **New behavior:** it now does a single `snapshot.anchor_range_in_buffer(folding_range.range.clone())?`.
- **Why this is a regression:** in partial-excerpt editors, folds whose start/end span excerpt boundaries are now silently dropped even when part of the fold is visible and the old excerpt-aware code could still create a crease for the visible excerpt.

### 8. Medium: path-key metadata now grows monotonically across `clear()` and normal path churn

- **Where:** `crates/multi_buffer/src/path_key.rs#L283-L298`, `crates/multi_buffer/src/path_key.rs#L551-L608`, `crates/multi_buffer/src/multi_buffer.rs#L1731-L1762`
- **Why:** `get_or_create_path_key_index` only inserts into `path_keys_by_index` / `indices_by_path_key`. Neither `remove_excerpts` nor `clear()` removes entries from those maps.
- **Why this matters:** several clients on this branch reuse a long-lived multibuffer with `clear()` and then repopulate it, e.g. `crates/edit_prediction_ui/src/edit_prediction_context_view.rs#L200-L214` and `crates/acp_thread/src/diff.rs#L299-L309`. Each new path permanently grows the path-key intern tables.
- **Consequence:** this is a concrete memory/performance regression for reuse-heavy multibuffers, even if the retention was meant to help stale-anchor comparisons.

## Lower-confidence concerns

- `MultiBuffer::clone` rebuilds every diff with `DiffState::new(diff.diff.clone(), new_cx)` (`crates/multi_buffer/src/multi_buffer.rs#L1185-L1217`) even though inverted diffs are created with `DiffState::new_inverted(...)` and carry a `main_buffer` link (`crates/multi_buffer/src/multi_buffer.rs#L562-L600`, `crates/multi_buffer/src/multi_buffer.rs#L2209-L2223`). That looks like a real semantic mismatch for cloned multibuffers with inverted diffs, but I did not validate a concrete reachable caller that clones such a multibuffer on this branch.


