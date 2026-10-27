import { describe, expect, it } from "vitest";
import { fromBase64, toBase64 } from "./ipc";

describe("document byte encoding", () => {
  it("round-trips every byte value across chunk boundaries", () => {
    const original = Uint8Array.from({ length: 16385 }, (_, index) => index % 256);
    expect(fromBase64(toBase64(original))).toEqual(original);
    expect(fromBase64("")).toEqual(new Uint8Array());
  });

  it("rejects invalid and oversized input", () => {
    expect(() => fromBase64("invalid!")).toThrow();
    expect(() => fromBase64("A".repeat(Math.ceil(25 * 1024 * 1024 / 3) * 4 + 4)))
      .toThrow("문서가 너무 큽니다.");
  });
});
