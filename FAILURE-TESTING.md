# Windows and Linux failure testing (2026-09-27, `cf6652f` / v0.5.2)

Tested native Windows x86-64 and Ubuntu 24.04 x86-64 under WSL2. Linux work used a separate checkout; the existing interactive Linux workspace was not modified. All model traffic used loopback fixtures and dummy credentials.

## Results

- 44 agent fault scenarios passed: 11 cases, two operating systems, OpenAI-compatible and Anthropic streaming formats. Cases: incomplete EOF, broken chunked transport, output limit, truncated tool arguments, failed file tool, HTTP 503 with retry, HTTP 429, context-window rejection, stalled stream, cancellation, and forcibly killing/resuming the agent. Every case also sends a successful subsequent turn. Partial answers are checked for exactly-once preservation after streaming failures.
- Responses API streaming has an additional loopback test for premature EOF, explicit incomplete response, and explicit successful completion on both operating systems. No real OAuth account was used.
- Native IDE SSH integration passed on both platforms against a dedicated loopback OpenSSH server: unknown-host trust, trusted reconnect, portable helper upload, file read/write/list/errors, SFTP upload, remote PTY, model reverse forwarding, output-limit error, forced socket disconnect, rejecting writes while disconnected, pending request release, and remote session resume.
- The reconnect banner is rendered and clicked through egui input on both platforms. Existing frontend/window restart and whole-process/session restoration regression tests also pass.
- Linux workspace regression suite: 1,588 passed, 16 ignored. Windows IDE: 414 passed, 9 ignored. Windows agent: 224 passed, 5 ignored. Ignored tests require external services or explicitly configured hosts; the new ignored SSH integration test was run separately.

  **These counts are from `cf6652f` (v0.5.2), the tree this run measured, and have not been re-taken since.** The suite has grown: 1,671 test functions at v0.5.2, 1,720 at v0.6.0, including the SSRF, `robots.txt` and Web Bot Auth tests that 0.6.0's security work rests on. A re-run at 0.6.0 would report a higher number, not these. Settling it needs `cargo test --workspace` on Ubuntu 24.04 x86-64 and `cargo test -p forge-ide` / `-p forge-agent` on native Windows, at a named commit — the commit being the part this document originally left out.

## Repairs

Streams now require a completion signal and report output truncation as an error. Incomplete tool arguments are not executed. Partial text survives an error and session resume. Transient server failures participate in bounded retries. Missing-file reads report tool failure.

SSH tracks disconnection, releases pending requests, removes timed-out request entries, uses keepalives, and presents Reconnect while retaining the remote workspace and pending conversation. A failed reconnect cannot silently move remote work onto the local machine.

Windows stale agent locks now check native process liveness. Shell timeout cleanup terminates the child process tree: the regression originally took 120 seconds despite a two-second timeout, and the test now holds it under a 15-second ceiling. Windows test fixtures also account for native path separators and Git line-ending settings; the Unix shell-script fixture is Unix-only.

## Reproduce

The provider fault matrix is part of the test suite:

```sh
cargo test -p forge-agent --test resilience
```

Eleven cases against both the OpenAI-compatible and Anthropic streaming
formats, each on its own loopback provider and its own HOME, run in parallel.
The target reports 23 tests, not 22: `a_message_split_across_writes_is_not_lost`
lives there too, guarding the headless reader against losing a message that
arrives in pieces.
It was a standalone Python harness first; as a `cargo test` it runs wherever
the suite runs instead of having to be remembered. Every run writes `events.json` and
`requests.json` into the case's own fixture home; on a failure the panic prints
that directory, along with what the provider saw and the agent's stderr.

SSH integration requires a disposable account named `forgefault`, loopback sshd on port 22282, key authentication, SFTP, and TCP forwarding. Never point this fixture at a personal account: it writes the account's Forge configuration. Run `python3 scripts/ssh_fault_proxy.py --marker PATH` to provide the disconnecting relay on 22283 and the mock model on 22284 — it is not marked executable, so it needs the interpreter named. Clear the recorded requests with GET `http://127.0.0.1:22284/reset` before each test; that endpoint resets the request log only, and the provider keeps truncating the first response after every reset, which is the behaviour these tests want.

Build portable x86-64 Linux helpers with `bash scripts/build_remote_tools.sh` (requires Rust and `musl-gcc` on `PATH`). The `musl-gcc` name is what the script checks for, which in practice means Ubuntu/Debian `musl-tools`: a macOS `musl-cross` install provides `x86_64-linux-musl-gcc` instead and the script will refuse. Set `FORGE_REMOTE_TOOLS_DIR` to that output directory. The IDE also looks for `remote/forge-agent-x86_64` and `remote/forge-server-x86_64` beside its executable. Existing Windows/Linux source installers do not automatically build or bundle this optional helper pack; provide it for SSH operation.

Invoke the compiled IDE test executable with `--ignored --nocapture ssh::failure_tests::live_ssh_failure_roundtrip`. Set HOME (and USERPROFILE on Windows), FORGE_TEST_HOME, FORGE_TEST_SSH_KEY, FORGE_REMOTE_TOOLS_DIR, and FORGE_TEST_DROP_FILE. Use a fresh fixture home for the initial unknown-host check. The test refuses to proceed if the effective home is not the supplied fixture home.

## Scope and limits

This is controlled failure injection, not a physical network outage or whole-machine power cut. Agent process kill/resume was exercised; an unsaved partial token at an abrupt kill is not guaranteed durable. Mid-write filesystem crash consistency, interrupted helper installation, silent network blackholes, arbitrary provider quirks, and ARM targets are not certified by this matrix. GUI reconnect was tested with egui input and SSH with the real IDE transport; this was not a manual visual test of every remote editing workflow.

Tested binaries were built in the development checkouts. Existing installed application generations were not replaced by this test run. SSH helper artifacts and source history are preserved alongside the evidence backup; private fixture keys are excluded.