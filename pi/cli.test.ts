import assert from "node:assert/strict";
import test from "node:test";
import { readStdin } from "./cli.ts";

test("readStdin keeps a multibyte character split across two reads", async () => {
  const text = '{"prompt":"Bake at 180°C, then add ½ cup ข้าว"}';
  const bytes = Buffer.from(text, "utf8");
  // Cut inside the two-byte "°".
  const split = bytes.indexOf(Buffer.from("°", "utf8")) + 1;
  async function* chunks(): AsyncGenerator<Buffer> {
    yield bytes.subarray(0, split);
    yield bytes.subarray(split);
  }
  assert.equal(await readStdin(chunks()), text);
});
