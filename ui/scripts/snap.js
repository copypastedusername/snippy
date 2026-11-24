// Snaps a selection to the edges visible in the frozen frame (hold Alt while dragging).
//
// Each side of the rectangle looks a few pixels in and out for the boundary where neighbouring
// pixels differ most along that side: a window border, a button outline, the edge of a picture.
// A side moves only when that boundary is a real line, i.e. most of the side's length crosses it,
// so noise and gradients leave the selection where the pointer put it. Screen edges count as lines.

(function exportSnap(global) {
  'use strict';

  /** How far (in screen pixels) a side may jump to reach an edge. */
  const SNAP_RADIUS = 10;
  /** Two pixels count as different when a colour channel differs by at least this much. */
  const PIXEL_DIFFERENCE = 24;
  /** A boundary is a line when at least this share of the side's length crosses it. */
  const LINE_SHARE = 0.75;
  /** ...and stand out from the typical boundary around it by at least this much. */
  const STANDS_OUT = 0.5;
  /** Samples taken along a side; long sides are sampled evenly instead of pixel by pixel. */
  const MAX_SAMPLES = 96;

  function differs(pixels, width, a, b) {
    const i = a * 4;
    const j = b * 4;
    return Math.abs(pixels[i] - pixels[j]) >= PIXEL_DIFFERENCE
      || Math.abs(pixels[i + 1] - pixels[j + 1]) >= PIXEL_DIFFERENCE
      || Math.abs(pixels[i + 2] - pixels[j + 2]) >= PIXEL_DIFFERENCE;
  }

  /**
   * Share (0..1) of the side's samples where the pixels on both sides of the boundary differ.
   * `vertical`: the boundary lies between columns `boundary - 1` and `boundary`, over rows
   * `from..to`; otherwise between rows, over columns.
   */
  function lineStrength(frame, vertical, boundary, from, to) {
    const limit = vertical ? frame.width : frame.height;
    if (boundary <= 0 || boundary >= limit) {
      return 1;
    }
    const length = Math.max(1, to - from);
    const step = Math.max(1, Math.floor(length / MAX_SAMPLES));
    let hits = 0;
    let samples = 0;
    for (let along = from; along < to; along += step) {
      const a = vertical ? along * frame.width + boundary - 1 : (boundary - 1) * frame.width + along;
      const b = vertical ? along * frame.width + boundary : boundary * frame.width + along;
      if (differs(frame.pixels, frame.width, a, b)) {
        hits += 1;
      }
      samples += 1;
    }
    return samples === 0 ? 0 : hits / samples;
  }

  /**
   * The best boundary near `position`, or `position` itself when there is no line nearby.
   * `outward` (-1 or +1) is the direction that enlarges the selection: a thin line has a boundary
   * on each side, and the outer one is taken so the line ends up inside the snip, not cut in half.
   */
  function snapSide(frame, vertical, position, from, to, outward) {
    const limit = vertical ? frame.width : frame.height;
    const candidates = [];
    for (let distance = 0; distance <= SNAP_RADIUS; distance += 1) {
      for (const candidate of distance === 0 ? [position] : [position - distance, position + distance]) {
        if (candidate >= 0 && candidate <= limit) {
          candidates.push({ at: candidate, strength: lineStrength(frame, vertical, candidate, from, to) });
        }
      }
    }
    // A real edge stands out from the boundaries around it; in texture or noise they all look
    // alike, so a candidate must beat the typical (median) boundary nearby by a clear margin.
    const sorted = candidates.map((candidate) => candidate.strength).sort((a, b) => a - b);
    const typical = sorted[Math.floor(sorted.length / 2)] ?? 0;
    let best = null;
    for (const candidate of candidates) {
      const isLine = candidate.strength >= LINE_SHARE && candidate.strength - typical >= STANDS_OUT;
      // Candidates are ordered by distance, so among equal lines the nearest one is kept.
      if (isLine && (!best || candidate.strength > best.strength)) {
        best = candidate;
      }
    }
    if (!best) {
      return position;
    }
    const outer = best.at + outward;
    if (outer >= 0 && outer <= limit
      && lineStrength(frame, vertical, outer, from, to) >= best.strength * 0.9) {
      return outer;
    }
    return best.at;
  }

  /**
   * `rect` is {x, y, width, height} in frame pixels (sides on pixel boundaries);
   * `frame` is {pixels: RGBA bytes, width, height}. Returns the snapped rectangle.
   */
  function snapRectangle(rect, frame) {
    if (!rect || !frame || rect.width < 2 || rect.height < 2) {
      return rect;
    }
    // Sample along the inner part of each side so the corners of a neighbouring edge do not count.
    const inset = (length) => Math.min(Math.floor(length / 4), SNAP_RADIUS);
    const rowsFrom = rect.y + inset(rect.height);
    const rowsTo = rect.y + rect.height - inset(rect.height);
    const colsFrom = rect.x + inset(rect.width);
    const colsTo = rect.x + rect.width - inset(rect.width);

    const left = snapSide(frame, true, rect.x, rowsFrom, rowsTo, -1);
    const right = snapSide(frame, true, rect.x + rect.width, rowsFrom, rowsTo, 1);
    const top = snapSide(frame, false, rect.y, colsFrom, colsTo, -1);
    const bottom = snapSide(frame, false, rect.y + rect.height, colsFrom, colsTo, 1);
    if (right - left < 1 || bottom - top < 1) {
      return rect;
    }
    return { x: left, y: top, width: right - left, height: bottom - top };
  }

  global.snapRectangle = snapRectangle;
  if (typeof module !== 'undefined') {
    module.exports = { snapRectangle };
  }
}(typeof window !== 'undefined' ? window : globalThis));
