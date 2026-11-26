// Rectangle drag helpers for the snip overlay.
//
// Shift: the rectangle takes the nearest common proportion (1:1, 4:3, 3:2, 16:10, 16:9, 21:9 or
// their portrait forms) with exact whole-pixel sides, the corner where the drag started staying put.
//
// Alt: the sides snap to edges near them. An edge is either a face of a visible window (its exact
// rectangle is known) or a line in the frozen frame: a boundary where the pixels two steps on each
// side differ in brightness or colour along an unbroken run of the side, so soft, anti-aliased and
// coloured edges count while noise, gradients and texture do not. The search radius follows the
// monitor's scale, and a snapped side holds its edge until the pointer clearly leaves it or a much
// better edge appears, so it does not flicker. With Shift too, only the two moving sides snap, and
// only to edges that keep the proportion exact.

(function exportSnap(global) {
  'use strict';

  /** Proportions offered with Shift, landscape first; portrait forms follow. */
  const LANDSCAPE_RATIOS = [
    [1, 1, '1:1'],
    [4, 3, '4:3'],
    [3, 2, '3:2'],
    [8, 5, '16:10'],
    [16, 9, '16:9'],
    [7, 3, '21:9'],
  ];
  const RATIOS = [
    ...LANDSCAPE_RATIOS,
    ...LANDSCAPE_RATIOS.filter(([p, q]) => p !== q).map(([p, q, label]) => [q, p, label.split(':').reverse().join(':')]),
  ].map(([p, q, label]) => ({ p, q, label }));
  /** A held proportion gives way only to one fitting the drag clearly (2 %) better. */
  const RATIO_SWITCH = Math.log(1.02);
  /** Shift does nothing until the drag is this big (pixels), so a click stays a click. */
  const RATIO_MIN_SIDE = 3;
  const RATIO_MIN_LONG_SIDE = 3;
  /** The smallest selection the overlay accepts. */
  const MIN_SIDE = 3;

  /** Two neighbouring averages differ (0..255 units) by at least this much at an edge. */
  const EDGE_DIFFERENCE = 10;
  /** Differences beyond this count as full strength. */
  const EDGE_FULL = 40;
  /** An edge must cover this share of the side's sampled length, as one unbroken run... */
  const EDGE_COVER = 0.45;
  /** ...and at least this many pixels of it. */
  const EDGE_MIN_PIXELS = 8;
  /** And it must beat the typical boundary around it by this much (rules out texture). */
  const EDGE_STANDS_OUT = 0.12;
  /** Samples taken along a side. */
  const MAX_SAMPLES = 64;
  /** Window faces checked; the topmost windows come first. */
  const MAX_WINDOWS = 128;
  /** A challenger must beat the held edge by this much to take over. */
  const SWITCH_MARGIN = 0.1;
  /** How much distance costs a candidate, relative to the search radius. */
  const DISTANCE_COST = 0.25;
  const FRAME_EDGE_SCORE = 0.88;

  const SOURCE_RANK = { window: 0, frame: 1, pixel: 2 };

  function gcd(a, b) {
    return b === 0 ? a : gcd(b, a % b);
  }

  /**
   * Prepares the frozen frame once per snip: the pixels (RGBA, borrowed), the visible faces of
   * windows (frame-local, topmost first) and the monitors with their scale.
   */
  function prepareSnapFrame({ pixels, width, height, windows = [], monitors = [], scale = 1 }) {
    const validPixels = pixels && width > 0 && height > 0 && pixels.length >= width * height * 4
      ? pixels
      : null;
    return {
      pixels: validPixels,
      width,
      height,
      monitors: monitors.filter((monitor) => monitor.width > 0 && monitor.height > 0),
      scale: Number.isFinite(scale) && scale > 0 ? scale : 1,
      faces: visibleFaces(windows.slice(0, MAX_WINDOWS), width, height),
    };
  }

  /** Subtracts [from, to) from a sorted list of disjoint intervals. */
  function subtract(intervals, from, to) {
    const left = [];
    for (const [a, b] of intervals) {
      if (b <= from || a >= to) {
        left.push([a, b]);
        continue;
      }
      if (a < from) left.push([a, from]);
      if (b > to) left.push([to, b]);
    }
    return left;
  }

  /**
   * Each window's four faces with the parts of them hidden behind windows above cut away.
   * `vertical` faces are x positions spanning rows; horizontal faces are y positions spanning
   * columns. A face only counts where the pixels just inside it belong to its own window.
   */
  function visibleFaces(windows, frameWidth, frameHeight) {
    const faces = { vertical: [], horizontal: [] };
    const rects = windows
      .map((w) => ({ x: Math.round(w.x), y: Math.round(w.y), width: Math.round(w.width), height: Math.round(w.height) }))
      .filter((w) => w.width >= MIN_SIDE && w.height >= MIN_SIDE);
    rects.forEach((rect, index) => {
      const above = rects.slice(0, index);
      const right = rect.x + rect.width;
      const bottom = rect.y + rect.height;
      const top = Math.max(0, rect.y);
      const bottomClipped = Math.min(frameHeight, bottom);
      const left = Math.max(0, rect.x);
      const rightClipped = Math.min(frameWidth, right);
      const addVertical = (at, insideColumn) => {
        if (at <= 0 || at >= frameWidth) return;
        let intervals = [[top, bottomClipped]];
        for (const higher of above) {
          if (insideColumn >= higher.x && insideColumn < higher.x + higher.width) {
            intervals = subtract(intervals, higher.y, higher.y + higher.height);
          }
        }
        intervals.filter(([a, b]) => b > a).forEach(([a, b]) => faces.vertical.push({ at, from: a, to: b }));
      };
      const addHorizontal = (at, insideRow) => {
        if (at <= 0 || at >= frameHeight) return;
        let intervals = [[left, rightClipped]];
        for (const higher of above) {
          if (insideRow >= higher.y && insideRow < higher.y + higher.height) {
            intervals = subtract(intervals, higher.x, higher.x + higher.width);
          }
        }
        intervals.filter(([a, b]) => b > a).forEach(([a, b]) => faces.horizontal.push({ at, from: a, to: b }));
      };
      addVertical(rect.x, rect.x);
      addVertical(right, right - 1);
      addHorizontal(rect.y, rect.y);
      addHorizontal(bottom, bottom - 1);
    });
    return faces;
  }

  /** The scale of the monitor under `point`, kept between 1 and 3. */
  function scaleAt(frame, point) {
    const monitor = frame.monitors.find((m) => point.x >= m.x && point.x < m.x + m.width
      && point.y >= m.y && point.y < m.y + m.height);
    const scale = monitor?.scale ?? frame.scale;
    return Math.min(3, Math.max(1, Number.isFinite(scale) ? scale : 1));
  }

  function radii(scale) {
    const acquire = Math.min(24, Math.round(10 * scale));
    return { acquire, release: acquire + Math.min(8, Math.ceil(4 * scale)) };
  }

  /** Whether one monitor holds both points (so a sample never straddles a gap between screens). */
  function sameMonitor(frame, ax, ay, bx, by) {
    if (frame.monitors.length === 0) return true;
    return frame.monitors.some((m) => ax >= m.x && ax < m.x + m.width && ay >= m.y && ay < m.y + m.height
      && bx >= m.x && bx < m.x + m.width && by >= m.y && by < m.y + m.height);
  }

  /**
   * How much the boundary `b` looks like an edge over the side's span (0..1). `vertical`: the
   * boundary lies between columns b-1 and b, over rows from..to; otherwise between rows.
   */
  function pixelEdgeScore(frame, vertical, b, from, to) {
    const { pixels, width, height } = frame;
    const limit = vertical ? width : height;
    const length = to - from;
    if (!pixels || b <= 0 || b >= limit || length < EDGE_MIN_PIXELS) return 0;
    const samples = Math.min(MAX_SAMPLES, length);
    const near = [Math.max(0, b - 2), b - 1, b, Math.min(limit - 1, b + 1)];
    let runHits = 0;
    let runStrength = 0;
    let runSign = 0;
    let pendingMiss = false;
    let bestHits = 0;
    let bestStrength = 0;
    const closeRun = () => {
      if (runHits > bestHits || (runHits === bestHits && runStrength > bestStrength)) {
        bestHits = runHits;
        bestStrength = runStrength;
      }
    };
    for (let s = 0; s < samples; s += 1) {
      const along = from + Math.floor(((s + 0.5) * length) / samples);
      const at = (across) => (vertical ? (along * width + across) * 4 : (across * width + along) * 4);
      const valid = vertical
        ? sameMonitor(frame, near[0], along, near[3], along)
        : sameMonitor(frame, along, near[0], along, near[3]);
      let hit = 0;
      let strength = 0;
      if (valid) {
        const i0 = at(near[0]); const i1 = at(near[1]); const i2 = at(near[2]); const i3 = at(near[3]);
        const dR = (pixels[i2] + pixels[i3] - pixels[i0] - pixels[i1]) / 2;
        const dG = (pixels[i2 + 1] + pixels[i3 + 1] - pixels[i0 + 1] - pixels[i1 + 1]) / 2;
        const dB = (pixels[i2 + 2] + pixels[i3 + 2] - pixels[i0 + 2] - pixels[i1 + 2]) / 2;
        const luma = (54 * dR + 183 * dG + 19 * dB) / 256;
        const channel = [dR, dG, dB].reduce((best, d) => (Math.abs(d) > Math.abs(best) ? d : best), 0);
        const signed = Math.abs(luma) >= 0.75 * Math.abs(channel) ? luma : 0.75 * channel;
        if (Math.abs(signed) >= EDGE_DIFFERENCE) {
          hit = Math.sign(signed);
          strength = Math.min(Math.abs(signed), EDGE_FULL) / EDGE_FULL;
        }
      }
      if (hit !== 0 && (hit === runSign || runSign === 0)) {
        runSign = hit;
        runHits += 1;
        runStrength += strength;
        pendingMiss = false;
      } else if (hit !== 0) {
        closeRun();
        runSign = hit;
        runHits = 1;
        runStrength = strength;
        pendingMiss = false;
      } else if (runSign !== 0 && !pendingMiss) {
        pendingMiss = true; // one stray sample does not break an edge
      } else {
        closeRun();
        runSign = 0;
        runHits = 0;
        runStrength = 0;
        pendingMiss = false;
      }
    }
    closeRun();
    const cover = bestHits / samples;
    if (cover < EDGE_COVER || cover * length < EDGE_MIN_PIXELS) return 0;
    return 0.7 * cover + 0.3 * (bestStrength / bestHits);
  }

  /**
   * Edges a side at `position` could snap to: pixel lines, visible window faces and the frame's
   * own border, each with a score (0..1). `from..to` is the span along the side.
   */
  function edgeCandidates(frame, vertical, position, from, to, scale, merge = true) {
    const { acquire, release } = radii(scale);
    const limit = vertical ? frame.width : frame.height;
    const lo = Math.max(0, position - release);
    const hi = Math.min(limit, position + release);
    const inset = Math.min(Math.floor((to - from) / 4), Math.round(6 * scale));
    const spanFrom = from + inset;
    const spanTo = to - inset;
    const candidates = [];

    const scores = [];
    for (let b = lo; b <= hi; b += 1) {
      scores.push(b > 0 && b < limit ? pixelEdgeScore(frame, vertical, b, spanFrom, spanTo) : 0);
    }
    const interior = scores.filter((_, index) => lo + index > 0 && lo + index < limit).sort((a, b) => a - b);
    const typical = interior.length ? interior[Math.floor(interior.length / 2)] : 0;
    for (let index = 0; index < scores.length; index += 1) {
      const score = scores[index];
      if (score <= 0 || score < typical + EDGE_STANDS_OUT) continue;
      // A plateau of equal scores counts once, at its middle, and only if nothing next to it on
      // either side scores higher.
      if ((scores[index - 1] ?? -1) === score) continue;
      let end = index;
      while ((scores[end + 1] ?? -1) === score) end += 1;
      if ((scores[index - 1] ?? 0) > score || (scores[end + 1] ?? 0) > score) continue;
      candidates.push({ at: lo + Math.floor((index + end + 1) / 2), score, source: 'pixel' });
    }

    const length = spanTo - spanFrom;
    if (length > 0) {
      for (const face of vertical ? frame.faces.vertical : frame.faces.horizontal) {
        if (face.at < lo || face.at > hi) continue;
        const overlap = Math.min(face.to, spanTo) - Math.max(face.from, spanFrom);
        const share = overlap / length;
        if (overlap >= EDGE_MIN_PIXELS && share >= EDGE_COVER) {
          candidates.push({ at: face.at, score: 0.78 + 0.18 * Math.min(1, share), source: 'window' });
        }
      }
    }
    for (const border of [0, limit]) {
      if (border >= lo && border <= hi) {
        candidates.push({ at: border, score: FRAME_EDGE_SCORE, source: 'frame' });
      }
    }

    // A pixel edge right next to a window face or the frame border is that edge: keep the exact
    // position and the better score. Faces and borders never absorb one another.
    const mergeDistance = Math.max(1, Math.round(scale));
    candidates.sort((a, b) => SOURCE_RANK[a.source] - SOURCE_RANK[b.source] || b.score - a.score);
    const merged = [];
    for (const candidate of candidates) {
      const same = merge && candidate.source === 'pixel'
        ? merged.find((kept) => kept.source !== 'pixel' && Math.abs(kept.at - candidate.at) <= mergeDistance)
        : null;
      if (same) {
        same.score = Math.max(same.score, candidate.score);
      } else {
        merged.push({ ...candidate });
      }
    }
    return merged.map((candidate) => ({
      ...candidate,
      distance: Math.abs(candidate.at - position),
      rank: candidate.score - DISTANCE_COST * Math.abs(candidate.at - position) / acquire,
    }));
  }

  function better(a, b) {
    return a.rank - b.rank
      || b.distance - a.distance
      || SOURCE_RANK[b.source] - SOURCE_RANK[a.source];
  }

  /**
   * Picks the edge for one side: a new edge within the acquire radius, or the held one while it is
   * still an edge within the release radius and nothing beats it by the switch margin.
   */
  function pickEdge(candidates, held, scale) {
    const { acquire, release } = radii(scale);
    const fresh = candidates.filter((c) => c.distance <= acquire).sort((a, b) => better(b, a))[0] ?? null;
    const kept = held ? candidates.find((c) => c.at === held.at && c.distance <= release) : null;
    if (kept && (!fresh || fresh.at === kept.at || fresh.rank < kept.rank + SWITCH_MARGIN)) {
      return kept;
    }
    return fresh;
  }

  function errorOf(ratio, w, h) {
    return Math.abs(Math.log(w / h) - Math.log(ratio.p / ratio.q));
  }

  /** The largest and smallest multiple `k` of p:q that fits between the anchor and the frame. */
  function ratioRange(ratio, availableWidth, availableHeight) {
    const min = Math.max(Math.ceil(MIN_SIDE / ratio.p), Math.ceil(MIN_SIDE / ratio.q));
    const max = Math.floor(Math.min(availableWidth / ratio.p, availableHeight / ratio.q));
    return { min, max };
  }

  function chooseRatio(w, h, held, availableWidth, availableHeight) {
    if (Math.min(w, h) < RATIO_MIN_SIDE || Math.max(w, h) < RATIO_MIN_LONG_SIDE) return null;
    const feasible = RATIOS.filter((ratio) => {
      const range = ratioRange(ratio, availableWidth, availableHeight);
      return range.min <= range.max;
    });
    let best = null;
    for (const ratio of feasible) {
      if (!best || errorOf(ratio, w, h) < errorOf(best, w, h)) best = ratio;
    }
    const keep = held && feasible.find((ratio) => ratio.p === held.p && ratio.q === held.q);
    if (keep && best && errorOf(keep, w, h) - errorOf(best, w, h) < RATIO_SWITCH) return keep;
    return best;
  }

  function rectFrom(anchor, dirX, dirY, w, h) {
    return {
      x: dirX < 0 ? anchor.x - w : anchor.x,
      y: dirY < 0 ? anchor.y - h : anchor.y,
      width: w,
      height: h,
    };
  }

  /**
   * Resolves the rectangle being dragged from `anchor` (where the drag started) to `pointer`
   * (frame boundaries, 0..width/height) with Alt and Shift as given. `previous` is the `next` of
   * the last call in this drag (or null). Returns the rectangle, the proportion used (or null), the
   * sides that snapped ({left, right, top, bottom} booleans) and the memory for the next call.
   */
  function resolveRectangleDrag(frame, { anchor, pointer, alt = false, shift = false, previous = null }) {
    const dirX = pointer.x < anchor.x ? -1 : 1;
    const dirY = pointer.y < anchor.y ? -1 : 1;
    const quadrant = `${dirX},${dirY}`;
    const memory = previous && previous.quadrant === quadrant ? previous : null;
    const rawW = Math.abs(pointer.x - anchor.x);
    const rawH = Math.abs(pointer.y - anchor.y);
    const availableWidth = dirX > 0 ? frame.width - anchor.x : anchor.x;
    const availableHeight = dirY > 0 ? frame.height - anchor.y : anchor.y;
    const scale = scaleAt(frame, {
      x: Math.min(frame.width - 1, Math.max(0, pointer.x)),
      y: Math.min(frame.height - 1, Math.max(0, pointer.y)),
    });
    const none = { left: false, right: false, top: false, bottom: false };
    const next = { quadrant, ratio: null, sides: {} };

    const ratio = shift ? chooseRatio(rawW, rawH, memory?.ratio, availableWidth, availableHeight) : null;
    let rect;
    let k = 0;
    if (ratio) {
      const range = ratioRange(ratio, availableWidth, availableHeight);
      k = Math.round((ratio.p * rawW + ratio.q * rawH) / (ratio.p * ratio.p + ratio.q * ratio.q));
      k = Math.min(range.max, Math.max(range.min, k));
      rect = rectFrom(anchor, dirX, dirY, ratio.p * k, ratio.q * k);
      next.ratio = ratio;
    } else {
      rect = rectFrom(anchor, dirX, dirY, rawW, rawH);
    }

    if (!alt || rect.width < MIN_SIDE || rect.height < MIN_SIDE) {
      return { rect, ratio: ratio ? { ...ratio } : null, snappedSides: none, next };
    }

    if (!ratio) {
      // Alt alone: every side snaps on its own.
      const sides = {
        left: [true, rect.x, rect.y, rect.y + rect.height],
        right: [true, rect.x + rect.width, rect.y, rect.y + rect.height],
        top: [false, rect.y, rect.x, rect.x + rect.width],
        bottom: [false, rect.y + rect.height, rect.x, rect.x + rect.width],
      };
      const chosen = {};
      for (const [name, [vertical, position, from, to]] of Object.entries(sides)) {
        const candidates = edgeCandidates(frame, vertical, position, from, to, scale);
        const edge = pickEdge(candidates, memory?.sides?.[name], scale);
        chosen[name] = edge ? edge.at : position;
        if (edge) next.sides[name] = { at: edge.at, source: edge.source };
      }
      const snapped = {
        x: chosen.left,
        y: chosen.top,
        width: chosen.right - chosen.left,
        height: chosen.bottom - chosen.top,
      };
      if (snapped.width < MIN_SIDE || snapped.height < MIN_SIDE) {
        return { rect, ratio: null, snappedSides: none, next: { quadrant, ratio: null, sides: {} } };
      }
      return {
        rect: snapped,
        ratio: null,
        snappedSides: {
          left: Boolean(next.sides.left),
          right: Boolean(next.sides.right),
          top: Boolean(next.sides.top),
          bottom: Boolean(next.sides.bottom),
        },
        next,
      };
    }

    // Alt with Shift: the two moving sides snap only to edges that keep p:q exact.
    const xSide = dirX > 0 ? 'right' : 'left';
    const ySide = dirY > 0 ? 'bottom' : 'top';
    const movingX = anchor.x + dirX * rect.width;
    const movingY = anchor.y + dirY * rect.height;
    const range = ratioRange(ratio, availableWidth, availableHeight);
    const { acquire, release } = radii(scale);
    // Only the edge a side already holds may stay out to the release radius.
    const reach = (side, edge) => (memory?.sides?.[side]?.at === edge.at ? release : acquire);
    const groups = new Map();
    const consider = (side, vertical, position, from, to, unit) => {
      const direction = vertical ? dirX : dirY;
      for (const edge of edgeCandidates(frame, vertical, position, from, to, scale, false)) {
        // An edge on the far side of the anchor cannot be this side's.
        const span = (edge.at - (vertical ? anchor.x : anchor.y)) * direction;
        if (span <= 0 || span % unit !== 0 || edge.distance > reach(side, edge)) continue;
        const multiple = span / unit;
        if (multiple < range.min || multiple > range.max) continue;
        const group = groups.get(multiple) ?? {};
        if (!group[side] || better(edge, group[side]) > 0) group[side] = edge;
        groups.set(multiple, group);
      }
    };
    consider(xSide, true, movingX, rect.y, rect.y + rect.height, ratio.p);
    consider(ySide, false, movingY, rect.x, rect.x + rect.width, ratio.q);
    let best = null;
    for (const [multiple, group] of groups) {
      const score = (group[xSide]?.rank ?? 0) + (group[ySide]?.rank ?? 0);
      // The held proportion's multiple keeps a bonus only while it still uses a held edge.
      const heldHere = memory?.k === multiple
        && memory?.ratio?.p === ratio.p && memory?.ratio?.q === ratio.q
        && [xSide, ySide].some((side) => group[side] && memory.sides?.[side]?.at === group[side].at);
      const candidate = { multiple, group, score: score + (heldHere ? SWITCH_MARGIN : 0) };
      if (!best || candidate.score > best.score
        || (candidate.score === best.score && Math.abs(multiple - k) < Math.abs(best.multiple - k))) {
        best = candidate;
      }
    }
    if (!best) {
      return { rect, ratio: { ...ratio }, snappedSides: none, next };
    }
    const snappedRect = rectFrom(anchor, dirX, dirY, ratio.p * best.multiple, ratio.q * best.multiple);
    next.k = best.multiple;
    const snappedSides = { ...none };
    for (const side of [xSide, ySide]) {
      if (best.group[side]) {
        snappedSides[side] = true;
        next.sides[side] = { at: best.group[side].at, source: best.group[side].source };
      }
    }
    return { rect: snappedRect, ratio: { ...ratio }, snappedSides, next };
  }

  /** The earlier single-call form: snaps every side of `rect` (frame pixels) to nearby edges. */
  function snapRectangle(rect, frameLike) {
    if (!rect || !frameLike || rect.width < 2 || rect.height < 2) return rect;
    const frame = frameLike.faces ? frameLike : prepareSnapFrame(frameLike);
    return resolveRectangleDrag(frame, {
      anchor: { x: rect.x, y: rect.y },
      pointer: { x: rect.x + rect.width, y: rect.y + rect.height },
      alt: true,
    }).rect;
  }

  const api = { prepareSnapFrame, resolveRectangleDrag, snapRectangle, RATIOS };
  global.prepareSnapFrame = prepareSnapFrame;
  global.resolveRectangleDrag = resolveRectangleDrag;
  global.snapRectangle = snapRectangle;
  if (typeof module !== 'undefined') {
    module.exports = api;
  }
}(typeof window !== 'undefined' ? window : globalThis));
