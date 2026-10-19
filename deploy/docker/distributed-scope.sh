#!/usr/bin/env bash
# Run inside a systemd service delegated to the invoking user. This script
# never changes controllers or ownership outside that service's cgroup.
set -euo pipefail
test "$EUID" -ne 0

IFS=: read -r hierarchy controllers relative < /proc/self/cgroup
test "$hierarchy" = 0
test -z "$controllers"
scope="/sys/fs/cgroup${relative}"
test "$scope" != /sys/fs/cgroup/
test "$scope" != /sys/fs/cgroup

# Keep supervisors in a leaf so the delegated root can distribute controllers
# to task siblings. Every task migration then stays inside this delegation.
mkdir "$scope/supervisors"
printf '%s\n' "$$" > "$scope/supervisors/cgroup.procs"
printf '+cpu +memory +pids\n' > "$scope/cgroup.subtree_control"
export BERGSMITH_TEST_CGROUP_ROOT="$scope"
exec make test-distributed
