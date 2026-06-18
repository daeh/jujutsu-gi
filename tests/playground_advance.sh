#!/usr/bin/env bash
# Tests source-workspace auto-advance when creating workspaces.
#
# Under the current model (src/operations.rs::create_workspace) the source
# workspace steps forward — a new "(ji::step-forward)" revision becomes @ —
# only when the new workspace branches from a NON-TRIVIAL head. Branching from
# a trivial head (empty WIP with a trivial description) or from an interior
# (non-head) revision does not step the source.
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
# Test 1: branching from a NON-TRIVIAL @ steps the source forward
# ===================================================================
echo ""
echo "=== Test 1: advance from a non-trivial head ==="

# Give @ real work so it is unambiguously a non-trivial head. The `jj log`
# below snapshots this edit into @ before `ji new` runs — load-bearing,
# because the create path classifies @ with `--ignore-working-copy` and would
# otherwise still see an empty head and not step.
echo "alpha work" > alpha.txt
BEFORE=$(jj log --no-graph -r '@' -T 'change_id')
echo "  @ before: $BEFORE"

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji new feature/alpha --revision "$BEFORE" -m "alpha start" 2>&1

AFTER=$(jj log --no-graph -r '@' -T 'change_id')
echo "  @ after:  $AFTER"

if [ "$BEFORE" != "$AFTER" ]; then
    DESC=$(jj log --no-graph -r '@' -T 'description.first_line()')
    if [ "$DESC" = "(ji::step-forward)" ]; then
        echo "  PASS: source @ advanced with '(ji::step-forward)' description"
    else
        echo "  FAIL: source @ advanced but description is '$DESC', expected '(ji::step-forward)'"
        exit 1
    fi
else
    echo "  FAIL: source @ did not advance from a non-trivial head"
    exit 1
fi

# The new workspace must be rooted at the non-trivial head itself (the junction).
ALPHA_JUNCTION=$(jj log --no-graph -r '"eg.feature-alpha"@-' -T 'change_id')
if [ "$ALPHA_JUNCTION" = "$BEFORE" ]; then
    echo "  PASS: feature/alpha rooted at the non-trivial head"
else
    echo "  FAIL: feature/alpha rooted at '$ALPHA_JUNCTION', expected junction '$BEFORE'"
    exit 1
fi

# Clean up
jj workspace forget eg.feature-alpha

# ===================================================================
# Test 2: branching from a TRIVIAL head does NOT advance
# ===================================================================
echo ""
echo "=== Test 2: no advance from a trivial head ==="

# A fresh empty working-copy revision (no file changes, default description)
# is a trivial head.
jj new
BEFORE=$(jj log --no-graph -r '@' -T 'change_id')
echo "  @ before: $BEFORE"

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji new feature/beta --revision "$BEFORE" -m "beta start" 2>&1

AFTER=$(jj log --no-graph -r '@' -T 'change_id')
echo "  @ after:  $AFTER"

if [ "$BEFORE" = "$AFTER" ]; then
    echo "  PASS: source @ unchanged when branching from a trivial head"
else
    echo "  FAIL: source @ changed when branching from a trivial head"
    exit 1
fi

# The new workspace must be rooted at the trivial head's PARENT, not the head.
BETA_JUNCTION=$(jj log --no-graph -r '"eg.feature-beta"@-' -T 'change_id')
BETA_EXPECTED=$(jj log --no-graph -r "${BEFORE}-" -T 'change_id')
if [ "$BETA_JUNCTION" = "$BETA_EXPECTED" ]; then
    echo "  PASS: feature/beta rooted at the trivial head's parent"
else
    echo "  FAIL: feature/beta rooted at '$BETA_JUNCTION', expected parent '$BETA_EXPECTED'"
    exit 1
fi

# Clean up
jj workspace forget eg.feature-beta

# ===================================================================
# Test 3: branching from an interior bookmark does NOT advance
# ===================================================================
echo ""
echo "=== Test 3: branch from bookmark (no advance) ==="

BEFORE=$(jj log --no-graph -r '@' -T 'change_id')
echo "  @ before: $BEFORE"

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji new feature/gamma --revision main -m "gamma start" 2>&1

AFTER=$(jj log --no-graph -r '@' -T 'change_id')
echo "  @ after:  $AFTER"

if [ "$BEFORE" = "$AFTER" ]; then
    echo "  PASS: source @ unchanged when branching from an interior bookmark"
else
    echo "  FAIL: source @ changed when branching from an interior bookmark"
    exit 1
fi

# The new workspace must be rooted at main (the interior revision branched from).
GAMMA_JUNCTION=$(jj log --no-graph -r '"eg.feature-gamma"@-' -T 'change_id')
MAIN_ID=$(jj log --no-graph -r 'main' -T 'change_id')
if [ "$GAMMA_JUNCTION" = "$MAIN_ID" ]; then
    echo "  PASS: feature/gamma rooted at main"
else
    echo "  FAIL: feature/gamma rooted at '$GAMMA_JUNCTION', expected main '$MAIN_ID'"
    exit 1
fi

# Clean up
jj workspace forget eg.feature-gamma

echo ""
echo "=== All tests passed ==="
