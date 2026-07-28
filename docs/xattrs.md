# macOS file metadata — Finder aliases, xattrs, hard links

jj (like git) stores only file content and a file mode (regular, executable, symlink). Anything else a file carries on disk is not versioned:

- **Extended attributes (xattrs)** — including `com.apple.FinderInfo`, the attribute that makes a Finder alias an alias
- **Hard links** — two tracked paths sharing one inode

Whenever jj materializes a file — creating a workspace, merging into a working copy, resolving staleness — it writes a fresh file: xattrs are stripped and hard links become independent files. Symlinks are unaffected (jj tracks them as their own file mode).

## What that does to Finder aliases

A Finder alias is a regular file whose content is bookmark data and whose alias-ness lives in a 32-byte `com.apple.FinderInfo` xattr (`alisMACS` plus the kIsAlias flag). After a jj materialization the bookmark data survives byte-for-byte, but the xattr is gone — macOS stops treating the file as an alias and shows a plain document.

## What ji does

Before an operation that materializes files (`ji new`, `ji close`, `ji transfer`, `ji sync`), ji records the Finder-relevant xattrs (`com.apple.FinderInfo`, `com.apple.ResourceFork`) and the hard-link identity of the tracked files in every on-disk workspace. After the operation it restores what can be restored and warns about what cannot:

- **Identical content** — a materialized file whose bytes match a recorded copy gets that copy's full Finder metadata back. This covers unchanged aliases, aliases carried into a new workspace, and a new alias merged from one workspace into another.
- **Retargeted alias** — bookmark data whose bytes changed still gets the alias flag back (the flag is content-independent). The old resource fork is not re-attached to new content.
- **Genuinely different content** — a plain file that replaced an alias gets nothing; ji never invents metadata.
- Files that still carry Finder metadata are left untouched — ji never overwrites metadata that survived or that you set yourself.

Only `com.apple.FinderInfo` and `com.apple.ResourceFork` are handled. System-managed attributes (`com.apple.provenance`, `com.apple.quarantine`) are never copied.

### Warnings

Reported as `(ji)::warning:` lines on CLI stderr and in the TUI status line:

- `hard link broken: <workspace>:<path> is now an independent file` — a hard-linked file was materialized as an independent copy. Content is preserved; the shared inode is not. Re-link manually if needed (`ln -f <original> <copy>`).
- `finder alias: <workspace>:<path> has bookmark data but no restorable Finder metadata (alias flag lost)` — the file looks like an alias, but no on-disk copy exists to take metadata from (for example, the alias only exists in an old revision).

## Configuration

```toml
# .config/ji.toml
preserve-finder-xattrs = true   # default
```

Setting it to `false` disables the restore writes; the fidelity warnings are still reported. See [configuration.md](configuration.md#preserve-finder-xattrs--bool-default-true).

## Limitations

- Metadata exists only on disk, never in the repository. An alias present only in a revision that no current workspace has checked out cannot be restored — ji warns instead.
- Operations run outside ji (`jj workspace add`, `jj new`, `jj op restore`, plain jj in another shell) strip metadata as usual; ji restores only around its own operations.
- A conflicted alias (conflict markers in the data fork) matches no restore rule. Resolve the conflict; a later ji operation can then restore the flag.

## Repairing an already-stripped alias

The bookmark data survives, so re-adding the FinderInfo xattr fully restores an alias:

```sh
xattr -wx com.apple.FinderInfo \
  "616C69734D414353800000000000000000000000000000000000000000000000" <file>
```

## Prefer symlinks where possible

Symlinks round-trip through jj perfectly. If you don't need alias semantics (an alias keeps tracking a target that moves; a symlink does not), a symlink is the only link type jj can represent faithfully.
