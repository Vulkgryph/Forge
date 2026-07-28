// SPDX-License-Identifier: Apache-2.0
//
// Integration test: drives real ink through the branch that erases scrollback,
// and asserts our writer contains it in a synchronized update. The unit tests
// cover the writer in isolation; this one proves it's wired to the path that
// actually caused the jitter.
import { describe, expect, test } from "bun:test";
import React from "react";
import { Box, Text, render } from "ink";
import { EventEmitter } from "node:events";

import { createFrameWriter, SYNC_BEGIN, SYNC_END } from "./frame-writer.js";

const ERASE_SCROLLBACK = "\x1b[3J";

/** A tty-shaped stdout with a small row count, recording every write. */
function recordingTty(rows: number) {
  const writes: string[] = [];
  const stream = Object.assign(new EventEmitter(), {
    isTTY: true,
    rows,
    columns: 40,
    write(chunk: unknown): boolean {
      writes.push(String(chunk));
      return true;
    },
  });
  return { stream: stream as unknown as NodeJS.WriteStream, writes };
}

/** Non-tty stdin, so ink skips raw mode. */
function fakeStdin() {
  return Object.assign(new EventEmitter(), {
    isTTY: false,
    setEncoding() {},
    read: () => null,
    setRawMode() {},
    resume() {},
    pause() {},
    ref() {},
    unref() {},
  }) as unknown as NodeJS.ReadStream;
}

const tick = () => new Promise<void>((resolve) => setImmediate(resolve));

describe("frame writer with real ink", () => {
  test("ink's scrollback-erasing repaint is wrapped in a synchronized update", async () => {
    // 4 rows, 12 lines of content: forces outputHeight >= rows, which is the
    // branch that writes clearTerminal (and therefore ESC[3J).
    const { stream, writes } = recordingTty(4);
    const { stdout, flush } = createFrameWriter(stream);

    const lines = Array.from({ length: 12 }, (_, i) => (
      <Text key={i}>line {i}</Text>
    ));
    const app = render(<Box flexDirection="column">{lines}</Box>, {
      stdout,
      stdin: fakeStdin(),
      patchConsole: false,
    });

    await tick();
    flush();
    app.unmount();
    app.clear();

    const all = writes.join("");

    // Precondition: we actually exercised the destructive branch. If ink stops
    // taking it, this test is no longer testing what it claims to.
    expect(all).toContain(ERASE_SCROLLBACK);

    // Every scrollback erase must sit inside a begin/end pair, so the terminal
    // never presents the wiped intermediate state.
    for (const write of writes) {
      if (!write.includes(ERASE_SCROLLBACK)) continue;
      expect(write.startsWith(SYNC_BEGIN)).toBe(true);
      expect(write.endsWith(SYNC_END)).toBe(true);
    }
  });

  test("ink still receives correct row and column counts through the proxy", async () => {
    const { stream } = recordingTty(30);
    const { stdout, flush } = createFrameWriter(stream);

    let seen: { rows?: number; columns?: number } = {};
    const Probe = () => {
      seen = { rows: stdout.rows, columns: stdout.columns };
      return <Text>probe</Text>;
    };
    const app = render(<Probe />, {
      stdout,
      stdin: fakeStdin(),
      patchConsole: false,
    });

    await tick();
    flush();
    app.unmount();

    expect(seen).toEqual({ rows: 30, columns: 40 });
  });
});
