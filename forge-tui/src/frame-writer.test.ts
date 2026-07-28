// SPDX-License-Identifier: Apache-2.0
import { describe, expect, test } from "bun:test";

import { createFrameWriter, SYNC_BEGIN, SYNC_END } from "./frame-writer.js";

/** Minimal stand-in for a tty write stream, recording what reaches it. */
function fakeTty(opts: { isTTY?: boolean } = {}) {
  const writes: string[] = [];
  const stream = {
    isTTY: opts.isTTY ?? true,
    rows: 24,
    columns: 80,
    write(chunk: unknown): boolean {
      writes.push(String(chunk));
      return true;
    },
  };
  return { stream: stream as unknown as NodeJS.WriteStream, writes };
}

const tick = () => new Promise<void>((resolve) => setImmediate(resolve));

/** The sequence that erases scrollback — ansi-escapes' clearTerminal. */
const CLEAR_TERMINAL = "\x1b[2J\x1b[3J\x1b[H";

describe("createFrameWriter", () => {
  test("wraps a scrollback-erasing repaint in a synchronized update", async () => {
    const { stream, writes } = fakeTty();
    const { stdout } = createFrameWriter(stream);

    stdout.write(CLEAR_TERMINAL + "transcript");
    await tick();

    expect(writes).toEqual([SYNC_BEGIN + CLEAR_TERMINAL + "transcript" + SYNC_END]);
  });

  test("coalesces a multi-write frame into one synchronized update", async () => {
    const { stream, writes } = fakeTty();
    const { stdout } = createFrameWriter(stream);

    // Ink's <Static> path: clear, write static output, write main output.
    stdout.write("\x1b[2K");
    stdout.write("static");
    stdout.write("main");
    await tick();

    // One frame, not three — otherwise the terminal could present between them.
    expect(writes).toHaveLength(1);
    expect(writes[0]).toBe(SYNC_BEGIN + "\x1b[2Kstaticmain" + SYNC_END);
  });

  test("preserves ink's byte stream exactly, minus the brackets", async () => {
    const { stream, writes } = fakeTty();
    const { stdout } = createFrameWriter(stream);

    const chunks = [CLEAR_TERMINAL, "line one\n", "line two\n", "\x1b[3A"];
    for (const c of chunks) stdout.write(c);
    await tick();

    const inner = writes.join("").slice(SYNC_BEGIN.length, -SYNC_END.length);
    expect(inner).toBe(chunks.join(""));
  });

  test("separate ticks produce separate frames", async () => {
    const { stream, writes } = fakeTty();
    const { stdout } = createFrameWriter(stream);

    stdout.write("first");
    await tick();
    stdout.write("second");
    await tick();

    expect(writes).toEqual([
      SYNC_BEGIN + "first" + SYNC_END,
      SYNC_BEGIN + "second" + SYNC_END,
    ]);
  });

  test("emits nothing when no writes occurred", async () => {
    const { stream, writes } = fakeTty();
    createFrameWriter(stream);
    await tick();
    expect(writes).toEqual([]);
  });

  test("flush emits synchronously, for the exit path", () => {
    const { stream, writes } = fakeTty();
    const { stdout, flush } = createFrameWriter(stream);

    stdout.write("final frame");
    expect(writes).toEqual([]); // still buffered
    flush();
    expect(writes).toEqual([SYNC_BEGIN + "final frame" + SYNC_END]);
  });

  test("flush is idempotent and drops nothing", () => {
    const { stream, writes } = fakeTty();
    const { stdout, flush } = createFrameWriter(stream);

    stdout.write("x");
    flush();
    flush();
    expect(writes).toEqual([SYNC_BEGIN + "x" + SYNC_END]);
  });

  test("passes rows and columns through to the real stream", () => {
    const { stream } = fakeTty();
    const { stdout } = createFrameWriter(stream);

    expect(stdout.rows).toBe(24);
    expect(stdout.columns).toBe(80);
  });

  test("reflects live resizes rather than a snapshot", () => {
    const { stream } = fakeTty();
    const { stdout } = createFrameWriter(stream);

    (stream as unknown as { rows: number }).rows = 50;
    expect(stdout.rows).toBe(50);
  });

  test("leaves a non-tty stream untouched", async () => {
    const { stream, writes } = fakeTty({ isTTY: false });
    const { stdout } = createFrameWriter(stream);

    stdout.write("piped output");
    await tick();

    expect(writes).toEqual(["piped output"]); // no escapes added
  });

  test("non-string chunks bypass buffering without reordering", () => {
    const { stream, writes } = fakeTty();
    const { stdout } = createFrameWriter(stream);

    stdout.write("before");
    stdout.write(Buffer.from("raw"));

    // The buffered string must land first, then the passthrough.
    expect(writes).toEqual([SYNC_BEGIN + "before" + SYNC_END, "raw"]);
  });

  test("a write with a callback bypasses buffering and still fires", () => {
    const { stream } = fakeTty();
    let called = false;
    // Accept the callback form on the underlying stream.
    (stream as unknown as { write: (c: unknown, cb?: () => void) => boolean }).write = (
      _c,
      cb,
    ) => {
      cb?.();
      return true;
    };
    const { stdout } = createFrameWriter(stream);

    stdout.write("done", () => {
      called = true;
    });
    expect(called).toBe(true);
  });
});
