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
