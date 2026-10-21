#!/usr/bin/env bash
# Prints the workspace packages whose tests a change can affect, one per line.
#
# A package is affected when a changed file lies inside it, or when it depends
# (normally, for building, or for tests) on an affected package, transitively.
# Changes to shared build or test inputs affect every package. A change that
# touches no package and no shared input (documentation, for example) prints
# nothing.
#
# The change is everything since the merge base with BASE (default
# origin/main), plus staged, unstaged, and untracked files.
#
# This is a local convenience. CI always runs the full suite, which the
# coverage gate requires.
set -euo pipefail

base="${1:-origin/main}"
root="$(git rev-parse --show-toplevel)"
cd "$root"

merge_base="$(git merge-base "$base" HEAD)"
changed="$(
	{
		git diff --name-only "$merge_base"
		git ls-files --others --exclude-standard
	} | sort -u
)"
[ -n "$changed" ] || exit 0

# Workspace packages as "<directory> <name>", read from their manifests.
packages="$(
	for manifest in crates/*/Cargo.toml bin/*/Cargo.toml; do
		name="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$manifest" | head -n 1)"
		printf '%s %s\n' "$(dirname "$manifest")" "$name"
	done
)"

# Inputs every package's build or tests read. Anything here selects all.
shared='^(Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|clippy\.toml|deny\.toml|buf\.yaml|Makefile|\.cargo/|\.config/|\.github/|deploy/|scripts/)'
if grep -Eq "$shared" <<<"$changed"; then
	cut -d' ' -f2 <<<"$packages"
	exit 0
fi

touched=()
while read -r directory name; do
	if grep -q "^$directory/" <<<"$changed"; then
		touched+=("$name")
	fi
done <<<"$packages"
[ "${#touched[@]}" -gt 0 ] || exit 0

# Reverse dependencies over normal, build, and dev edges, limited to
# workspace members.
invert=()
for name in "${touched[@]}"; do
	invert+=(--invert "$name")
done
cargo tree --workspace --locked --offline --edges normal,build,dev \
	--prefix none --format '{p}' "${invert[@]}" |
	cut -d' ' -f1 |
	grep -Fxf <(cut -d' ' -f2 <<<"$packages") |
	sort -u
