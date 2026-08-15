import { describe, expect, it, beforeEach } from "vitest";
import { parseSseFrames, frameToBusEvent } from "./sse";
import { bootstrapToken, hasToken, setToken } from "./client";

describe("parseSseFrames", () => {
  it("parses a single data frame", () => {
    const frames = parseSseFrames('data: {"type":"x"}\n\n');
    expect(frames).toHaveLength(1);
    expect(frames[0].data).toBe('{"type":"x"}');
  });

  it("handles multiple frames in one chunk", () => {
    const frames = parseSseFrames('data: {"a":1}\n\ndata: {"b":2}\n\n');
    expect(frames).toHaveLength(2);
    expect(frames[0].data).toBe('{"a":1}');
    expect(frames[1].data).toBe('{"b":2}');
  });

  it("drops keepalive comment lines", () => {
    const frames = parseSseFrames(": keep-alive\n\n");
    expect(frames).toHaveLength(0);
  });

  it("joins multi-line data and keeps the event: field", () => {
    const frames = parseSseFrames("event: session.idle\ndata: one\ndata: two\n\n");
    expect(frames).toHaveLength(1);
    expect(frames[0].eventType).toBe("session.idle");
    expect(frames[0].data).toBe("one\ntwo");
  });

  it("flushes a trailing frame without a blank line", () => {
    const frames = parseSseFrames('data: {"type":"y"}\n');
    expect(frames).toHaveLength(1);
    expect(frames[0].data).toBe('{"type":"y"}');
  });
});

describe("frameToBusEvent", () => {
  it("parses valid event JSON", () => {
    const ev = frameToBusEvent({ eventType: "", data: '{"type":"session.idle"}' });
    expect(ev).not.toBeNull();
    expect(ev?.type).toBe("session.idle");
  });

  it("returns null for malformed data", () => {
    expect(frameToBusEvent({ eventType: "", data: "not json" })).toBeNull();
  });
});

describe("bootstrapToken", () => {
  beforeEach(() => {
    setToken(null);
    // The suite runs in `node` (no jsdom); give bootstrapToken the two
    // globals it touches.
    (globalThis as Record<string, unknown>).window = {
      location: { hash: "", pathname: "/ui/", search: "" },
      history: {
        replaceState: (_state: unknown, _title: string, url: string) => {
          (window as unknown as { location: { hash: string } }).location.hash = "";
          void url;
        },
      },
    };
  });

  it("reads #token from the hash, keeps it in memory, and strips the URL", () => {
    (window as unknown as { location: { hash: string } }).location.hash = "#token=abc123";
    const t = bootstrapToken();
    expect(t).toBe("abc123");
    expect(hasToken()).toBe(true);
    expect((window as unknown as { location: { hash: string } }).location.hash).not.toContain(
      "token=",
    );
  });

  it("returns null when no token is present", () => {
    (window as unknown as { location: { hash: string } }).location.hash = "";
    expect(bootstrapToken()).toBeNull();
    expect(hasToken()).toBe(false);
  });
});
