#!/usr/bin/env bash
# Tests workspace path expansion and template variable validation.
#
# Exercises:
#   - {{ home }} and {{ default_workspace_path }} expansion
#   - {{ bbb }} triggers an unknown-variable warning
#   - {{home}} (missing spaces) triggers a malformed-delimiter warning
#
# Usage: task playground:paths
set -euo pipefail
export JI_LOG=1

DIR="$(mktemp -d)"
DIRECTIVE="$(mktemp)"
STDERR_FILE="$(mktemp)"
trap 'rm -rf "$DIR" "$DIRECTIVE" "$STDERR_FILE"' EXIT

REPO="$DIR/repo/eg"
mkdir -p "$REPO"

# --- init repo ---
jj git init "$REPO"
jj -R "$REPO" config set --repo user.name "Test"
jj -R "$REPO" config set --repo user.email "test@test.com"

# --- seed history ---
cd "$REPO"
echo "base" > base.txt
jj describe -m "initial commit"
jj new -m "second commit"
echo "second" > second.txt
jj describe -m "second commit"
jj new -m "default wip"
jj bookmark create main -r @-

# ===================================================================
# Test 1: {{ home }} expansion
# ===================================================================
echo ""
echo "=== Test 1: {{ home }} expansion ==="

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji init
cat > .config/ji.toml << 'TOML'
workspace-path = "{{ home }}/ji-test-workspaces/{{ repo }}.{{ bookmark }}"
TOML

EXPECTED="$HOME/ji-test-workspaces/eg.test-home"
JI_DIRECTIVE_FILE="$DIRECTIVE" command ji ws test/home --create --revision main -m "test home"

if [ -d "$EXPECTED" ]; then
    echo "  PASS: {{ home }} expanded to $HOME"
    # Clean up
    jj -R "$REPO" workspace forget test-home 2>/dev/null || true
    rm -rf "$EXPECTED"
    rmdir "$HOME/ji-test-workspaces" 2>/dev/null || true
else
    echo "  FAIL: expected $EXPECTED"
    find "$DIR" -type d -name "eg.*" 2>/dev/null || true
    exit 1
fi

# ===================================================================
# Test 2: {{ default_workspace_path }} expansion
# ===================================================================
echo ""
echo "=== Test 2: {{ default_workspace_path }} expansion ==="

cat > .config/ji.toml << 'TOML'
workspace-path = "{{ default_workspace_path }}/../workspaces/{{ repo }}.{{ bookmark }}"
TOML

REPO_ABS="$(cd "$REPO" && pwd -P)"
EXPECTED="$REPO_ABS/../workspaces/eg.test-pwp"
# Normalize
EXPECTED="$(cd "$REPO_ABS/.." && pwd -P)/workspaces/eg.test-pwp"

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji ws test/pwp --create --revision main -m "test pwp"

if [ -d "$EXPECTED" ]; then
    echo "  PASS: {{ default_workspace_path }} expanded correctly"
    jj -R "$REPO" workspace forget test-pwp 2>/dev/null || true
    rm -rf "$EXPECTED"
else
    echo "  FAIL: expected $EXPECTED"
    find "$DIR" -type d -name "eg.*" 2>/dev/null || true
    exit 1
fi

# ===================================================================
# Test 3: {{ bbb }} triggers unknown-variable warning
# ===================================================================
echo ""
echo "=== Test 3: unknown variable warning ==="

cat > .config/ji.toml << 'TOML'
workspace-path = "../aaa/{{ bbb }}/workspaces/{{ repo }}.{{ bookmark }}"
TOML

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji ws test/unknown --create --revision main -m "test unknown" 2>"$STDERR_FILE" || true
STDERR_OUTPUT="$(cat "$STDERR_FILE")"

if echo "$STDERR_OUTPUT" | grep -q "unknown template variable"; then
    echo "  PASS: warning printed for {{ bbb }}"
    echo "  stderr: $STDERR_OUTPUT"
else
    echo "  FAIL: no warning for {{ bbb }}"
    echo "  stderr: $STDERR_OUTPUT"
    exit 1
fi

# Clean up workspace if created
jj -R "$REPO" workspace forget test-unknown 2>/dev/null || true

# ===================================================================
# Test 4: {{home}} (malformed) triggers warning
# ===================================================================
echo ""
echo "=== Test 4: malformed delimiter warning ==="

cat > .config/ji.toml << 'TOML'
workspace-path = "{{home}}/workspaces/{{ repo }}.{{ bookmark }}"
TOML

JI_DIRECTIVE_FILE="$DIRECTIVE" command ji ws test/malformed --create --revision main -m "test malformed" 2>"$STDERR_FILE" || true
STDERR_OUTPUT="$(cat "$STDERR_FILE")"

if echo "$STDERR_OUTPUT" | grep -q "malformed template variable"; then
    echo "  PASS: warning printed for {{home}}"
    echo "  stderr: $STDERR_OUTPUT"
else
    echo "  FAIL: no warning for {{home}}"
    echo "  stderr: $STDERR_OUTPUT"
    exit 1
fi

# Clean up workspace if created
jj -R "$REPO" workspace forget test-malformed 2>/dev/null || true

echo ""
echo "=== All tests passed ==="
