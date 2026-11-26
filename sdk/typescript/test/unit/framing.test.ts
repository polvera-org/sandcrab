import assert from "node:assert/strict";
import { test } from "node:test";

import { encodeFrame, encodeRecord, FrameDecoder, FramingError, MAX_RECORD_DATA_LEN, RECORD_EXIT, RECORD_STDIN_EOF, RECORD_STDOUT, RecordDecoder } from "../../src/framing.ts";

test("frames decode when split at every byte boundary", () => {
  const messages = [{ jsonrpc: "2.0", id: 1, result: { text: "héllo ✓" } }, { jsonrpc: "2.0", method: "n", params: {} }];
  const bytes = Buffer.concat(messages.map(encodeFrame));
  for (let cut = 0; cut <= bytes.length; cut++) {
    const d = new FrameDecoder();
    const got: unknown[] = [];
    for (const part of [bytes.subarray(0, cut), bytes.subarray(cut)]) {
      d.push(part);
      for (let m = d.next(); m !== undefined; m = d.next()) got.push(m);
    }
    assert.deepEqual(got, messages, `cut at ${cut}`);
  }
});

test("frames decode one byte at a time, and rest() returns what follows", () => {
  const frame = encodeFrame({ a: 1 });
  const d = new FrameDecoder();
  for (let i = 0; i < frame.length - 1; i++) {
    d.push(frame.subarray(i, i + 1));
    assert.equal(d.next(), undefined);
  }
  d.push(Buffer.concat([frame.subarray(frame.length - 1), Buffer.from("tail")]));
  assert.deepEqual(d.next(), { a: 1 });
  assert.equal(d.rest().toString(), "tail");
});

test("an oversized or non-JSON frame is an error", () => {
  const d = new FrameDecoder();
  d.push(Buffer.from([0xff, 0xff, 0xff, 0xff]));
  assert.throws(() => d.next(), FramingError);
  const e = new FrameDecoder();
  e.push(Buffer.from([0, 0, 0, 2, 0x7b, 0x7b]));
  assert.throws(() => e.next(), FramingError);
});

test("records decode when split at every byte boundary", () => {
  const records = [
    { kind: RECORD_STDOUT, payload: Buffer.from([0, 255, 13, 10, 128]) },
    { kind: RECORD_STDOUT, payload: Buffer.alloc(0) },
    { kind: RECORD_EXIT, payload: Buffer.from('{"code":0}') },
  ];
  const bytes = Buffer.concat(records.map((r) => encodeRecord(r.kind, r.payload)));
  for (let cut = 0; cut <= bytes.length; cut++) {
    const d = new RecordDecoder();
    const got: { kind: number; payload: Buffer }[] = [];
    for (const part of [bytes.subarray(0, cut), bytes.subarray(cut)]) {
      d.push(part);
      for (let r = d.next(); r !== undefined; r = d.next()) got.push({ kind: r.kind, payload: Buffer.from(r.payload) });
    }
    assert.deepEqual(got, records, `cut at ${cut}`);
  }
});

test("an unknown kind or an oversized record is an error", () => {
  const d = new RecordDecoder();
  d.push(Buffer.from([0x7f, 0, 0, 0, 0]));
  assert.throws(() => d.next(), FramingError);
  const e = new RecordDecoder();
  const header = Buffer.alloc(5);
  header.writeUInt8(RECORD_STDOUT, 0);
  header.writeUInt32BE(MAX_RECORD_DATA_LEN + 1, 1);
  e.push(header);
  assert.throws(() => e.next(), FramingError);
  const f = new RecordDecoder();
  f.push(Buffer.from([RECORD_STDIN_EOF, 0, 0, 0, 1, 0]));
  assert.throws(() => f.next(), FramingError);
});
