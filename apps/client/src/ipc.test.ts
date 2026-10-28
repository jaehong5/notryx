import { afterEach, describe, expect, it, vi } from "vitest";
import { fromBase64, toBase64 } from "./ipc";

const nativeDecoder = Object.getOwnPropertyDescriptor(Uint8Array, "fromBase64");
afterEach(() => {
  if (nativeDecoder) Object.defineProperty(Uint8Array, "fromBase64", nativeDecoder);
  else Reflect.deleteProperty(Uint8Array, "fromBase64");
});

describe("document byte encoding", () => {
  it("uses the native decoder for valid source bytes", () => {
    const original = new TextEncoder().encode("\ufeff# 한글 😀 e\u0301\n");
    const encoded = "77u/IyDtlZzquIAg8J+YgCBlzIEK";
    const native = vi.fn(() => original);
    Object.defineProperty(Uint8Array, "fromBase64", { configurable: true, value: native });
    expect(fromBase64(encoded)).toEqual(original);
    expect(native).toHaveBeenCalledExactlyOnceWith(encoded);
  });

  it.each([16385, 5 * 1024 * 1024])("preserves all bytes in a %i-byte native decode", (length) => {
    const original = Uint8Array.from({ length }, (_, index) => index % 256);
    const encoded = toBase64(original);
    const native = vi.fn((value: string) => {
      const binary = atob(value);
      const bytes = new Uint8Array(binary.length);
      for (let index = 0; index < binary.length; index++) bytes[index] = binary.charCodeAt(index);
      return bytes;
    });
    Object.defineProperty(Uint8Array, "fromBase64", { configurable: true, value: native });
    const decoded = fromBase64(encoded);
    expect(decoded.length).toBe(original.length);
    expect(decoded.every((value, index) => value === original[index])).toBe(true);
    expect(native).toHaveBeenCalledExactlyOnceWith(encoded);
  });

  it.each(["TQ", "TWE", "TR", " \t\n\r\fT W E = ", " \t\n\r\f"])(
    "preserves unpadded and whitespace input %j with either decoder",
    (encoded) => {
      const expected = Uint8Array.from(atob(encoded), (character) => character.charCodeAt(0));
      const native = vi.fn((value: string) => Uint8Array.from(atob(value), (character) => character.charCodeAt(0)));
      Object.defineProperty(Uint8Array, "fromBase64", { configurable: true, value: native });
      expect(fromBase64(encoded)).toEqual(expected);
      expect(native).toHaveBeenCalledExactlyOnceWith(encoded);
      Object.defineProperty(Uint8Array, "fromBase64", { configurable: true, value: undefined });
      expect(fromBase64(encoded)).toEqual(expected);
    },
  );

  it("falls back to atob when the native decoder rejects input", () => {
    const native = vi.fn(() => { throw new SyntaxError("native error"); });
    Object.defineProperty(Uint8Array, "fromBase64", { configurable: true, value: native });
    expect(fromBase64("TQ")).toEqual(Uint8Array.of(77));
    let expected: unknown;
    try { atob("TQ="); } catch (cause) { expected = cause; }
    expect(expected).toBeDefined();
    expect(() => fromBase64("TQ=")).toThrow(expected as Error);
    expect(native).toHaveBeenCalledTimes(2);
  });

  it("retains the 25 MiB decoded size boundary on the native path", () => {
    const maximum = 25 * 1024 * 1024;
    const native = vi.fn()
      .mockReturnValueOnce(new Uint8Array(maximum))
      .mockReturnValueOnce(new Uint8Array(maximum + 1));
    Object.defineProperty(Uint8Array, "fromBase64", { configurable: true, value: native });
    const prefix = "A".repeat(Math.floor(maximum / 3) * 4);
    expect(fromBase64(prefix + "AA==").length).toBe(maximum);
    expect(() => fromBase64(prefix + "AAA=")).toThrow("문서가 너무 큽니다.");
    expect(() => fromBase64(prefix + "AAAAAAAA")).toThrow("문서가 너무 큽니다.");
    expect(native).toHaveBeenCalledTimes(2);
  });

  it("uses the available native encoder for a byte-array view", () => {
    const bytes = Uint8Array.of(255, 0, 251, 255, 42).subarray(1, 4);
    const native = vi.fn(function (this: Uint8Array) {
      expect(this).toBe(bytes);
      return "APv/";
    });
    Object.defineProperty(bytes, "toBase64", { value: native });
    expect(toBase64(bytes)).toBe("APv/");
    expect(native).toHaveBeenCalledOnce();
  });

  it.each([0, 1, 2, 3, 8191, 8192, 8193, 5 * 1024 * 1024])(
    "preserves %i bytes with and without the native encoder",
    (length) => {
      const backing = Uint8Array.from({ length: length + 2 }, (_, index) => (index + 2) % 3);
      const bytes = backing.subarray(1, length + 1);
      const expected = "AAEC".repeat(Math.floor(length / 3)) + ["", "AA==", "AAE="][length % 3];
      Object.defineProperty(bytes, "toBase64", { value: undefined, configurable: true });
      expect(toBase64(bytes)).toBe(expected);
      const native = vi.fn(function (this: Uint8Array) { expect(this).toBe(bytes); return expected; });
      Object.defineProperty(bytes, "toBase64", { value: native });
      const encoded = toBase64(bytes);
      expect(encoded).toBe(expected);
      const decoded = fromBase64(encoded);
      expect(decoded.length).toBe(bytes.length);
      expect(decoded.every((value, index) => value === bytes[index])).toBe(true);
      if (length > 0) expect(native).toHaveBeenCalledOnce();
    },
  );

  it("preserves BOM and multilingual UTF-8 bytes with the native encoder", () => {
    const bytes = new TextEncoder().encode("\ufeff# 한글 😀 e\u0301\n");
    const native = vi.fn(function (this: Uint8Array) { expect(this).toBe(bytes); return "77u/IyDtlZzquIAg8J+YgCBlzIEK"; });
    Object.defineProperty(bytes, "toBase64", { value: native });
    expect(fromBase64(toBase64(bytes))).toEqual(bytes);
    expect(native).toHaveBeenCalledOnce();
  });

  it("preserves empty detached arrays and propagates native encoder failures", () => {
    const detached = Uint8Array.of(1);
    Object.defineProperty(detached, "toBase64", { value: () => { throw new Error("detached"); } });
    structuredClone(detached.buffer, { transfer: [detached.buffer] });
    expect(toBase64(detached)).toBe("");
    const bytes = Uint8Array.of(1);
    Object.defineProperty(bytes, "toBase64", { value: () => { throw new Error("encoding failed"); } });
    expect(() => toBase64(bytes)).toThrow("encoding failed");
  });

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
