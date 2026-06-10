# Operations

`ji sync`, `ji transfer`, and `ji close` are the three commands that combine two workspaces. This page documents the mode detection, the concrete methods, and the exact jj commands each method runs.

- [Terminology](#terminology)
- [Sync mode detection](#sync-mode-detection)
- [Adaptive resolution](#adaptive-resolution)
- [`ji sync`](#ji-sync)
- [Transfer methods](#transfer-methods)
- [Close methods](#close-methods)
- [Bookmark actions on close](#bookmark-actions-on-close)
- [Post-operation behavior](#post-operation-behavior)

## Terminology

- **Default workspace** — the workspace at the repository root, addressable as `default@`. It serves the role of the `main`/`master` branch in git. `close`, `transfer`, and `sync` default their `target` argument to this workspace.
- **Source** / **target** — in `ji close`, `ji transfer`, and `ji sync`, the source is the workspace the operation is run *from* (the current working directory unless `--source` overrides), and the target is the workspace the operation is run *against* (`default@` unless a positional `target` is given). By convention, source is a feature workspace and target is `default@`.
- **Step-forward commit** — an empty commit ji inserts on a workspace's `@` to keep two workspaces from sharing the same working-copy revision. Description: `(ji::step-forward)`. Two workspaces must never have the same `@`, so ji adds these after any operation that would otherwise produce a shared `@`.
- **Trivial head** — a graph head that is empty (no file changes), single-parent (not a merge), and has a "trivial" description. A description counts as trivial if it is empty, is a single word with no whitespace, starts with `(ji::` (on a single line), or matches jj's configured `templates.new_description` exactly. Step-forward commits and `(ji::fast-forward)` commits are both trivial by this definition.
- **Effective head** — a workspace's line-of-work tip, with a trivial `@` skipped. If `@` is a trivial head, the effective head is `@-`; otherwise it is `@`. ji compares effective heads when deciding whether two workspaces are in sync.
- **Last common ancestor (LCA)** — `latest(fork_point(src_eff | tgt_eff))` — the most recent revision that is an ancestor of both effective heads.
- **Singular bookmark** — the bookmark whose name matches the workspace's `{{ bookmark }}` template value (derived from `workspace-path` and the workspace name). ji creates it on `ji new` and manages it automatically during close/transfer/sync.
- **Adaptive method** — `--method adaptive` (the default for `close` and `transfer`). ji inspects the [sync mode](#sync-mode-detection) and dispatches to a concrete method; see [adaptive resolution](#adaptive-resolution).
- **Disposal method** — a close method that does not require a target: `detach` and `abandon`. Both forget the source workspace; `abandon` additionally deletes its revisions.

In diagrams below, `src_eff` / `tgt_eff` are the effective heads, `src_at` / `tgt_at` are the actual `@`, `lca` is the last common ancestor of the two effective heads, and `{src}` / `{tgt}` are the workspace names. `[{ws}] jj ...` means the command runs with that workspace's directory as its working directory.

### A note on `--ignore-working-copy`

Every `jj` command ji runs as part of these operations — `jj new`, `jj squash`, `jj rebase`, `jj abandon`, `jj bookmark set`, `jj workspace forget`, `jj log` — passes `--ignore-working-copy`, so jj neither snapshots the working copy nor writes it out on each call. The only exception is an explicit `jj util snapshot` ji runs before `jj workspace forget` (to capture pending edits in the workspace being closed). The command sequences below omit the `--ignore-working-copy` flag for readability, but it is present on every non-snapshot call.

## Sync mode detection

Before running a close/transfer/sync operation, ji computes a `SyncModeInfo` by:

1. Resolving each workspace's effective head (one `jj log` call per workspace).
2. Finding the LCA of the two effective heads.
3. Comparing each effective head to the LCA.
4. Recording the current operation-log head for later validation.

The four sync modes:

| Mode | Source at LCA | Target at LCA | Meaning |
|---|---|---|---|
| `InSync` | yes | yes | Both effective heads are at the LCA — nothing to do |
| `SourceOnly` | no | yes | Only the source has new work |
| `TargetOnly` | yes | no | Only the target has new work |
| `Diverged` | no | no | Both have new work since they last agreed |

### Operation-log validation

The `SyncModeInfo` captures the repo's operation head at detection time. When the operation is later executed, ji compares the current op head to the captured one. If the op head has not changed, the cached head info is used as-is. If the op head *has* changed (another process modified the repo in the meantime), ji recomputes the head info and the sync mode — and if the sync mode has shifted to a different category (e.g. what was `Diverged` is now `SourceOnly`), ji aborts with `repo changed externally, please retry`. A change in head info without a change in sync mode is accepted silently.

## Adaptive resolution

**Adaptive transfer** (`ji transfer --method adaptive`):

| Sync mode | Concrete method |
|---|---|
| `SourceOnly` | `fast-forward-target` |
| `TargetOnly` | `fast-forward-source` |
| `Diverged` | `merge` |
| `InSync` | errors: `adaptive merge unavailable for current sync state` |

**Adaptive close** (`ji close --method adaptive`):

| Sync mode | Concrete method |
|---|---|
| `SourceOnly` | `fast-forward` |
| `Diverged` | `merge` |
| `TargetOnly` | errors: `adaptive close unavailable for current sync state` |
| `InSync` | errors: `adaptive close unavailable for current sync state` |

(`TargetOnly` is an error for close because the source has no new work to move into the target — there is nothing to close.)

## `ji sync`

```
ji sync [target] [--source <NAME>]
```

`ji sync` reconciles two workspaces; both remain open. It has no `--method` flag — the strategy is always picked from the [sync mode](#sync-mode-detection):

| Mode | Action |
|---|---|
| `InSync` | no-op, prints `(ji)::sync already in sync` |
| `SourceOnly` | fast-forward target to source's effective head, then step source forward |
| `TargetOnly` | fast-forward source to target's effective head, then step target forward |
| `Diverged` | create a merge commit in the source workspace with both effective heads as parents, then step both workspaces forward past the merge |

For the exact command sequences and diagrams, see the corresponding transfer methods ([fast-forward-target](#transfer-fast-forward-target), [fast-forward-source](#transfer-fast-forward-source), [merge](#transfer-merge)). `ji sync`'s fast-forward steps the *ahead* side forward after the behind side catches up, so neither workspace is left sitting on the other's `@`.

---

## Transfer methods

`ji transfer` reconciles two workspaces; both remain open. Adaptive picks a concrete method from the sync mode; you can also pass a concrete method explicitly.

### Transfer: merge

```
ji transfer [target] --method merge
```

Create a merge commit whose parents are the two effective heads, then step both workspaces forward past the merge.

**Available in:** any non-`InSync` mode.

**Commands:**

```bash
[{src}] jj new -m "(ji::merge) {src}@{src_eff} + {tgt}@{tgt_eff}" -- {src_eff} {tgt_eff}
[{src}] jj new -m "(ji::step-forward)"
[{tgt}] jj new -m "(ji::step-forward)" -- {merge_id}
jj abandon -- {src_trivial} {tgt_trivial}   # only the trivials that existed pre-op
```

**Diagram:**

```
| INITIAL |  t@: A → B → C
|  STATE  |  s@: ↳ X → Y → Z

                         @
t@:  A → B → C ----↘   ↗ Nt
       ↘             M
s@:      X → Y → Z ↗   ↘ Ns
                         @
```

`M` is a merge commit with parents `tgt_eff` (`C`) and `src_eff` (`Z`). `src@` lands at `Ns` and `tgt@` at `Nt` — both step-forward commits that are children of `M`. The chiasma shape shows the two chains converging into `M` and then fanning out to separate step-forward commits so neither workspace sits on the merge itself.

### Transfer: fast-forward-target

```
ji transfer [target] --method fast-forward-target
```

Fast-forward the target to the source's effective head. The source is stepped forward so that the two workspaces do not share `@`.

**Available in:** any non-`InSync` mode. Adaptive chooses it when the sync mode is `SourceOnly`. Forcing it in `TargetOnly` or `Diverged` leaves the target's previous chain as an anonymous branch (not forcibly removed — just no longer reachable from `tgt@`).

**Commands:**

```bash
[{tgt}] jj new -m "(ji::fast-forward) {tgt}@ to {src}@{src_eff}" -- {src_eff}
jj abandon -- {tgt_trivial}             # only if the target had a trivial head pre-op
[{src}] jj new -m "(ji::step-forward)"  # skipped if src@ is already a trivial head
```

**Diagram:**

```
| INITIAL |  t@: A → B → C
|  STATE  |  s@: ↳ X → Y → Z

                     @
t@:  A → X → Y → Z → Nt
s@:              ↳ Ns
                   @
```

`Nt` is the `(ji::fast-forward)` commit created by `operations::fast_forward` on top of `src_eff` (`Z`); target's `@` lands at `Nt`. `Ns` is the step-forward commit created by `step_head` on the source side, also a child of `Z`. `Nt` and `Ns` are siblings under `Z`. The target's old chain (`A → B → C`) is no longer reachable from `tgt@`.

### Transfer: fast-forward-source

```
ji transfer [target] --method fast-forward-source
```

Mirror of `fast-forward-target`: fast-forward the source to the target's effective head, then step the target forward. Adaptive chooses it when the sync mode is `TargetOnly`. Forcing it in `SourceOnly` or `Diverged` leaves the source's previous chain as an anonymous branch.

**Commands:**

```bash
[{src}] jj new -m "(ji::fast-forward) {src}@ to {tgt}@{tgt_eff}" -- {tgt_eff}
jj abandon -- {src_trivial}             # only if the source had a trivial head pre-op
[{tgt}] jj new -m "(ji::step-forward)"  # skipped if tgt@ is already a trivial head
```

**Diagram:** mirror of [`fast-forward-target`](#transfer-fast-forward-target) with `src` and `tgt` swapped.

### Transfer: merge-abandon-old

```
ji transfer [target] --method merge-abandon-old
```

An alternative merge implementation that uses the raw `@` heads of both workspaces as merge parents, then relies on jj's auto-rebase-on-abandon to clean up trivial heads after the fact. Structurally, the end state is equivalent to `merge` — both workspaces have fresh step-forward commits under a merge of the effective heads — but the intermediate steps and the final merge commit's direct parents differ.

**Available in:** any non-`InSync` mode. Retained as a fallback; prefer `merge` for new work.

**Commands:**

```bash
[{src}] jj new -m "(ji::merge) {tgt}@{tgt_eff} into {src}@{src_eff}" -- {src_at} {tgt_at}
[{src}] jj new -m "(ji::step-forward)" -- {merge_id}
[{tgt}] jj new -m "(ji::step-forward)" -- {merge_id}
jj abandon -- {src_trivial} {tgt_trivial}   # jj auto-rebases the merge
                                            # onto the trivials' parents
```

The final abandon triggers jj's auto-rebase: because the merge's direct parents included trivial heads (the pre-op `@`s of both workspaces), abandoning those trivial heads causes jj to rewrite the merge so its parents become the trivials' parents (`src_eff` and `tgt_eff`). The end graph is the same as [`merge`](#transfer-merge).

**Diagram:** same shape as [`merge`](#transfer-merge) after the auto-rebase.

### Transfer: rebase

```
ji transfer [target] --method rebase
```

Rebase the source's unique chain onto the target's effective head, producing linear history. Only the target is stepped forward; the source stays at the rebased position of its original effective head.

**Available in:** `SourceOnly` or `Diverged`. `InSync` and `TargetOnly` have no source chain to rebase.

**Commands:**

```bash
jj rebase --source "roots({lca}..{src_eff})" --onto {tgt_eff}
[{tgt}] jj new -m "(ji::step-forward)"   # skipped if tgt@ is already a trivial head
```

`jj rebase --source <revset>` rebases the given revisions *and their descendants*. `roots({lca}..{src_eff})` selects the first commits after the LCA on the source's path; rebasing them and their descendants moves the entire source chain onto `tgt_eff`.

**Diagram:**

```
| INITIAL |  t@: A → B → C
|  STATE  |  s@: ↳ X → Y → Z

                 @
t@:  A → B → C → N
s@:          ↳ X' → Y' → Z'
                         @
```

After: the source chain is replayed on top of `C` (`tgt_eff`). `src@` is at `Z'` (the rebased tip; change IDs `X`, `Y`, `Z` are preserved, commit IDs change — hence the primes). `tgt@` is `N`, a step-forward commit branching from `C` — a sibling of `X'`, *not* a descendant of `Z'`. The `↳` aligns under `C` to mark where the rebased chain attaches.

### Transfer: merge-squash

```
ji transfer [target] --method merge-squash
```

Squash the source's unique chain into a single commit (change ID of the first unique commit is preserved), then create a confluence merge between the squashed commit and the target's effective head, then step both workspaces forward.

**Available in:** `SourceOnly` or `Diverged`.

**Commands:**

```bash
# X = latest(roots({lca}..{src_eff})) — the first commit after the LCA on src's path.
jj squash --from "{lca}..{src_eff}" --into {X} --message "(ji::squash) ..."

[{src}] jj new -m "(ji::merge) {src}@{X} + {tgt}@{tgt_eff}" -- {X} {tgt_eff}
[{src}] jj new -m "(ji::step-forward)"
[{tgt}] jj new -m "(ji::step-forward)" -- {merge_id}
jj abandon -- {src_trivial} {tgt_trivial}    # only the trivials that existed pre-op
```

The squash uses `jj squash --into X` where `X = roots({lca}..{src_eff})` — the first commit after the LCA on the source's path. This preserves `X`'s change ID and collapses the rest of the source chain (`Y`, `Z`) into it. The squash message is a structured multi-section string that preserves each original revision's description under `### (ji::squash) revision N of M ...` headers.

**Diagram:**

```
| INITIAL |  t@: A → B → C
|  STATE  |  s@: ↳ X → Y → Z

                     @
t@:  A → B → C ↘   ↗ Nt
       ↘         M
s@:      X* ---↗ ↳ Ns
                   @
```

`X*` has change ID `X` (preserved) and contains the combined content of `X`, `Y`, `Z`. `M` is a merge commit with parents `X*` and `tgt_eff` (`C`); both `tgt@` and `src@` step forward to siblings under `M` (`Nt` and `Ns` respectively). The `↘` from `A` shows where the original source chain branched off; the `---↗` from `X*` and `↘` from `C` meet at `M`.

### Transfer: adaptive

```
ji transfer [target] --method adaptive     # default
```

See [adaptive resolution](#adaptive-resolution). Resolves to `fast-forward-target`, `fast-forward-source`, or `merge` based on the sync mode.

---

## Close methods

`ji close` merges the source's work into the target and then forgets the source workspace.

### Close: merge

```
ji close [target] --method merge
```

Forget the source, then create a merge commit in the target workspace whose parents are the two effective heads. Unlike `transfer --method merge`, there is no step-forward after the merge — the target's `@` is exactly the merge commit.

**Available in:** any non-`InSync` mode.

**Commands:**

```bash
[{src}] jj util snapshot                                  # capture pending edits
jj workspace forget -- {src_name}
[{tgt}] jj new -m "(ji::merge) {src}@{src_eff} + {tgt}@{tgt_eff}" -- {src_eff} {tgt_eff}
jj abandon -- {src_trivial} {tgt_trivial}                 # only the trivials that existed pre-op
```

The `jj util snapshot` (run without `--ignore-working-copy`) captures any uncommitted edits in the source workspace before it is forgotten, so you don't lose pending work. Every other `jj` call in this sequence passes `--ignore-working-copy`.

**Diagram:**

```
| INITIAL |  t@: A → B → C
|  STATE  |  s@: ↳ X → Y → Z

                   @
t@:  A → B → C  →  M
     ↳ X → Y → Z ↗
```

`M` has parents `tgt_eff` (`C`) and `src_eff` (`Z`). `tgt@` is exactly `M` — no step-forward commit after. The source's chain (`X → Y → Z`) still lives in the graph; the `↗` from `Z` points up into `M`, showing that `Z` is `M`'s second parent. The source workspace is forgotten but its revisions remain.

### Close: squash-merge

```
ji close [target] --method squash-merge
```

Forget the source, squash its unique chain, then merge the squashed commit with the target. The target's `@` is the merge commit.

**Available in:** `SourceOnly` or `Diverged`.

**Commands:**

```bash
[{src}] jj util snapshot
jj workspace forget -- {src_name}
jj squash --from "{lca}..{src_eff}" --into {X} --message "(ji::squash) ..."
[{tgt}] jj new -m "(ji::merge) {src}@{X} + {tgt}@{tgt_eff}" -- {X} {tgt_eff}
jj abandon -- {src_trivial} {tgt_trivial}   # only the trivials that existed pre-op
```

**Diagram:**

```
| INITIAL |  t@: A → B → C
|  STATE  |  s@: ↳ X → Y → Z

                 @
t@:  A → B → C → M
     ↳ X* -----↗
```

`X*` has change ID `X` (preserved) and contains the combined content of `X`, `Y`, `Z`. `M` has parents `X*` and `tgt_eff` (`C`); `tgt@` is exactly `M`. The `-----↗` traces from `X*` rightward and up into `M`, indicating that `X*` is `M`'s second parent. The source workspace is forgotten.

### Close: fast-forward

```
ji close [target] --method fast-forward
```

Forget the source, then fast-forward the target to the source's effective head.

**Available in:** `SourceOnly` is the well-defined case — the source has work and the target is at the LCA. Forcing this method in other modes leaves the target's previous chain as an anonymous branch (not forcibly removed — just no longer reachable from `tgt@`).

**Commands:**

```bash
[{src}] jj util snapshot
jj workspace forget -- {src_name}
[{tgt}] jj edit {src_eff}
jj abandon -- {tgt_trivial}                 # only if the target had a trivial head pre-op
```

Unlike `transfer --method fast-forward-target`, there is no new `(ji::fast-forward)` commit. Because the source is being forgotten, `tgt@` can land directly on `src_eff` with `jj edit` — no need for a fresh leaf to keep the two workspaces off the same `@`.

**Diagram (typical case: target at LCA):**

```
| INITIAL |  t@: A
|  STATE  |  s@: ↳ X → Y → Z

                 @
t@:  A → X → Y → Z
```

`tgt@` is exactly `Z` (= `src_eff`) after the operation — `jj edit` reassigns the working-copy commit without creating a new revision. The target's previous `@` (at `A`) is abandoned if it was a trivial head. The source workspace is forgotten.

### Close: detach

```
ji close --method detach
```

Forget the source workspace without touching its revisions. The source chain remains in the graph as an anonymous line of work (or anchored to any bookmarks that were on it).

**Available in:** any sync mode. No target is required.

**Commands:**

```bash
[{src}] jj util snapshot
jj workspace forget -- {src_name}
```

**Diagram:**

```
| INITIAL |  t@: A → B → C
|  STATE  |  s@: ↳ X → Y → Z

            @
t@: A → B → C    (unchanged)
s@: ↳ X → Y → Z
              @
```

The source revisions (`X → Y → Z`) still live in the graph as an anonymous branch off `A`. Use `detach` when you want to keep the revisions reachable (e.g., a bookmark pins them, or you want to revisit them later) but no longer want a workspace sitting on them. Note: the source workspace is forgotten, so the `s@` row shows only the surviving revisions — the `@` marker below `Z` marks where the last `@` sat before the forget, not a live workspace head.

### Close: abandon

```
ji close --method abandon
```

Forget the source workspace *and* abandon every revision in its chain. Use this to throw away work entirely.

**Available in:** any sync mode. No target is required.

**Commands:**

```bash
[{src}] jj util snapshot
jj workspace forget -- {src_name}
jj abandon -- {rev1} {rev2} ... {revN}   # every revision in the source workspace's chain
```

The abandon list is the revision chain ji already fetched for the workspace (the same list displayed in the TUI workspace info panel). ji caps the abandon call at 50 revisions as a safety measure (`MAX_DESTRUCTIVE_REVISIONS`).

**Diagram:**

```
| INITIAL |  t@: A → B → C
|  STATE  |  s@: ↳ X → Y → Z

            @
t@: A → B → C    (unchanged)
```

### Close: adaptive

```
ji close [target] --method adaptive     # default
```

See [adaptive resolution](#adaptive-resolution). Resolves to `fast-forward` or `merge` based on the sync mode. `InSync` and `TargetOnly` are unavailable.

---

## Bookmark actions on close

A workspace usually carries one or more bookmarks. ji splits them into two groups:

- **Singular bookmark** — the bookmark whose name matches the workspace's `{{ bookmark }}` template value. ji manages it automatically during close, transfer, and sync; see the table below.
- **Non-singular bookmarks** — every other bookmark on the workspace.

### Singular bookmark, automatic behavior

| Operation | Source singular bookmark | Target singular bookmark |
|---|---|---|
| `ji sync` (Done) | advance to source's `@` | advance to target's `@` |
| `ji transfer` (any method, Done) | advance to source's `@` | advance to target's `@` |
| `ji close --method merge` | advance to source's effective head | advance to target's `@` |
| `ji close --method squash-merge` | advance to source's effective head | advance to target's `@` |
| `ji close --method fast-forward` | advance to source's effective head | advance to target's `@` |
| `ji close --method detach` | advance to source's effective head | not touched |
| `ji close --method abandon` | **deleted** | not touched |

### Non-singular bookmarks

The **TUI** close dialog exposes a per-close bookmark action for non-singular bookmarks (cycled with `b`):

| Action | Effect |
|---|---|
| `NoAction` (default) | Leave the bookmarks in place |
| `Advance` | Move every affected bookmark to the target workspace's change ID |
| `Delete` | Delete every affected bookmark |

The **CLI** (`ji close`) always uses `NoAction` for non-singular bookmarks — there is no CLI flag to choose `Advance` or `Delete`. If you want those actions on non-singular bookmarks as part of a CLI close, run the equivalent `jj bookmark set` / `jj bookmark delete` commands after `ji close` returns.

---

## Post-operation behavior

### Staleness resolution

Because ji runs most jj calls with `--ignore-working-copy` (see [the note above](#a-note-on---ignore-working-copy)), workspaces in the repository can end up stale when ji rewrites shared state without snapshotting them. Each operation does two things about this:

1. **Update the workspaces it directly touched.** Transfer and sync call `jj workspace update-stale` on both source and target after the operation. Close variants that keep the target (`merge`, `squash-merge`, `fast-forward`) call it on the target only. Disposal variants (`detach`, `abandon`) don't call it — the source is being forgotten, and the target is unchanged.
2. **Report workspaces that remain stale.** ji snapshots which workspaces were stale *before* the operation, then re-checks all workspaces *after*. Any workspace still stale at the end is reported:
   - `(was already stale)` if it was stale before the operation started.
   - `(unexpected)` if it became stale as a side effect of this operation.

   Remaining stale workspaces can be fixed manually with `jj workspace update-stale` run from that workspace's directory.

### Delete files (close only)

`ji close --delete-files` (CLI) or the `k` toggle in the TUI close dialog triggers `rm -rf` on the source workspace directory *after* the jj-level close succeeds. In the TUI this is gated behind an additional confirmation prompt; in the CLI it runs unconditionally. If the deleted directory was the current working directory, ji writes a `cd <repo_root>` directive to the [shell wrapper](shell-integration.md) so the parent shell leaves the deleted directory.

### Author override

When `ji-author` is set in `.config/ji.toml`, every new revision ji creates in these operations is immediately rewritten with `jj metaedit --author` to use the configured author. See [`ji-author` in configuration.md](configuration.md#ji-author--string-optional). This keeps ji's helper revisions (step-forward, fast-forward markers, merge commits, squash commits) separate from your personal author identity in `jj log --filter 'author(me)'`.
