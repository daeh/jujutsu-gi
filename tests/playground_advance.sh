#!/usr/bin/env bash
# Tests advance-parent behavior when creating workspaces.
#
# Usage: task playground:advance
set -euo pipefail
export JI_LOG=1

DIR="$(mktemp -d)"
DIRECTIVE="$(mktemp)"
trap 'rm -rf "$DIR" "$DIRECTIVE"' EXIT

REPO="$DIR/repo/eg"
mkdir -p "$REPO"

# --- init repo ---
jj git init "$REPO"
jj -R "$REPO" config set --repo user.name "Test"
jj -R "$REPO" config set --repo user.email "test@test.com"

cd "$REPO"
echo "base" > base.txt
jj describe -m "initial commit"
jj new -m "default wip"
jj bookmark create main -r @-

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji init

# ===================================================================
# Test 1: Branching from @ advances parent (default)
# ===================================================================
echo ""
echo "=== Test 1: advance parent (default) ==="

DEFAULT_AT_BEFORE=$(jj log --no-graph -r '@' -T 'change_id')
echo "  default @ before: $DEFAULT_AT_BEFORE"

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji ws feature/alpha --create --revision "$DEFAULT_AT_BEFORE" -m "alpha start" 2>&1

DEFAULT_AT_AFTER=$(jj log --no-graph -r '@' -T 'change_id')
echo "  default @ after:  $DEFAULT_AT_AFTER"

if [ "$DEFAULT_AT_BEFORE" != "$DEFAULT_AT_AFTER" ]; then
    DEFAULT_DESC=$(jj log --no-graph -r '@' -T 'description.first_line()')
    if [ "$DEFAULT_DESC" = "(ji)::branch" ]; then
        echo "  PASS: default @ advanced with '(ji)::branch' description"
    else
        echo "  FAIL: default @ advanced but description is '$DEFAULT_DESC', expected '(ji)::branch'"
        exit 1
    fi
else
    echo "  FAIL: default @ did not advance"
    exit 1
fi

# Clean up
jj workspace forget feature-alpha 2>/dev/null || true

# ===================================================================
# Test 2: --no-advance prevents parent advance
# ===================================================================
echo ""
echo "=== Test 2: --no-advance ==="

DEFAULT_AT_BEFORE=$(jj log --no-graph -r '@' -T 'change_id')
echo "  default @ before: $DEFAULT_AT_BEFORE"

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji ws feature/beta --create --no-advance --revision "$DEFAULT_AT_BEFORE" -m "beta start" 2>&1

DEFAULT_AT_AFTER=$(jj log --no-graph -r '@' -T 'change_id')
echo "  default @ after:  $DEFAULT_AT_AFTER"

if [ "$DEFAULT_AT_BEFORE" = "$DEFAULT_AT_AFTER" ]; then
    echo "  PASS: default @ unchanged with --no-advance"
else
    echo "  FAIL: default @ changed despite --no-advance"
    exit 1
fi

# Clean up
jj workspace forget feature-beta 2>/dev/null || true

# ===================================================================
# Test 3: Branching from bookmark does NOT advance
# ===================================================================
echo ""
echo "=== Test 3: branch from bookmark (no advance) ==="

DEFAULT_AT_BEFORE=$(jj log --no-graph -r '@' -T 'change_id')
echo "  default @ before: $DEFAULT_AT_BEFORE"

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji ws feature/gamma --create --revision main -m "gamma start" 2>&1

DEFAULT_AT_AFTER=$(jj log --no-graph -r '@' -T 'change_id')
echo "  default @ after:  $DEFAULT_AT_AFTER"

if [ "$DEFAULT_AT_BEFORE" = "$DEFAULT_AT_AFTER" ]; then
    echo "  PASS: default @ unchanged when branching from bookmark"
else
    echo "  FAIL: default @ changed when branching from bookmark"
    exit 1
fi

# Clean up
jj workspace forget feature-gamma 2>/dev/null || true

echo ""
echo "=== All tests passed ==="
