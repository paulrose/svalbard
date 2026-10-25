import test from "node:test";
import assert from "node:assert/strict";
import * as run from "../src/scripts/timeline.mjs";

test("the snapshot closes after the last read and before the upload starts", () => {
  assert.ok(run.CLOSE > run.LAST_READ);
  assert.ok(run.UPLOAD[0] > run.CLOSE);
});

test("the upload starts only once all data has reached svalbard", () => {
  assert.ok(run.UPLOAD[0] >= run.WRITTEN);
});

test("no moment shows an open snapshot while storage receives data", () => {
  for (let t = 0; t < run.CYCLE; t += 0.01) {
    const phase = run.phaseAt(t);
    assert.ok(
      !(phase.snapshot === "open" && phase.storage),
      `overlap at ${t.toFixed(2)}s`,
    );
  }
});

test("each run settles before the next one starts", () => {
  assert.ok(run.SINK[1] < run.CYCLE);
  assert.ok(run.COOL[1] < run.CYCLE);
});
