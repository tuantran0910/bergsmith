#!/usr/bin/env bash
# Checks the CI area mapping in scripts/changed-areas.sh.
#
# The selector is small on purpose, but it decides which gates run. These
# cases keep a new path pattern from silently dropping a gate.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
selector="$root/scripts/changed-areas.sh"
paths="$(mktemp)"
trap 'rm -f "$paths"' EXIT

failed=0

check() {
	local name="$1"
	local expected="$2"
	shift 2

	printf '%s\n' "$@" >"$paths"
	local output
	output="$("$selector" --files "$paths")"
	local case_failed=0

	local area expected_value actual
	for area in rust web proto deps; do
		expected_value=false
		case " $expected " in
			*" $area "*) expected_value=true ;;
		esac
		actual="$(printf '%s\n' "$output" | sed -n "s/^$area=//p")"
		if [ "$actual" != "$expected_value" ]; then
			echo "FAIL $name: $area expected $expected_value, got ${actual:-missing}" >&2
			case_failed=1
			failed=1
		fi
	done

	if [ "$case_failed" -eq 0 ]; then
		echo "ok $name"
	fi
}

check "web source" "web" "web/src/App.tsx"
check "web lockfile" "web" "web/package-lock.json"
check "rust source" "rust" "crates/common/src/lib.rs"
check "rust toolchain" "rust" "rust-toolchain.toml"
check "rust build input" "rust" "deploy/docker/compose.yml"
check "mixed rust and web" "rust web" \
	"crates/common/src/lib.rs" "web/src/App.tsx"
check "generated API contract" "rust web" \
	"crates/coordinator/src/api/openapi.json"
check "protobuf source" "rust proto" "crates/protocol/proto/bergsmith.proto"
check "buf config" "rust proto" "buf.yaml"
check "Cargo manifest" "rust deps" "Cargo.toml"
check "Cargo lockfile" "rust deps" "Cargo.lock"
check "dependency policy" "deps" "deny.toml"
check "documentation" "" "docs/operations/setup.md"
check "root documentation" "" "README.md"
check "workflow" "rust web proto deps" ".github/workflows/ci.yml"
check "Makefile" "rust web proto deps" "Makefile"
check "selector script" "rust web proto deps" "scripts/changed-areas.sh"
check "unknown path fails closed" "rust web proto deps" "new-area/file.txt"

all="$("$selector" --all)"
for area in rust web proto deps; do
	if ! printf '%s\n' "$all" | grep -qx "$area=true"; then
		echo "FAIL --all: $area was not selected" >&2
		failed=1
	fi
done

fallback="$("$selector" --base refs/heads/bergsmith-no-such-branch 2>/dev/null)"
for area in rust web proto deps; do
	if ! printf '%s\n' "$fallback" | grep -qx "$area=true"; then
		echo "FAIL unavailable base: $area was not selected" >&2
		failed=1
	fi
done

if [ "$failed" -ne 0 ]; then
	exit 1
fi
echo "changed-area mapping is correct"
