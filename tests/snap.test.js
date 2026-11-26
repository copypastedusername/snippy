// Unit tests for ui/scripts/snap.js (run: node --test tests).
'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { prepareSnapFrame, resolveRectangleDrag, snapRectangle } = require('../ui/scripts/snap.js');

/** A width×height RGBA frame filled by `paint(x, y) -> [r, g, b]`. */
function frameOf(width, height, paint, extra = {}) {
  const pixels = new Uint8ClampedArray(width * height * 4);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      const [r, g, b] = paint(x, y);
      const i = (y * width + x) * 4;
      pixels[i] = r;
      pixels[i + 1] = g;
      pixels[i + 2] = b;
      pixels[i + 3] = 255;
    }
  }
  return prepareSnapFrame({ pixels, width, height, ...extra });
}

const flat = (width = 400, height = 300, extra) => frameOf(width, height, () => [128, 128, 128], extra);

function drag(frame, anchor, pointer, options = {}) {
  return resolveRectangleDrag(frame, { anchor, pointer, ...options });
}

test('shift turns a nearly 16:9 drag into an exact 16:9 rectangle in every direction', () => {
  const frame = flat(2000, 1200);
  const anchor = { x: 1000, y: 600 };
  for (const [dx, dy] of [[1, 1], [-1, 1], [1, -1], [-1, -1]]) {
    const result = drag(frame, anchor, { x: anchor.x + dx * 290, y: anchor.y + dy * 162 }, { shift: true });
    assert.equal(result.ratio.label, '16:9');
    assert.equal(result.rect.width * 9, result.rect.height * 16);
    // The start corner stays where the drag began.
    assert.equal(dx > 0 ? result.rect.x : result.rect.x + result.rect.width, anchor.x);
    assert.equal(dy > 0 ? result.rect.y : result.rect.y + result.rect.height, anchor.y);
  }
});

test('shift makes a square from a near-square drag and a portrait ratio from a tall one', () => {
  const frame = flat(1000, 1000);
  const square = drag(frame, { x: 100, y: 100 }, { x: 200, y: 197 }, { shift: true });
  assert.equal(square.ratio.label, '1:1');
  assert.equal(square.rect.width, square.rect.height);
  const tall = drag(frame, { x: 100, y: 100 }, { x: 190, y: 260 }, { shift: true });
  assert.equal(tall.ratio.label, '9:16');
  assert.equal(tall.rect.width * 16, tall.rect.height * 9);
});

test('a held proportion does not flip to a neighbour that fits only slightly better', () => {
  const frame = flat(2000, 1200);
  const anchor = { x: 10, y: 10 };
  const first = drag(frame, anchor, { x: 10 + 300, y: 10 + 200 }, { shift: true });
  assert.equal(first.ratio.label, '3:2');
  // Halfway between 3:2 and 16:10 the held 3:2 stays.
  const between = drag(frame, anchor, { x: 10 + 310, y: 10 + 200 }, { shift: true, previous: first.next });
  assert.equal(between.ratio.label, '3:2');
});

test('shift ignores a drag too small to have a shape yet', () => {
  const frame = flat();
  const result = drag(frame, { x: 10, y: 10 }, { x: 15, y: 10 }, { shift: true });
  assert.equal(result.ratio, null);
});

test('alt snaps to a soft edge the old detector missed', () => {
  // A two-pixel ramp 40 -> 52 -> 64 -> 76: no neighbouring pair differs by 24.
  const ramp = [40, 52, 64, 76];
  const frame = frameOf(400, 300, (x) => {
    const step = Math.min(3, Math.max(0, x - 198));
    return [ramp[step], ramp[step], ramp[step]];
  });
  const result = drag(frame, { x: 50, y: 50 }, { x: 205, y: 250 }, { alt: true });
  assert.ok(Math.abs(result.rect.x + result.rect.width - 200) <= 1, `right side at ${result.rect.x + result.rect.width}`);
  assert.equal(result.snappedSides.right, true);
});

test('alt snaps to an edge that covers only part of the side', () => {
  // A panel edge at x = 220 over rows 50..160 of a side spanning rows 40..240.
  const frame = frameOf(400, 300, (x, y) => (x >= 220 && y >= 50 && y < 160 ? [30, 30, 30] : [220, 220, 220]));
  const result = drag(frame, { x: 60, y: 40 }, { x: 226, y: 240 }, { alt: true });
  assert.equal(result.rect.x + result.rect.width, 220);
});

test('alt leaves noise and texture alone', () => {
  const checker = frameOf(400, 300, (x, y) => ((x + y) % 2 ? [0, 0, 0] : [255, 255, 255]));
  const result = drag(checker, { x: 50, y: 50 }, { x: 203, y: 203 }, { alt: true });
  assert.equal(result.rect.x + result.rect.width, 203);
  assert.equal(result.snappedSides.right, false);
});

test('alt snaps to a colour edge of equal brightness', () => {
  // Red and a green of nearly the same luminance: a luminance-only detector would miss it.
  const frame = frameOf(400, 300, (x) => (x < 210 ? [200, 60, 60] : [70, 100, 70]));
  const result = drag(frame, { x: 50, y: 50 }, { x: 205, y: 250 }, { alt: true });
  assert.equal(result.rect.x + result.rect.width, 210);
});

test('alt snaps to the exact face of a visible window even on flat pixels', () => {
  const windows = [{ x: 120, y: 80, width: 150, height: 120 }];
  const frame = flat(400, 300, { windows });
  const result = drag(frame, { x: 100, y: 100 }, { x: 266, y: 190 }, { alt: true });
  assert.equal(result.rect.x + result.rect.width, 270);
  assert.equal(result.next.sides.right.source, 'window');
});

test('a window face hidden behind a higher window does not count', () => {
  const lower = { x: 120, y: 80, width: 150, height: 120 };
  const higher = { x: 200, y: 0, width: 200, height: 300 };
  const frame = flat(400, 300, { windows: [higher, lower] });
  const result = drag(frame, { x: 100, y: 100 }, { x: 266, y: 190 }, { alt: true });
  assert.equal(result.rect.x + result.rect.width, 266);
});

test('the search radius follows the monitor scale', () => {
  const paint = (x) => (x < 215 ? [230, 230, 230] : [20, 20, 20]);
  const at100 = frameOf(400, 300, paint, { monitors: [{ x: 0, y: 0, width: 400, height: 300, scale: 1 }] });
  const at200 = frameOf(400, 300, paint, { monitors: [{ x: 0, y: 0, width: 400, height: 300, scale: 2 }] });
  const pointer = { x: 200, y: 250 };
  assert.equal(drag(at100, { x: 50, y: 50 }, pointer, { alt: true }).rect.x
    + drag(at100, { x: 50, y: 50 }, pointer, { alt: true }).rect.width, 200);
  const snapped = drag(at200, { x: 50, y: 50 }, pointer, { alt: true });
  assert.equal(snapped.rect.x + snapped.rect.width, 215);
});

test('a snapped side holds its edge a little past the acquire radius, then lets go', () => {
  const frame = frameOf(400, 300, (x) => (x < 200 ? [230, 230, 230] : [20, 20, 20]),
    { monitors: [{ x: 0, y: 0, width: 400, height: 300, scale: 1 }] });
  const anchor = { x: 50, y: 50 };
  const caught = drag(frame, anchor, { x: 208, y: 250 }, { alt: true });
  assert.equal(caught.rect.x + caught.rect.width, 200);
  // 12 px away: beyond the 10 px acquire radius, within the 14 px release radius.
  const held = drag(frame, anchor, { x: 212, y: 250 }, { alt: true, previous: caught.next });
  assert.equal(held.rect.x + held.rect.width, 200);
  const fresh = drag(frame, anchor, { x: 212, y: 250 }, { alt: true });
  assert.equal(fresh.rect.x + fresh.rect.width, 212);
  const released = drag(frame, anchor, { x: 216, y: 250 }, { alt: true, previous: held.next });
  assert.equal(released.rect.x + released.rect.width, 216);
});

test('alt with shift snaps only to edges that keep the proportion exact', () => {
  const windows = [{ x: 0, y: 0, width: 330, height: 400 }];
  const frame = flat(600, 400, { windows, monitors: [{ x: 0, y: 0, width: 600, height: 400, scale: 1 }] });
  const anchor = { x: 10, y: 10 };
  // 16:9 from (10,10): width 320 = 16 * 20 fits the window face at x = 330 exactly.
  const result = drag(frame, anchor, { x: 10 + 314, y: 10 + 176 }, { alt: true, shift: true });
  assert.equal(result.ratio.label, '16:9');
  assert.equal(result.rect.x + result.rect.width, 330);
  assert.equal(result.rect.width * 9, result.rect.height * 16);
  assert.equal(result.snappedSides.right, true);
});

test('resolving again with the returned memory gives the same rectangle (what you see is what you get)', () => {
  const windows = [{ x: 0, y: 0, width: 192, height: 400 }, { x: 0, y: 0, width: 196, height: 400 }];
  const frame = frameOf(400, 300, (x, y) => (x >= 180 || y >= 150 ? [30, 30, 30] : [220, 220, 220]),
    { windows, monitors: [{ x: 0, y: 0, width: 400, height: 300, scale: 2 }] });
  const anchor = { x: 100, y: 100 };
  for (const options of [{ alt: true }, { alt: true, shift: true }, { shift: true }]) {
    let previous = null;
    for (let x = 160; x <= 200; x += 2) {
      const pointer = { x, y: 100 + Math.round((x - 100) * 0.75) };
      const shown = drag(frame, anchor, pointer, { ...options, previous });
      const committed = drag(frame, anchor, pointer, { ...options, previous: shown.next });
      assert.deepEqual(committed.rect, shown.rect, `${JSON.stringify(options)} at x = ${x}`);
      previous = shown.next;
    }
  }
});

test('alt with shift never snaps a side to an edge across the anchor', () => {
  // At 300 % the search reaches 24 px, wider than this 21 px wide 3:7 selection: the window face
  // at x = 97 lies behind the anchor (x = 100) yet within reach of the moving right side.
  const windows = [{ x: 0, y: 0, width: 97, height: 300 }];
  const frame = flat(400, 300, { windows, monitors: [{ x: 0, y: 0, width: 400, height: 300, scale: 3 }] });
  const result = drag(frame, { x: 100, y: 100 }, { x: 121, y: 149 }, { alt: true, shift: true });
  assert.equal(result.ratio.label, '9:21');
  assert.deepEqual(result.rect, { x: 100, y: 100, width: 21, height: 49 });
  assert.equal(result.snappedSides.right, false);
});

test('a drag to the very edge keeps the frame border even beside a pixel edge', () => {
  const frame = frameOf(400, 300, (x) => (x >= 398 ? [20, 20, 20] : [230, 230, 230]),
    { monitors: [{ x: 0, y: 0, width: 400, height: 300, scale: 2 }] });
  const result = drag(frame, { x: 50, y: 50 }, { x: 400, y: 250 }, { alt: true });
  assert.equal(result.rect.x + result.rect.width, 400);
});

test('a ramp leading into a sharp step snaps to the step, not inside the ramp', () => {
  // Brightness climbs slowly from x = 190, then drops sharply at x = 200.
  const frame = frameOf(400, 300, (x) => {
    if (x < 190) return [120, 120, 120];
    if (x < 200) return [120 + (x - 189) * 6, 120 + (x - 189) * 6, 120 + (x - 189) * 6];
    return [10, 10, 10];
  });
  const result = drag(frame, { x: 50, y: 50 }, { x: 196, y: 250 }, { alt: true });
  assert.equal(result.rect.x + result.rect.width, 200);
});

test('shift shapes even a tiny selection', () => {
  const frame = flat();
  const result = drag(frame, { x: 10, y: 10 }, { x: 21, y: 18 }, { shift: true });
  assert.ok(result.ratio, 'a proportion applies');
  assert.equal(result.rect.width * result.ratio.q, result.rect.height * result.ratio.p);
});

test('the old single-call snapRectangle still snaps all sides', () => {
  const frame = frameOf(400, 300, (x, y) => (x >= 100 && x < 300 && y >= 80 && y < 220 ? [20, 20, 20] : [230, 230, 230]));
  const snapped = snapRectangle({ x: 104, y: 83, width: 192, height: 133 }, frame);
  assert.deepEqual(snapped, { x: 100, y: 80, width: 200, height: 140 });
});

test('a 4K side stays within a few milliseconds', () => {
  const windows = Array.from({ length: 40 }, (_, i) => ({ x: 50 + i * 60, y: 40 + i * 30, width: 900, height: 700 }));
  const frame = frameOf(3840, 2160, (x, y) => [(x * 7 + y * 13) % 256, (x * 3) % 256, (y * 5) % 256], {
    windows,
    monitors: [{ x: 0, y: 0, width: 1920, height: 2160, scale: 2 }, { x: 1920, y: 0, width: 1920, height: 2160, scale: 2 }],
  });
  const start = performance.now();
  for (let i = 0; i < 20; i += 1) {
    drag(frame, { x: 200, y: 200 }, { x: 3000 + i, y: 1800 }, { alt: true });
    drag(frame, { x: 200, y: 200 }, { x: 3000 + i, y: 1800 }, { alt: true, shift: true });
  }
  const perCall = (performance.now() - start) / 40;
  assert.ok(perCall < 20, `took ${perCall.toFixed(1)} ms per call`);
});
