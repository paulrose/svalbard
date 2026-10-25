import test from "node:test";
import assert from "node:assert/strict";
import { daylightAt } from "../src/scripts/daylight.mjs";

test("Longyearbyen stays light through both noon and midnight in midsummer", () => {
  for (const hour of ["10:00", "22:00"]) {
    const sun = daylightAt(new Date(`2026-06-21T${hour}:00Z`));
    assert.equal(sun.theme, "light");
    assert.equal(sun.phase, "Midnight sun");
    assert.ok(sun.elevation > 10);
  }
});

test("the winter solstice has polar night even at local noon", () => {
  for (const hour of ["11:00", "23:00"]) {
    const sun = daylightAt(new Date(`2026-12-21T${hour}:00Z`));
    assert.equal(sun.theme, "dark");
    assert.equal(sun.phase, "Polar night");
    assert.ok(sun.elevation < -6);
  }
});

test("equinox daylight, civil twilight, and night produce different states", () => {
  assert.equal(daylightAt(new Date("2026-03-20T11:00:00Z")).theme, "light");
  const twilight = daylightAt(new Date("2026-03-20T18:00:00Z"));
  assert.equal(twilight.theme, "dark");
  assert.equal(twilight.phase, "Twilight");
  assert.equal(daylightAt(new Date("2026-03-20T23:00:00Z")).phase, "Night");
});

test("timezone offsets do not change the solar state for the same instant", () => {
  const utc = daylightAt(new Date("2028-02-29T11:00:00Z"));
  const offset = daylightAt(new Date("2028-02-29T03:00:00-08:00"));
  assert.deepEqual(utc, offset);
  assert.ok(Number.isFinite(utc.elevation));
});
