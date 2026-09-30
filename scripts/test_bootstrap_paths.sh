#!/usr/bin/env bash
# Every repo path the bootstrap installers name must exist.
#
# Why this exists: `bootstrap.sh` was written when install.sh, update.sh and
# the `forge` wrapper sat at the root of a standalone checkout. The monorepo
# moved them under forge-agent/ and the bootstrap was not updated, so
# `chmod +x install.sh update.sh forge` failed on a path that no longer
# existed and `set -euo pipefail` aborted the run before the installer was
# ever invoked. `bash -n` does not catch this — the syntax was fine, the
# filenames were wrong. Nothing else in CI executes these scripts, because
# doing so installs Forge onto the runner.
#
# So: resolve the paths statically and require them to be present.
set -euo pipefail

cd "$(dirname "$0")/.."
status=0

check() {
    if [[ -e "$1" ]]; then
        echo "  ok       $1  ($2)"
    else
        echo "  MISSING  $1  (named by $2)" >&2
        status=1
    fi
}

# The scripts must be at the root, because that is where the published URL
# says they are. Checked first and explicitly: if they are not here, every
# grep below fails on a missing file and the output is a pile of grep errors
# instead of the actual problem.
for script in bootstrap.sh bootstrap.ps1; do
    if [[ ! -e "$script" ]]; then
        echo "MISSING  $script at the repo root." >&2
        echo "         The published one-command install fetches it from" >&2
        echo "         raw.githubusercontent.com/.../main/$script — if it is not" >&2
        echo "         here, that URL 404s for everyone." >&2
        exit 1
    fi
done

# Paths bootstrap.sh chmods and then runs. Read out of the script rather than
# restated here, so the test tracks the script instead of drifting from it.
echo "bootstrap.sh:"
chmod_line="$(grep -m1 '^chmod +x ' bootstrap.sh)"
for path in ${chmod_line#chmod +x }; do
    check "$path" "bootstrap.sh chmod"
done

# The installer it hands off to, and the one bootstrap.ps1 hands off to.
check "$(grep -m1 -o 'bash [^ ]*install\.sh' bootstrap.sh | cut -d' ' -f2)" "bootstrap.sh handoff"
check "install.ps1" "bootstrap.ps1 handoff"

# The scripts advertise their own download URL in a comment. That URL is the
# one published in the README and on the site, and it 404'd for the entire
# time these lived under forge-agent/ — so pin the location to the path the
# URL resolves to.
echo
echo "advertised URLs resolve to a file that exists here:"
for script in bootstrap.sh bootstrap.ps1; do
    url="$(grep -m1 -o 'https://raw.githubusercontent.com/[^ |]*' "$script")"
    # .../<owner>/<repo>/<branch>/<path> — strip the first five segments.
    rel="${url#https://raw.githubusercontent.com/}"
    rel="$(echo "$rel" | cut -d/ -f4-)"
    check "$rel" "$script advertised URL"
done

# The preflight must cover what the installer it hands off to refuses to run
# without. It did not: `forge-agent/install.sh` exits when ripgrep is absent
# and shells out to python3 to write the config, and the bootstrap checked
# neither — so the advertised one-liner cloned the repo and then died inside
# the installer on a fresh machine. Both scripts parse fine; the gap is
# between them, which is why neither `bash -n` nor a path check finds it.
echo
echo "bootstrap preflight covers the installer's hard requirements:"
required="$(grep -oE 'command -v [a-z0-9_]+ +&>/dev/null \|\| MISSING' forge-agent/install.sh \
            | awk '{print $3}' | sort -u)"
# python3 is invoked directly rather than probed, so it is named here.
required="$required
python3"
checked="$(grep -oE 'command -v [a-z0-9_]+' bootstrap.sh | awk '{print $3}' | sort -u)"
for tool in $required; do
    if echo "$checked" | grep -qx "$tool"; then
        echo "  ok       $tool"
    else
        echo "  MISSING  $tool  (forge-agent/install.sh needs it; bootstrap.sh does not check for it)" >&2
        status=1
    fi
done

echo
if [[ $status -eq 0 ]]; then
    echo "All bootstrap paths resolve."
else
    echo "The one-command install is broken: a path does not exist, or the preflight" >&2
    echo "does not cover what the installer it calls requires." >&2
fi
exit $status
