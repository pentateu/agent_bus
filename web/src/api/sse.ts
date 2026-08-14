// A fetch-stream SSE reader (§2.1, decision 3). `EventSource` cannot set the
// `Authorization` header, so we hand-roll the parser (~60 lines) over `fetch`
// with the in-memory bearer token.

import { hasToken } from "./client";
import type { BusEvent } from "./types";

/** Parse one SSE frame (`event:` + joined `data:` lines) from a chunk. */
export interface SseFrame {
  eventType: string;
  data: string;
}

export function parseSseFrames(input: string): SseFrame[] {
  const frames: SseFrame[] = [];
  let eventType = "";
  const data: string[] = [];
  let sawField = false;
  for (const line of input.split("\n")) {
    if (line === "") {
      if (sawField) {
        frames.push({ eventType, data: data.join("\n") });
        eventType = "";
        data.length = 0;
        sawField = false;
      }
      continue;
    }
    if (line.startsWith(":")) continue;
    if (line.startsWith("event:")) {
      eventType = line.slice(6).trim();
      sawField = true;
    } else if (line.startsWith("data:")) {
      data.push(line.slice(5).trim());
      sawField = true;
    }
  }
  if (sawField) frames.push({ eventType, data: data.join("\n") });
  return frames;
}

/** Convert a frame to a BusEvent (the daemon sends the event JSON in `data`). */
export function frameToBusEvent(frame: SseFrame): BusEvent | null {
  try {
    return JSON.parse(frame.data) as BusEvent;
  } catch {
    return null;
  }
}

/**
 * Stream SSE events from `/api/v1/events` as an async iterator, reconnecting
 * with backoff on error/EOF. Callers `break` out of the loop to stop.
 */
export async function* streamEvents(): AsyncGenerator<BusEvent> {
  if (!hasToken()) return;
  let backoff = 1000;
  for (;;) {
    try {
      const res = await fetch("/api/v1/events", {
        headers: { Accept: "text/event-stream" },
      });
      if (!res.ok || !res.body) throw new Error(`events ${res.status}`);
      backoff = 1000;
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let idx = buffer.lastIndexOf("\n\n");
        if (idx < 0) continue;
        const chunk = buffer.slice(0, idx);
        buffer = buffer.slice(idx + 2);
        for (const frame of parseSseFrames(chunk)) {
          const event = frameToBusEvent(frame);
          if (event) yield event;
        }
      }
    } catch {
      // fall through to reconnect
    }
    await new Promise((r) => setTimeout(r, backoff));
    backoff = Math.min(backoff * 2, 30_000);
  }
}
