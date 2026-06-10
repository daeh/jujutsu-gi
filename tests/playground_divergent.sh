#!/usr/bin/env bash
# Scaffolds a jj repo containing divergent revisions for manual TUI testing.
# Usage: task playground:divergence
set -euo pipefail
export JI_LOG=1

DIR="$(mktemp -d)"

REPO="$DIR/repo"
jj git init "$REPO"
jj -R "$REPO" config set --repo user.name "Test"
jj -R "$REPO" config set --repo user.email "test@test.com"

cd "$REPO"

# --- build base history ---
echo "base" > base.txt
jj describe -m "initial commit"
jj bookmark create main -r @
jj new -m "second commit"
echo "second" > second.txt

# --- create a divergent change ---
# 1. Make a commit and capture its SHA
jj commit -m "will-diverge: original version"
ORIG_SHA=$(jj log -r @- -T 'commit_id' --no-graph --color never --no-pager)

# 2. Rewrite it (hides the original, creates successor with same change ID)
jj metaedit -r @- -m "will-diverge: rewritten version"

# 3. Make the hidden original visible by creating a child of it
jj new "$ORIG_SHA" -m "child of original (causes divergence)"
echo "child content" > child.txt

# --- continue history on main line ---
jj new main -m "back on main"
echo "more work" > more.txt
jj commit -m "post-divergence work"
jj new -m "wip"

# --- done ---
echo ""
echo "=== Divergent playground ready ==="
echo "  repo: $REPO"
echo ""
echo "Run: cd $REPO && jj log --color never --no-pager --limit 15"
echo "Look for '??' markers indicating divergent changes."
