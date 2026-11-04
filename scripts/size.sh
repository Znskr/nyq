#!/usr/bin/env bash
# Measure the release binary and compare it with size-budget.json.
#
# One command, no per-release guesswork: build, measure, record a JSON report,
# and fail when the binary exceeds the budget for its target. Budgets are per
# target triple so Linux, macOS and Windows can be tracked independently.
#
# Usage:
#   scripts/size.sh                     # build, measure, record
#   scripts/size.sh --check --summary   # build, measure, gate, print markdown
#   scripts/size.sh --update            # record the current size as the budget
#   scripts/size.sh --attribution       # also show per-crate text sizes
#   scripts/size.sh --binary path/nyq --target aarch64-apple-darwin --no-build
#
# The budget is a cap, not an equality check: shrinking passes and `--update`
# tightens the cap when a reduction is intentional.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BUDGET_FILE="$ROOT/size-budget.json"
REPORT_DIR="$ROOT/target/size"
DEFAULT_TOLERANCE=65536 # 64 KiB of build noise, about 1% of the current size

BUILD=1
CHECK=0
UPDATE=0
SUMMARY=0
ATTRIBUTION=0
BINARY=""
TARGET=""
TOLERANCE=""

usage() {
    cat <<'EOF'
Measure the release binary and compare it with size-budget.json.

Usage:
  scripts/size.sh                     # build, measure, record
  scripts/size.sh --check --summary   # build, measure, gate, print markdown
  scripts/size.sh --update            # record the current size as the budget
  scripts/size.sh --attribution       # also show per-crate text sizes
  scripts/size.sh --binary path/nyq --target aarch64-apple-darwin --no-build

Options:
  --binary PATH       measure this binary instead of building one
  --target TRIPLE     build and record for this target
  --budget FILE       use a budget file other than size-budget.json
  --tolerance BYTES   build noise allowance (default 65536)
  --no-build          skip the cargo build
  --check             exit 1 when the binary exceeds its budget
  --update            record the current size as the budget
  --summary           print a markdown row (and append to GITHUB_STEP_SUMMARY)
  --attribution       show per-crate text sizes (needs cargo-bloat)

The budget is a cap, not an equality check: shrinking passes and --update
tightens the cap when a reduction is intentional.
EOF
}

die() {
    printf 'size.sh: %s\n' "$*" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --binary)
            [ $# -ge 2 ] || die "--binary needs a path"
            BINARY=$2
            BUILD=0
            shift 2
            ;;
        --target)
            [ $# -ge 2 ] || die "--target needs a triple"
            TARGET=$2
            shift 2
            ;;
        --budget)
            [ $# -ge 2 ] || die "--budget needs a path"
            BUDGET_FILE=$2
            shift 2
            ;;
        --tolerance)
            [ $# -ge 2 ] || die "--tolerance needs a byte count"
            TOLERANCE=$2
            shift 2
            ;;
        --no-build)
            BUILD=0
            shift
            ;;
        --check)
            CHECK=1
            shift
            ;;
        --update)
            UPDATE=1
            shift
            ;;
        --summary)
            SUMMARY=1
            shift
            ;;
        --attribution)
            ATTRIBUTION=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            die "unknown argument: $1"
            ;;
    esac
done

command -v python3 >/dev/null || die "python3 is required for JSON handling"

HOST=$(rustc -vV | sed -n 's/^host: //p')
[ -n "$HOST" ] || die "could not determine the host triple from rustc"
TRIPLE=${TARGET:-$HOST}

# Cargo puts --target builds under target/<triple>/release, and host builds
# directly under target/release. Keep the lookup in one place so the same
# command works locally and in CI (where CARGO_TARGET_DIR is outside the repo).
target_dir=${CARGO_TARGET_DIR:-target}
case "$target_dir" in
    /*) ;;
    *) target_dir="$ROOT/$target_dir" ;;
esac
if [ -n "$BINARY" ]; then
    binary=$BINARY
elif [ -n "$TARGET" ]; then
    binary="$target_dir/$TRIPLE/release/nyq"
else
    binary="$target_dir/release/nyq"
fi
case "$TRIPLE" in
    *windows*) binary="$binary.exe" ;;
esac

cd "$ROOT"
if [ "$BUILD" = 1 ]; then
    cargo_args=(build --release --locked)
    if [ -n "$TARGET" ]; then
        cargo_args+=(--target "$TARGET")
    fi
    printf 'size.sh: cargo %s\n' "${cargo_args[*]}"
    cargo "${cargo_args[@]}"
fi

[ -f "$binary" ] || die "no binary at $binary (build it, or pass --binary)"

bytes=$(wc -c <"$binary" | tr -d ' ')
mib=$(awk -v b="$bytes" 'BEGIN { printf "%.2f", b / 1048576 }')
sha=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)
rustc_version=$(rustc --version | cut -d' ' -f2)

# Update first, so `--update --check` checks the budget it just wrote.
if [ "$UPDATE" = 1 ]; then
    python3 - "$BUDGET_FILE" "$TRIPLE" "$bytes" "${TOLERANCE:-}" "$DEFAULT_TOLERANCE" <<'PY'
import json, sys
path, target, size = sys.argv[1], sys.argv[2], int(sys.argv[3])
requested, fallback = (int(x) if x else None for x in sys.argv[4:6])
try:
    data = json.load(open(path))
except FileNotFoundError:
    data = {"version": 1, "targets": {}}
data.setdefault("version", 1)
data.setdefault("targets", {})
existing = data["targets"].get(target, {}).get("tolerance_bytes")
tolerance = requested if requested is not None else (existing if existing is not None else fallback)
data["targets"][target] = {"bytes": size, "tolerance_bytes": tolerance}
with open(path, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
PY
    printf 'size.sh: recorded %s = %s bytes in %s\n' "$TRIPLE" "$bytes" "$BUDGET_FILE"
fi

# Read the budget entry for this target. Empty means "not seeded yet": record
# the measurement, but do not fail the build on an unknown baseline.
budget=$(
    python3 - "$BUDGET_FILE" "$TRIPLE" <<'PY'
import json, sys
path, target = sys.argv[1], sys.argv[2]
try:
    data = json.load(open(path))
except FileNotFoundError:
    sys.exit(0)
entry = data.get("targets", {}).get(target)
if entry:
    print(entry["bytes"], entry.get("tolerance_bytes", 0))
PY
)

budget_bytes=""
tolerance_bytes=""
limit_bytes=""
status="ok"
if [ -n "$budget" ]; then
    budget_bytes=${budget%% *}
    tolerance_bytes=${budget##* }
    limit_bytes=$((budget_bytes + tolerance_bytes))
    if [ "$bytes" -gt "$limit_bytes" ]; then
        status="over"
    fi
else
    status="unseeded"
fi

mkdir -p "$REPORT_DIR"
report="$REPORT_DIR/$TRIPLE.json"
python3 - "$report" "$TRIPLE" "$bytes" "$mib" "$budget_bytes" "$tolerance_bytes" \
    "$status" "$sha" "$rustc_version" "$binary" <<'PY'
import json, sys
path, target, size, mib, budget, tolerance, status, sha, rustc, binary = sys.argv[1:]
report = {
    "target": target,
    "bytes": int(size),
    "mib": float(mib),
    "budget_bytes": int(budget) if budget else None,
    "tolerance_bytes": int(tolerance) if tolerance else None,
    "status": status,
    "git_sha": sha,
    "rustc": rustc,
    "binary": binary,
}
with open(path, "w") as f:
    json.dump(report, f, indent=2)
    f.write("\n")
PY

case "$status" in
    over)
        delta=$((bytes - limit_bytes))
        printf 'size.sh: %s %s bytes (%.2f MiB) exceeds the %s byte cap by %s bytes\n' \
            "$TRIPLE" "$bytes" "$mib" "$limit_bytes" "$delta" >&2
        ;;
    unseeded)
        printf 'size.sh: %s %s bytes (%.2f MiB); no budget seeded for this target\n' \
            "$TRIPLE" "$bytes" "$mib"
        ;;
    *)
        printf 'size.sh: %s %s bytes (%.2f MiB); within budget (%s byte cap)\n' \
            "$TRIPLE" "$bytes" "$mib" "$limit_bytes"
        ;;
esac

if [ "$SUMMARY" = 1 ]; then
    if [ -n "$budget_bytes" ]; then
        budget_cell=$limit_bytes
        delta=$((bytes - limit_bytes))
        if [ "$delta" -gt 0 ]; then
            delta_cell="+$delta"
        else
            delta_cell=$delta
        fi
    else
        budget_cell="not seeded"
        delta_cell="—"
    fi
    row="| $TRIPLE | $bytes | $mib | $budget_cell | $delta_cell | $status |"
    printf '\n| target | bytes | MiB | cap | delta | status |\n'
    printf '| --- | ---: | ---: | ---: | ---: | --- |\n'
    printf '%s\n' "$row"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        {
            printf '### Binary size\n\n'
            printf '| target | bytes | MiB | cap | delta | status |\n'
            printf '| --- | ---: | ---: | ---: | ---: | --- |\n'
            printf '%s\n' "$row"
        } >>"$GITHUB_STEP_SUMMARY"
    fi
fi

if [ "$ATTRIBUTION" = 1 ]; then
    if command -v cargo-bloat >/dev/null; then
        # cargo-bloat needs symbols, so it builds in its own directory and
        # leaves the measured release binary untouched.
        printf '\nPer-crate text attribution:\n'
        cargo bloat --release --locked --crates -n 25 \
            --target-dir "$ROOT/target/size-attribution"
    else
        printf 'size.sh: cargo-bloat is not installed; skipping attribution\n'
    fi
fi

if [ "$CHECK" = 1 ] && [ "$status" = over ]; then
    exit 1
fi
