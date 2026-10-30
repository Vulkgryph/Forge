# Releasing

Written for whoever cuts the release, which is usually an agent. Follow it in
order. Every step that can be checked by a machine is in `scripts/preflight.sh`;
the steps that cannot are the ones that have leaked, so they are written out
here in full.

## What keeps leaking, and why

Three identities have left this repository. Every one got past a check that was
looking somewhere else — a check on prose missed a machine path in a test, and
nothing had ever read a commit message or a built binary. Those holes are closed
now and `check_private.py` covers the tracked tree, commit messages, and the
artifacts before they are signed.

**None of that reads a release note.** The announcement, the release body, the
screenshots and the reply to whoever asks about it are written by hand, published
straight to strangers, and read by no guard at all. That is the surface this
document exists for.

The working rule: *a release note is written from the changelog, not from the
session that produced it.* Everything an agent knows about how a fix was found —
the machine it was found on, the host it was reproduced against, the conversation
that surfaced it — is exactly the material that leaks, and none of it belongs in
what gets posted.

## Before anything

```sh
./scripts/preflight.sh            # every mechanical gate, in one pass
```

It must exit 0. It checks the tracked tree for names and home directories,
licence attribution in NOTICE, documented versions, dead documentation links,
the installers' syntax and paths, and that the working tree is clean. It cannot
check what you are about to write; that is the rest of this file.

## 1. Decide the number

Semver, and 0.x still means something here:

- **Patch** (0.6.1 → 0.6.2) — fixes only. Nothing gained a capability it did not
  have.
- **Minor** (0.6.x → 0.7.0) — anything new a person can *use*: a command, a
  setting, a surface that did not exist. A behaviour change big enough that
  someone would notice without being told is also a minor.

If in doubt, read the changelog section as a stranger. If a line makes you think
"that's new", it is a minor.

**Never move a tag that is already on the remote.** Check first:

```sh
git ls-remote --tags origin | grep "v<version>"
```

If it is there, that number is spent, even if the release was only ever a draft.
Pick the next one. A tag that points at different code than it did yesterday is
how people stop trusting a version number, and this project is small enough that
it would be noticed.

## 2. Versions and changelog

Bump in lockstep:

- `forge-agent/Cargo.toml`
- `forge-ide/Cargo.toml`
- `forge-tui-rs/Cargo.toml`

Leave alone unless their own contents changed:

- `forge-ide/forge-server` — its version is the marker that forces a re-upload
  of the binary to **every remote host** someone has connected to. Bumping it
  without a reason costs them all a 1.2 MB transfer on next connect.
- `forge-ide/forge-proto`, `forge-agent-proto`, `forge-search` — internal, and
  not announced.

Never write a version as a literal in documentation or a user agent. CI fails on
`forge-(search|agent|ide|tui)/N.N.N` outside a changelog; build it from
`CARGO_PKG_VERSION` or write `<version>`.

Then give the changelog a section for the new version, dated, with the entries
for what actually ships in it. Entries for a version already released stay where
they are and are not edited — history is not a draft.

## 3. Write the release notes

**Source: the changelog section for this version. Nothing else.**

Each entry already says what broke, what it cost, and what changed, in a register
suitable for a stranger. Reuse that. If an entry cannot be published as-is, the
changelog entry is wrong and should be fixed there, so the next release inherits
the fix.

### Filter — refuse to publish any of these

| Class | Examples that have come up |
|---|---|
| Home directories | `/Users/<name>`, `C:\Users\<name>`, `/home/<name>` |
| Machine and host names | a laptop's name, `admin-1`, a `*.local` mDNS name |
| Addresses | LAN `10.*` / `192.168.*`, tailnet `100.*`, any bare IP |
| Account names | the account a build ran under, an SSH user |
| Private infrastructure | GPU hosts, model endpoints and ports, internal tunnels |
| People | real names, email addresses, anyone's handle but the project's |
| Registration details | the LLC's filing details, WHOIS contact, postal address |
| Other people's work | repository names and paths from unrelated projects |

Screenshots and terminal captures are the usual offenders, because the leak is
in the chrome rather than the subject: a window title, a path in a tab, a
hostname in a prompt, a file tree down the left. Crop to the thing being shown,
then look at what is left before attaching it.

The same filter applies to **anything you publish anywhere** — an issue reply, a
commit message, a comment on someone else's tracker, an email to whoever wrote
in. The release note is simply where it is easiest to forget.

### Say

- What changed, from the user's side, in their words not the code's.
- What it cost them before, when that is what makes the fix make sense.
- Known gaps that affect whether to upgrade, stated plainly.

### Do not say

- How it was found, on what machine, against which host, in what conversation.
- What an agent did, which model wrote it, or how long it took.
- Anything about a person: not the reporter, not the author, not a customer.
- Speculation about anyone else's software by name.

## 4. Credit

One contributor identity, and it is already configured — **use it rather
than passing a name on the command line**:

```sh
git config user.name        # Vulkgryph LLC
git -c user.name=something commit   # never this
```

0.7.0 was prepared with 26 of 82 commits authored as `windingcreek` because
an override was passed on every commit while the repository was configured
as `Vulkgryph LLC`. Same email, two names, in one public history. That is
not a leak on its own, but `windingcreek` owns private repositories
elsewhere, and two names on one email is a correlation nobody asked for.

Copyright is **Vulkgryph LLC**; see `NOTICE` and `LICENSE`. Release notes are
written in the project's voice — "Forge now…", not "I…" or "we fixed…". Nobody
is thanked by real name without asking them first, and an agent never signs its
own work: a commit trailer naming a model is a credit line nobody agreed to.

Third-party licences are attributed in `NOTICE`, which `preflight.sh` checks
against the dependency graph as it is *today*, not as it was when NOTICE was
written.

## 5. Build it on Windows

Not optional, and not covered by CI. `forge-ide` needs **NASM** on Windows —
`aws-lc-sys` fails outright without it — and GitHub's runners ship NASM
preinstalled, so the pipeline stays green on a machine that is not like the
one a person installs onto. Promoting the editor's Windows job from
`continue-on-error` would not have caught it.

A real Windows machine found that, and found a test module calling `cfg(unix)`
code while gated only on `test`. Both were invisible to every green build that
preceded them.

Carry the commits over without a public push:

```sh
git bundle create /tmp/f.bundle ^origin/main refs/heads/main
scp /tmp/f.bundle <host>:                       # then, on the box:
git fetch .../f.bundle refs/heads/main:refs/heads/wintest && git checkout wintest
cargo test --locked -p forge-agent-proto -p forge-search -p forge-tui-rs
cargo build -p forge-ide -p forge-proto -p forge-server
```

Name the ref in the bundle. A bundle built from a revision range alone carries
only `HEAD`, and the fetch then matches nothing and fails silently.

## 6. Build and sign

```sh
bash forge-ide/scripts/package_macos.sh
```

It builds outside `$HOME` so build scripts cannot bake a path naming the builder
into the binary, remaps the paths that would otherwise carry one, and runs
`check_private.py --artifacts` **before signing**, failing the build rather than
producing a signed leak. Sign with the Developer ID — an ad-hoc signature
invalidates every macOS folder permission a user has granted.

Then confirm the bundle holds what it claims: both binaries present and
executable, `Info.plist` in place, and the version it reports matching the tag.

**That is not the artifact.** `package_macos.sh` produces a `.app`; a release
ships a notarized `.dmg`, which is a second script:

```sh
bash forge-ide/scripts/sign_notarize_dmg.sh
```

It signs with the Developer ID, submits to Apple, waits for the ticket,
staples it, and writes `target/dist/Forge-IDE-<version>.dmg` — named from the
manifest, because an asset called `Forge IDE.dmg` tells nobody which build
they downloaded. It needs notarization credentials stored once:

```sh
xcrun notarytool store-credentials "forge-notary" --apple-id <email> --team-id <team>
```

The prompt wants an app-specific password from appleid.apple.com, not the
account password; notarytool refuses the latter with a 401.

It works on a copy of the bundle on purpose. Signing rewrites the
executables inside, and macOS checks each page of a running process against
the signature recorded when the file was mapped — re-signing a bundle a
window is running from kills that window with `CODESIGNING, Code 2, Invalid
Page`. That has happened here. Running this with the IDE open is safe.

Without the `.dmg` there is nothing to attach. A signed `.app` in
`target/dist` is a local build, not a release, and the two are easy to
confuse because the signing step is loud and the packaging step is a
different file.

## 7. Publish

Pushing the tag starts `.github/workflows/release.yml`, which re-runs every
mechanical gate on the exact tagged commit, checks the tag against the version
the crates declare and that both changelogs have a dated section for it, proves
the bundle still packages, and opens a **draft** release with a body built from
the changelog.

It signs nothing and holds no secrets. The Developer ID stays on the machine
that owns it: anything able to trigger a workflow could otherwise sign code as
Vulkgryph, and a signature cannot be rotated quietly — a re-issued one
invalidates every macOS folder permission a user has already granted.

So, after the workflow is green:

1. Tag, annotated, and push the tag. Wait for the workflow.
2. Build, sign and notarise locally — step 6 — and attach the bundle to the
   draft.
3. Read the draft body against the filter table above. The workflow wrote it
   from the changelog, which is reviewed, but no workflow can tell whether a
   sentence gives away a machine, a host, or a person.
4. Supersede any older draft for a version that never shipped — delete it rather
   than leaving two entries for a number that only ever meant one thing.
5. Read the published page as a stranger would, with the filter table open. This
   is the last point at which a leak costs nothing to fix.

## 8. After

Anything published is public permanently, whatever the platform says about
deletion — assume it was indexed and archived within the minute. Removing a leak
afterwards is damage control, never a remedy.

If something does get out, say so here, and add a check that would have caught
it. Three of these were found by a person reading carefully, which does not
scale and did not catch the fourth.
