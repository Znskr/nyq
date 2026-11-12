#!/bin/sh
# Seed the workspace (a task may build its fixture at run time instead).
cat > notes.md <<'EOF2'
# Notes
TODO: fill me in
last line
EOF2
git init -q 2>/dev/null || true
