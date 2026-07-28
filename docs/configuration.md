# Configuration

ji reads per-project configuration from `.config/ji.toml` at the repository root. The file is intended to be checked into version control so everyone working on the repo shares the same workspace paths, hooks, and templates.

## Generating

```sh
ji init                # generate .config/ji.toml
ji config init         # same
```

The generated file is the annotated template at [`src/ji.template.toml`](../src/ji.template.toml) with every field commented out. Uncomment and fill in what you need.

`ji init` refuses to overwrite an existing file.

## Fields

### `repo` — string, optional

Overrides the `{{ repo }}` template variable. If unset or empty, ji uses the repository directory name (the last path component of the repo root).

```toml
repo = "myproject"
```

### `workspace-path` — string

Template for the directory where `ji new` creates a new workspace. Default:

```toml
workspace-path = "../{{ repo }}.{{ bookmark }}"
```

The path is resolved relative to the repository root and expanded with template variables (see below). Forward slashes in the `{{ bookmark }}` value are replaced with hyphens before substitution. With the default template, a workspace for bookmark `feature/login` in repo `myproject` lands at `../myproject.feature-login`, next to the default workspace (`default@`) checkout.

### `log-template` — string, optional

A [jj template](https://docs.jj-vcs.dev/latest/templates/) passed to `jj log --template` for the TUI's graph pane. If unset, ji uses the default jj template.

Guidelines:

- Every revision in the graph pane must render to the same number of lines. ji uses a fixed line count per revision to line up its own inline annotations (short change IDs, workspace markers) against the rendered graph.
- Put a constant number of `\n` characters at the top level of the template. Avoid `if(...)` blocks that emit conditional newlines — those change the per-revision line count and misalign the annotations.

Example:

```toml
log-template = '''
concat(
  separate(" ",
    change_id.shortest(),
    if(bookmarks, label("rest", "[ " ++ bookmarks ++ " ]"), ""),
    if(empty, label("empty", "(empty)"), ""),
    if(conflict, label("conflict", "CONFLICT"), "")
  ),
  " > ",
  if(description == "", label("description placeholder", "(no desc)"), description.first_line()),
  "\n"
)
'''
```

### `ji-author` — string, optional

Overrides the author on every revision ji creates (via `jj metaedit --author` after the revision is made). Format: `"Name <email>"`.

```toml
ji-author = "Jujutsu Gi <ji@null.com>"
```

This isolates ji-generated helper revisions — such as the empty "step-forward" commits ji inserts to keep workspaces from sharing `@`, or the merge/fast-forward markers from close and transfer operations — from your personal author identity, so that `jj log --filter 'author(me)'` stays focused on your work. See [operations.md](operations.md#terminology) for what these helper revisions look like.

### `preserve-finder-xattrs` — bool, default `true`

jj stores only file content and mode; whenever it materializes working-copy files it strips macOS Finder metadata (`com.apple.FinderInfo`, `com.apple.ResourceFork`), which breaks Finder aliases. ji restores that metadata after its workspace operations. Set to `false` to disable the restore writes — the fidelity warnings are still reported.

```toml
preserve-finder-xattrs = false
```

See [xattrs.md](xattrs.md) for the mechanism, restore rules, and limitations.

## Hooks

Two hook tables, both keyed by hook name and mapping to a shell command. Commands are run via `sh -c` with the new workspace directory as cwd.

### `[pre-start]` — blocking, sequential, fail-fast

Runs after workspace creation, before `ji` returns (CLI) or before the TUI switches focus to the new workspace. Hooks run in key order (BTreeMap — alphabetical). A non-zero exit aborts subsequent hooks.

```toml
[pre-start]
deps = "uv sync"
mcp = """
if [ -f .mcp.json ]; then
  sed -i '' "s|{{ default_workspace_path }}|{{ workspace_path }}|g" .mcp.json
fi
"""
```

Use for: dependency installation, file rewrites, anything the workspace needs before you start working.

### `[post-start]` — fire-and-forget, background

Spawned in the background after `pre-start` completes. ji does not wait for them and does not collect their exit codes.

```toml
[post-start]
server = "npm run dev"
```

Use for: dev servers, watchers, long-running processes.

Because post-start hooks are not tracked, a background process started this way will keep running after you close the workspace — ji has no handle to stop it. If you need lifecycle-managed processes, run them under a supervisor (`launchd`, `tmux`, a project runner). See [troubleshooting.md](troubleshooting.md#background-hooks-outlive-a-closed-workspace).

## File templates

```toml
[templates]
".mcp.json" = """
{
  "mcpServers": {
    "myserver": {
      "command": "node",
      "args": ["{{ workspace_path }}/server.js"]
    }
  }
}
"""
```

Each entry creates a file in the new workspace. Keys are relative paths from the workspace root; parent directories are created automatically. Values are expanded with the template variables below. Existing files are not overwritten.

## Template variables

| Variable | `workspace-path` | hooks & file templates | Description |
|---|---|---|---|
| `{{ home }}` | ✓ | ✓ | `$HOME` |
| `{{ repo }}` | ✓ | ✓ | Repository name — from `repo` field, else directory name |
| `{{ bookmark }}` | ✓ | ✓ | Bookmark name for the new workspace, with `/` replaced by `-` |
| `{{ default_workspace_path }}` | ✓ | ✓ | Absolute path to the default workspace (`default@`) |
| `{{ workspace_path }}` |   | ✓ | Absolute path to the new workspace |
| `{{ workspace_name }}` |   | ✓ | Directory name of the new workspace |
| `{{ change_id }}` |   | ✓ | jj change ID of the new workspace's `@` at hook time |

Variables marked only under "hooks & file templates" are not available in `workspace-path` because the workspace does not yet exist when that template is resolved.

Template variables are applied by **string substitution only** — they are not exported as environment variables to hook commands. A hook that needs a value at runtime should inline it in the command string:

```toml
[pre-start]
# Good — the value is substituted into the command string at expansion time.
touch_marker = "echo {{ change_id }} > .ji/created-at"

# Will not work — $workspace_path is not set in the hook's environment.
bad = "cd $workspace_path && ls"
```

## Template validation

ji validates every template string at config load time. Two warning kinds are produced:

- **Unknown variable** — `{{ foo }}` where `foo` is not in the table above
- **Malformed delimiter** — e.g., `{{foo}}` (missing space), `{ foo }`, unclosed `{{`

Behavior on warnings:

- **TUI** — a `ConfigWarning` modal appears before any workspace create action, listing the warnings. You can proceed or cancel.
- **CLI** — warnings are printed to stderr and the command proceeds.

Fix warnings by correcting the template or removing the offending variable.

## Full example

```toml
# Override the default repo name (optional; defaults to directory name).
repo = "myproject"

# Put new workspaces next to the main checkout, named by bookmark.
workspace-path = "../{{ repo }}.{{ bookmark }}"

# Author override for ji-generated helper revisions.
ji-author = "Jujutsu Gi <ji@null.com>"

# Custom graph template for the TUI.
log-template = '''
concat(
  separate(" ",
    change_id.shortest(),
    if(bookmarks, label("rest", "[ " ++ bookmarks ++ " ]"), ""),
    if(empty, label("empty", "(empty)"), ""),
    if(conflict, label("conflict", "CONFLICT"), "")
  ),
  " > ",
  if(description == "", label("description placeholder", "(no desc)"), description.first_line()),
  "\n"
)
'''

# Blocking setup hooks.
[pre-start]
deps = "uv sync"
mcp = """
if [ -f .mcp.json ]; then
  sed -i '' "s|{{ default_workspace_path }}|{{ workspace_path }}|g" .mcp.json
fi
"""

# Background services.
[post-start]
server = "npm run dev"

# Files generated per workspace.
[templates]
".mcp.json" = """
{
  "mcpServers": {
    "myserver": {
      "command": "node",
      "args": ["{{ workspace_path }}/server.js"]
    }
  }
}
"""
```
