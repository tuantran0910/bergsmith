#!/usr/bin/env bash
# Classifies a change into the CI areas it can affect, one key per line.
#
#   rust  - Rust workspace, toolchain, and build/test inputs
#   web   - web UI and the generated API contract
#   proto - protobuf sources and buf configuration
#   deps  - Cargo dependency policy
#   observability - Prometheus/Grafana assets and the metrics contract
#
# Pull requests use this to skip unrelated jobs; pushes to main and manual
# runs pass --all. The selector fails closed: a path it does not recognize
# selects every area, so a new top-level directory cannot silently skip CI.
#
# Usage:
#   scripts/changed-areas.sh [--base REF] [--all]
#   scripts/changed-areas.sh --files FILE
#
# --base compares REF...HEAD (three-dot, so movement on the base branch is
# ignored) and includes untracked files. --files reads newline-separated paths
# instead, for tests. --all selects every area.
set -euo pipefail

base="origin/main"
all=false
files=""

while [ "$#" -gt 0 ]; do
	case "$1" in
		--base)
			[ "$#" -ge 2 ] || {
				echo "error: --base needs a ref" >&2
				exit 2
			}
			base="$2"
			shift 2
			;;
		--files)
			[ "$#" -ge 2 ] || {
				echo "error: --files needs a path" >&2
				exit 2
			}
			files="$2"
			shift 2
			;;
		--all)
			all=true
			shift
			;;
		--help | -h)
			sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
			exit 0
			;;
		*)
			echo "error: unknown argument: $1" >&2
			exit 2
			;;
	esac
done

rust=false
web=false
proto=false
deps=false
observability=false

select_all() {
	rust=true
	web=true
	proto=true
	deps=true
	observability=true
}

classify() {
	local path="$1"
	case "$path" in
		# Workflow, Makefile, and script changes can alter any gate.
		.github/* | Makefile | scripts/*)
			select_all
			;;
		# The web client is generated from this contract.
		crates/coordinator/src/api/openapi.json)
			rust=true
			web=true
			;;
		# Protobuf sources feed the Rust build and the buf checks.
		crates/protocol/* | *.proto | buf.yaml | buf.gen.yaml | buf.lock)
			rust=true
			proto=true
			;;
		# The metrics contract and the exposition acceptance fixture.
		crates/coordinator/src/observability/* | crates/coordinator/tests/observability.rs)
			rust=true
			observability=true
			;;
		# Packaged alert rules, dashboards, and their acceptance stack.
		deploy/observability/*)
			observability=true
			;;
		# The observability gate renders with the web toolchain's Chromium.
		web/package.json | web/package-lock.json | web/.nvmrc)
			web=true
			observability=true
			;;
		# Any manifest or lockfile can change the dependency policy.
		*Cargo.toml | *Cargo.lock)
			rust=true
			deps=true
			;;
		deny.toml)
			deps=true
			;;
		crates/* | bin/* | .cargo/* | .config/* | rust-toolchain.toml | rustfmt.toml | clippy.toml | deploy/*)
			rust=true
			;;
		web/*)
			web=true
			;;
		# Documentation and repository housekeeping do not run a code gate.
		docs/* | *.md | LICENSE | .gitignore | .gitattributes | .editorconfig)
			;;
		# Fail closed: an unmapped path runs everything until it is classified.
		*)
			select_all
			;;
	esac
}

if [ "$all" = true ]; then
	select_all
elif [ -n "$files" ]; then
	[ -f "$files" ] || {
		echo "error: no such path list: $files" >&2
		exit 2
	}
	while IFS= read -r path; do
		[ -n "$path" ] || continue
		classify "$path"
	done <"$files"
elif merge_base="$(git merge-base "$base" HEAD 2>/dev/null)"; then
	changed="$(mktemp)"
	trap 'rm -f "$changed"' EXIT
	if git diff --name-only -z "$merge_base" HEAD >"$changed"; then
		git ls-files --others --exclude-standard -z >>"$changed" || true
		while IFS= read -r -d '' path; do
			classify "$path"
		done <"$changed"
	else
		# A diff failure must not make a change look like it touched nothing.
		echo "warning: cannot diff against $merge_base; selecting every area" >&2
		select_all
	fi
else
	# Fail closed: an unavailable base (a shallow clone, for example) must
	# not make a pull request look like it touched nothing.
	echo "warning: cannot find the merge base with $base; selecting every area" >&2
	select_all
fi

printf 'rust=%s\n' "$rust"
printf 'web=%s\n' "$web"
printf 'proto=%s\n' "$proto"
printf 'deps=%s\n' "$deps"
printf 'observability=%s\n' "$observability"
