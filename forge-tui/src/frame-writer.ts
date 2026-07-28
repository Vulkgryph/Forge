// SPDX-License-Identifier: Apache-2.0
//
// Groups ink's writes into one synchronized-update frame per tick.
//
// Ink emits a frame as several separate writes, and in one case that frame
// begins by erasing scrollback (`ESC[3J`, part of ansi-escapes' clearTerminal)
// and reprinting the whole transcript from scratch. Ink takes that path
// whenever the rendered height reaches the terminal's row count:
//
//   if (outputHeight >= this.options.stdout.rows)
//       stdout.write(ansiEscapes.clearTerminal + this.fullStaticOutput + output)
//
// Every one of those writes is a frame the terminal can present, so a long
// transcript visibly tears: history is wiped, then restored a moment later. It
// reads as jitter, and the scrollbar jumps as scrollback shrinks and regrows.
//
// The fix is synchronized update (DEC private mode 2026), which tells the
// terminal to keep showing the last complete frame until we say we're done.
// Ink never emits it, so we bracket its writes ourselves.
//
// Ink renders synchronously within a tick, so buffering writes and flushing
// once per tick groups exactly one frame per flush — including the three-write
// `<Static>` path (clear, write static, write output), which no single-write
// wrapper could make atomic. The bytes handed to the terminal are the ones ink
// wrote, concatenated in order, so terminal state evolves identically; only the
// grouping changes.
//
// Terminals that don't implement 2026 parse the mode and ignore it, so this is
// safe to emit unconditionally.

export const SYNC_BEGIN = "\x1b[?2026h";
export const SYNC_END = "\x1b[?2026l";

export type FrameWriter = {
  /** Pass to ink's `render` as its `stdout`. */
  stdout: NodeJS.WriteStream;
  /**
   * Emit any buffered frame immediately. The caller owns registering this for
   * process exit, so a pending frame lands before the cursor is handed back to
   * the shell.
   */
  flush: () => void;
};

export function createFrameWriter(out: NodeJS.WriteStream): FrameWriter {
  // Only meaningful for a terminal. Pipes and CI logs should receive ink's
  // bytes untouched rather than escape sequences nothing will interpret.
  if (!out.isTTY) return { stdout: out, flush: () => {} };

  let pending: string[] = [];
  let scheduled = false;

  const flush = (): void => {
    scheduled = false;
    if (pending.length === 0) return;
    const frame = pending.join("");
    pending = [];
    try {
      out.write(SYNC_BEGIN + frame + SYNC_END);
    } catch {
      /* stdout closed underneath us; nothing useful to do */
    }
  };

  const write = (chunk: unknown, ...rest: unknown[]): boolean => {
    // Buffers and the callback/encoding forms go straight through, after
    // draining what's queued so byte order is preserved. Ink only ever writes
    // plain strings; this is for patched console output and anything else
    // sharing the stream.
    if (typeof chunk !== "string" || rest.length > 0) {
      flush();
      return (out.write as (...args: unknown[]) => boolean)(chunk, ...rest);
    }
    pending.push(chunk);
    if (!scheduled) {
      scheduled = true;
      setImmediate(flush);
    }
    // Ink ignores the backpressure signal, and a buffered write has no real
    // answer for it yet.
    return true;
  };

  const stdout = new Proxy(out, {
    get(target, prop) {
      if (prop === "write") return write;
      // Receiver is the real stream, not the proxy: `rows`/`columns` are
      // getters that must see the live tty, and stream methods reach for
      // internal state that a proxy `this` would hide from them.
      const value = Reflect.get(target, prop, target);
      return typeof value === "function" ? value.bind(target) : value;
    },
  }) as NodeJS.WriteStream;

  return { stdout, flush };
}
