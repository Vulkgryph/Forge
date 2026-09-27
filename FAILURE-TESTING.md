# Windows and Linux failure testing (2026-09-27)

Tested native Windows x86-64 and Ubuntu 24.04 x86-64 under WSL2. Linux work used a separate checkout; the existing interactive Linux workspace was not modified. All model traffic used loopback fixtures and dummy credentials.

## Results

- 44 agent fault scenarios passed: 11 cases, two operating systems, OpenAI-compatible and Anthropic streaming formats. Cases: incomplete EOF, broken chunked transport, output limit, truncated tool arguments, failed file tool, HTTP 503 with retry, HTTP 429, context-window rejection, stalled stream, cancellation, and forcibly killing/resuming the agent. Every case also sends a successful subsequent turn. Partial answers are checked for exactly-once preservation after streaming failures.
- Responses API streaming has an additional loopback test for premature EOF, explicit incomplete response, and explicit successful completion on both operating systems. No real OAuth account was used.
- Native IDE SSH integration passed on both platforms against a dedicated loopback OpenSSH server: unknown-host trust, trusted reconnect, portable helper upload, file read/write/list/errors, SFTP upload, remote PTY, model reverse forwarding, output-limit error, forced socket disconnect, rejecting writes while disconnected, pending request release, and remote session resume.
- The reconnect banner is rendered and clicked through egui input on both platforms. Existing frontend/window restart and whole-process/session restoration regression tests also pass.
- Linux workspace regression suite: 1,588 passed, 16 ignored. Windows IDE: 414 passed, 9 ignored. Windows agent: 224 passed, 5 ignored. Ignored tests require external services or explicitly configured hosts; the new ignored SSH integration test was run separately.

## Repairs

Streams now require a completion signal and report output truncation as an error. Incomplete tool arguments are not executed. Partial text survives an error and session resume. Transient server failures participate in bounded retries. Missing-file reads report tool failure.

SSH tracks disconnection, releases pending requests, removes timed-out request entries, uses keepalives, and presents Reconnect while retaining the remote workspace and pending conversation. A failed reconnect cannot silently move remote work onto the local machine.

Windows stale agent locks now check native process liveness. Shell timeout cleanup terminates the child process tree: the regression originally took 120 seconds despite a two-second timeout, and now returns promptly. Windows test fixtures also account for native path separators and Git line-ending settings; the Unix shell-script fixture is Unix-only.

## Reproduce

Build `forge-agent` and run (Python 3 standard library only; use a new output directory for each run):

```sh
python scripts/test_resilience.py --agent target/debug/forge-agent --output /tmp/forge-fault-openai
python scripts/test_resilience.py --agent target/debug/forge-agent --protocol anthropic --output /tmp/forge-fault-anthropic
```

On Windows use `target/debug/forge-agent.exe` and a Windows output path. `FORGE_CONFIG_FILE` explicitly isolates agent configuration; the harness checks the fixture model identity before sending anything.

SSH integration requires a disposable account named `forgefault`, loopback sshd on port 22282, key authentication, SFTP, and TCP forwarding. Never point this fixture at a personal account: it writes the account's Forge configuration. Run `scripts/ssh_fault_proxy.py --marker PATH` to provide the disconnecting relay on 22283 and mock model on 22284. Reset the provider with GET `http://127.0.0.1:22284/reset` before each test.

Build portable x86-64 Linux helpers with `bash scripts/build_remote_tools.sh` (requires musl-tools and Rust). Set `FORGE_REMOTE_TOOLS_DIR` to that output directory. The IDE also looks for `remote/forge-agent-x86_64` and `remote/forge-server-x86_64` beside its executable. Existing Windows/Linux source installers do not automatically build or bundle this optional helper pack; provide it for SSH operation.

Invoke the compiled IDE test executable with `--ignored --nocapture ssh::failure_tests::live_ssh_failure_roundtrip`. Set HOME (and USERPROFILE on Windows), FORGE_TEST_HOME, FORGE_TEST_SSH_KEY, FORGE_REMOTE_TOOLS_DIR, and FORGE_TEST_DROP_FILE. Use a fresh fixture home for the initial unknown-host check. The test refuses to proceed if the effective home is not the supplied fixture home.

## Scope and limits

This is controlled failure injection, not a physical network outage or whole-machine power cut. Agent process kill/resume was exercised; an unsaved partial token at an abrupt kill is not guaranteed durable. Mid-write filesystem crash consistency, interrupted helper installation, silent network blackholes, arbitrary provider quirks, and ARM targets are not certified by this matrix. GUI reconnect was tested with egui input and SSH with the real IDE transport; this was not a manual visual test of every remote editing workflow.

Tested binaries were built in the development checkouts. Existing installed application generations were not replaced by this test run. SSH helper artifacts and source history are preserved alongside the evidence backup; private fixture keys are excluded.