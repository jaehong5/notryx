import { expect, test } from "vitest";
import { Draft } from "./draft";
import { openSource } from "./source";

const source = () => openSource(new TextEncoder().encode("base"));

test("a late local reply cannot mark newer input durable", async () => {
  const saved: string[] = [];
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => { release = resolve; });
  const draft = new Draft(source(), "session", "0", async (generation, bytes) => {
    saved.push(new TextDecoder().decode(bytes));
    if (generation === "1") await blocked;
    return { edit_session_id: "session", generation };
  });
  draft.change("first");
  const flush = draft.flush();
  draft.change("latest");
  release();
  await flush;
  expect(saved).toEqual(["first", "latest"]);
  expect(draft.durable).toBe(2n);
});

test("a single automatic checkpoint preserves newer input for a later checkpoint", async () => {
  const saved: string[] = [];
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => { release = resolve; });
  const draft = new Draft(source(), "session", "0", async (generation, bytes) => {
    saved.push(new TextDecoder().decode(bytes));
    if (generation === "1") await blocked;
    return { edit_session_id: "session", generation };
  });
  draft.change("first");
  const checkpoint = draft.flushOnce();
  draft.change("latest");
  release();
  await checkpoint;
  expect(saved).toEqual(["first"]);
  expect(draft.durable).toBe(1n);
  expect(draft.dirty).toBe(true);
  await draft.flushOnce();
  expect(saved).toEqual(["first", "latest"]);
  expect(draft.durable).toBe(2n);
});

test("an explicit flush waits for an automatic checkpoint and saves newer input", async () => {
  const saved: string[] = [];
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => { release = resolve; });
  const draft = new Draft(source(), "session", "0", async (generation, bytes) => {
    saved.push(new TextDecoder().decode(bytes));
    if (generation === "1") await blocked;
    return { edit_session_id: "session", generation };
  });
  draft.change("first");
  const checkpoint = draft.flushOnce();
  draft.change("latest");
  const flush = draft.flush();
  release();
  await Promise.all([checkpoint, flush]);
  expect(saved).toEqual(["first", "latest"]);
  expect(draft.durable).toBe(2n);
  expect(draft.dirty).toBe(false);
});

test("failed persistence keeps input dirty and can retry", async () => {
  let fail = true;
  const draft = new Draft(source(), "session", "5", async (generation) => {
    if (fail) throw new Error("disk");
    return { edit_session_id: "session", generation };
  });
  draft.change("unsent");
  await expect(draft.flush()).rejects.toThrow();
  expect(draft.dirty).toBe(true);
  expect(draft.text).toBe("unsent");
  fail = false;
  await draft.flush();
  expect(draft.dirty).toBe(false);
});

test("reply from a different session or generation cannot complete the draft", async () => {
  for (const reply of [{ edit_session_id: "old", generation: "1" }, { edit_session_id: "session", generation: "0" }]) {
    const draft = new Draft(source(), "session", "0", async () => reply);
    draft.change("pending");
    await expect(draft.flush()).rejects.toThrow();
    expect(draft.dirty).toBe(true);
  }
});

test("unchanged and read-only documents never submit a checkpoint", async () => {
  let count = 0;
  const draft = new Draft(source(), "session", "0", async (generation) => {
    count++;
    return { edit_session_id: "session", generation };
  });
  await draft.flush();
  expect(count).toBe(0);
});


test("edited then undone CRLF text still writes an LF checkpoint", async () => {
  const bytes: Uint8Array[] = [];
  const draft = new Draft(openSource(new TextEncoder().encode("a\r\nb")), "session", "0", async (generation, content) => {
    bytes.push(content);
    return {edit_session_id: "session", generation};
  });
  draft.change("edited\nb");
  await draft.flush();
  draft.change("a\nb");
  await draft.flush();
  expect(new TextDecoder().decode(bytes[0])).toBe("edited\nb");
  expect(new TextDecoder().decode(bytes[1])).toBe("a\nb");
});

test("recovery snapshot retains original bytes until an edit and keeps input after a failed flush", async () => {
  const original = new TextEncoder().encode("\uFEFFa\r\nb");
  const draft = new Draft(openSource(original), "session", "0", async () => { throw new Error("disk"); });
  expect(draft.recoveryBytes()).toEqual(original);
  draft.change("수정\n내용");
  await expect(draft.flush()).rejects.toThrow("disk");
  expect(draft.recoveryBytes()).toEqual(new TextEncoder().encode("\uFEFF수정\n내용"));
  expect(draft.dirty).toBe(true);
  draft.change("a\nb");
  expect(draft.recoveryBytes()).toEqual(new TextEncoder().encode("\uFEFFa\nb"));
});

test("recovery snapshot keeps oversized edited input even when checkpoint refuses it", async () => {
  const draft = new Draft(source(), "session", "0", async () => { throw new Error("must not save"); });
  draft.change("x".repeat(5 * 1024 * 1024 + 1));
  await expect(draft.flush()).rejects.toThrow("5 MiB");
  expect(draft.recoveryBytes().length).toBe(5 * 1024 * 1024 + 1);
});

test("read-only recovery snapshot preserves non-UTF8 original bytes", () => {
  const original = Uint8Array.from([0xff, 0xfe, 0x00]);
  const draft = new Draft(openSource(original), "session", "0", async () => { throw new Error("must not save"); });
  expect(draft.recoveryBytes()).toEqual(original);
});
