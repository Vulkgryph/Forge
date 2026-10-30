#!/usr/bin/env bash
# Every gate a release has to pass that a machine can check, in one pass.
#
# Exists because the gates were real but scattered: some in CI, one in the
# pre-push hook, one inside the packaging script, and the rest written down
# nowhere. Running them meant knowing all of them, which meant they were run
# when somebody remembered. See RELEASING.md for the steps no script can do —
# which is where the leaks have actually come from.
#
# Runs everything before reporting, rather than stopping at the first failure:
# finding out about three problems at once is worth more than finding out about
# the first one sooner.
#
#   ./scripts/preflight.sh            # check
#   ./scripts/preflight.sh --quick    # skip the test suites
set -uo pipefail

# Run from the repository root whatever directory it was invoked from.
cd "$(dirname "$0")/.." || exit 1

quick=false
[[ "${1:-}" == "--quick" ]] && quick=true

pass=0; fail=0
failed_names=()

check() {
    local name="$1"; shift
    printf '  %-46s' "$name"
    local out
    if out="$("$@" 2>&1)"; then
        printf 'ok\n'; pass=$((pass + 1))
    else
        printf 'FAILED\n'; fail=$((fail + 1)); failed_names+=("$name")
        printf '%s\n' "$out" | sed 's/^/      /' | head -15
    fi
}

echo "Identity and privacy"
check "no name or home directory in the tree" \
    python3 scripts/check_private.py --tracked
check "the privacy guard does not cry wolf" \
    python3 scripts/test_check_private.py

echo
echo "Attribution"
check "NOTICE covers every licence shipped" \
    python3 scripts/check_notice_licences.py

echo
echo "Documentation"
# shellcheck disable=SC2016
check "no version pinned as a literal" \
    python3 scripts/check_pinned_versions.py
check "each crate has a dated section for its version" \
    python3 scripts/check_changelog_heads.py
check "documentation links resolve" python3 - <<'PY'
import os, re, subprocess, sys
files = [f for f in subprocess.run(["git","ls-files","*.md"],capture_output=True,text=True)
         .stdout.split() if "/vendor/" not in f and not f.startswith("vendor/")]
dead = []
for f in files:
    base = os.path.dirname(f)
    for m in re.finditer(r'\]\((?!https?:|mailto:)([^)#]+)', open(f,encoding="utf-8").read()):
        t = os.path.normpath(os.path.join(base, m.group(1).strip()))
        if not os.path.exists(t): dead.append(f"{f} -> {m.group(1)}")
for d in dead: print(d)
sys.exit(1 if dead else 0)
PY

echo
echo "Installers"
check "installer scripts parse" bash -c '
    bash -n forge-agent/install.sh && bash -n forge-agent/update.sh \
      && bash -n bootstrap.sh && bash -n install.sh && bash -n uninstall.sh'
check "bootstrap references paths that exist" bash scripts/test_bootstrap_paths.sh

echo
echo "Repository"
# The single quotes are the point: these expand inside the inner shell at
# check time, not here at parse time.
# shellcheck disable=SC2016
check "working tree is clean" bash -c '[ -z "$(git status --porcelain)" ]'
# Skipped on a detached HEAD, which is what a tag checkout is — the release
# workflow runs this against the tag, where having no upstream is correct
# rather than a fault. Reporting it there is how a gate teaches people to
# ignore gates.
# shellcheck disable=SC2016
check "on a branch with an upstream" bash -c '
    git symbolic-ref -q HEAD >/dev/null || exit 0
    git rev-parse --abbrev-ref --symbolic-full-name "@{u}" >/dev/null'
# shellcheck disable=SC2016
check "nothing unpushed" bash -c '
    git symbolic-ref -q HEAD >/dev/null || exit 0
    n=$(git rev-list --count "@{u}..HEAD" 2>/dev/null || echo 0)
    [ "$n" -eq 0 ] || { echo "$n commit(s) not pushed"; exit 1; }'

if ! $quick; then
    echo
    echo "Builds"
    # A plain build, not only the test build. They are not the same: a field
    # read solely by a test is dead code in the binary, and with warnings
    # denied that is an error CI hits and `cargo test` never does. That
    # happened — found by a Windows gate, reproduced on macOS in one command,
    # and not a platform difference at all.
    check "agent and clients build with warnings denied" \
        env RUSTFLAGS=-D\ warnings cargo build -q -p forge-agent -p forge-agent-proto -p forge-search -p forge-tui-rs
    check "editor builds with warnings denied" \
        env RUSTFLAGS=-D\ warnings cargo build -q -p forge-ide -p forge-proto -p forge-server

    echo
    echo "Tests"
    check "agent, protocol, search, terminal client" \
        cargo test -q -p forge-agent -p forge-agent-proto -p forge-search -p forge-tui-rs
    check "editor" cargo test -q -p forge-ide -p forge-proto -p forge-server
fi

echo
if [ "$fail" -eq 0 ]; then
    echo "preflight: $pass passed."
    echo
    echo "What this did NOT check — see RELEASING.md:"
    echo "  * the release notes, which no script reads and which is where"
    echo "    identity has actually leaked"
    echo "  * screenshots: crop them, then look at the window title, the tab"
    echo "    paths, the shell prompt and the file tree before attaching"
    echo "  * that the tag is not already on the remote"
    exit 0
fi

echo "preflight: $fail failed, $pass passed."
for n in ${failed_names[@]+"${failed_names[@]}"}; do echo "  - $n"; done
exit 1
