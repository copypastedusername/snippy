'use strict';

// Snippy annotation editor. Every mark is a vector object in image coordinates, drawn over the
// untouched original; the image is flattened only when copied or saved. History stores whole
// document snapshots (marks + crop), which keeps undo trivially correct.

const tauri = window.__TAURI__;
const invoke = tauri?.core?.invoke;
const listen = tauri?.event?.listen;

const PREFS_KEY = 'snippy-editor';
const COLORS = ['#ff3b30', '#ff9500', '#ffcc00', '#34c759', '#0a84ff', '#af52de', '#ffffff', '#1c1c1e'];
const EMOJI = [
  '👍', '👎', '👏', '🙌', '👀', '💯', '🔥', '⭐',
  '✅', '❌', '❓', '❗', '⚠️', '💡', '📌', '🎯',
  '😀', '😂', '😍', '🤔', '😮', '😢', '😡', '🥳',
  '❤️', '💔', '👉', '👈', '👆', '👇', '🚀', '🎉',
];
const TOOLS = ['select', 'pen', 'highlighter', 'arrow', 'line', 'rect', 'ellipse', 'text', 'emoji', 'blur', 'crop'];
const TOOL_KEYS = { v: 'select', p: 'pen', h: 'highlighter', a: 'arrow', l: 'line', r: 'rect', e: 'ellipse', t: 'text', m: 'emoji', b: 'blur', c: 'crop' };
const STROKE_TOOLS = new Set(['pen', 'highlighter', 'arrow', 'line', 'rect', 'ellipse']);
const OUTLINE_TOOLS = new Set(['arrow', 'line', 'rect', 'ellipse', 'text', 'pen']);
const PIXEL_BLOCK = 10;
const HISTORY_LIMIT = 100;
const MIN_ZOOM = 0.1;
const MAX_ZOOM = 8;

const $ = (id) => document.getElementById(id);
const el = {
  stage: $('stage'), sheet: $('sheet'), canvas: $('canvas'), textLayer: $('textLayer'), textEditor: $('textEditor'),
  colors: $('colorOptions'), widthOption: $('widthOption'), widthLabel: $('widthLabel'), widthRange: $('widthRange'),
  widthValue: $('widthValue'), outlineOption: $('outlineOption'), outlineSwitch: $('outlineSwitch'),
  fillOption: $('fillOption'), fillSwitch: $('fillSwitch'), emojiOption: $('emojiOption'), emojiButton: $('emojiButton'),
  emojiCurrent: $('emojiCurrent'), emojiPicker: $('emojiPicker'), emojiGrid: $('emojiGrid'), emojiInput: $('emojiInput'),
  cropOption: $('cropOption'), ocrOption: $('ocrOption'), ocrLanguage: $('ocrLanguage'), ocrStatus: $('ocrStatus'),
  textButton: $('textButton'), zoomLabel: $('zoomLabel'), status: $('status'), undo: $('undoButton'), redo: $('redoButton'),
};
const ctx = el.canvas.getContext('2d');

/** Tool settings survive restarts (the Windows tool forgets them). */
const prefs = Object.assign({
  tool: 'arrow', color: COLORS[0], width: 4, outline: true, fill: false, textSize: 28, emojiSize: 64,
  emoji: '👍', ocrLanguage: '',
}, readPrefs());

const doc = { id: null, image: null, base: null, width: 0, height: 0, name: '', marks: [], crop: null };
const view = { zoom: 1, fit: true };
const history = { past: [], future: [] };
/** Each history entry gets a revision number; the saved one is remembered to know what is unsaved. */
let nextRevision = 1;
let savedRevision = 0;
let reportedUnsaved = false;
let exporting = false;
let ocrRequest = 0; // counts text recognition requests, so only the latest one's answer is shown
let selection = null; // index into doc.marks
let draft = null; // mark being drawn, or { kind: 'crop' | 'move', ... }
let cropDraft = null;
let ocr = { active: false, result: null, language: null };
let loadGeneration = 0; // counts loaded pictures, so a late text result for an older one is dropped

function readPrefs() {
  try { return JSON.parse(localStorage.getItem(PREFS_KEY)) || {}; } catch { return {}; }
}
function savePrefs() {
  try { localStorage.setItem(PREFS_KEY, JSON.stringify(prefs)); } catch { /* storage unavailable */ }
}

function setStatus(message, isError = false) {
  el.status.textContent = message;
  el.status.classList.toggle('error', isError);
}
const errorText = (error) => (typeof error === 'string' ? error : error?.message ?? String(error));

// ---------- geometry ----------

const area = () => doc.crop ?? { x: 0, y: 0, w: doc.width, h: doc.height };

function toImage(event) {
  const rect = el.canvas.getBoundingClientRect();
  const a = area();
  return { x: a.x + (event.clientX - rect.left) / view.zoom, y: a.y + (event.clientY - rect.top) / view.zoom };
}

function normalizeRect(x1, y1, x2, y2) {
  return { x: Math.min(x1, x2), y: Math.min(y1, y2), w: Math.abs(x2 - x1), h: Math.abs(y2 - y1) };
}

/** Bounding box of a mark in image coordinates, used for selection and moving. */
function bounds(mark) {
  switch (mark.type) {
    case 'pen': case 'highlighter': {
      // A loop, not Math.min(...points): a very long stroke would exceed the argument limit.
      let minX = Infinity; let minY = Infinity; let maxX = -Infinity; let maxY = -Infinity;
      for (const [x, y] of mark.points) {
        if (x < minX) minX = x; if (x > maxX) maxX = x;
        if (y < minY) minY = y; if (y > maxY) maxY = y;
      }
      const pad = mark.width / 2;
      return { x: minX - pad, y: minY - pad, w: maxX - minX + 2 * pad, h: maxY - minY + 2 * pad };
    }
    case 'arrow': case 'line': {
      const pad = mark.width * 2;
      const r = normalizeRect(mark.x1, mark.y1, mark.x2, mark.y2);
      return { x: r.x - pad, y: r.y - pad, w: r.w + 2 * pad, h: r.h + 2 * pad };
    }
    case 'text': {
      ctx.save(); ctx.font = textFont(mark.size);
      const lines = mark.text.split('\n');
      const w = Math.max(...lines.map((line) => ctx.measureText(line).width));
      ctx.restore();
      return { x: mark.x, y: mark.y, w, h: lines.length * mark.size * 1.2 };
    }
    case 'emoji': return { x: mark.x - mark.size / 2, y: mark.y - mark.size / 2, w: mark.size, h: mark.size };
    default: return { x: mark.x, y: mark.y, w: mark.w, h: mark.h };
  }
}

/** Distance from `p` to the segment a–b. */
function segmentDistance(p, ax, ay, bx, by) {
  const dx = bx - ax; const dy = by - ay;
  const length = dx * dx + dy * dy;
  const t = length ? Math.max(0, Math.min(1, ((p.x - ax) * dx + (p.y - ay) * dy) / length)) : 0;
  return Math.hypot(p.x - (ax + t * dx), p.y - (ay + t * dy));
}

/** Whether `point` touches what the mark actually draws (not just its bounding box). */
function touches(mark, point, slack) {
  const reach = (mark.width ?? 0) / 2 + slack;
  switch (mark.type) {
    case 'pen': case 'highlighter': {
      if (mark.points.length === 1) return Math.hypot(point.x - mark.points[0][0], point.y - mark.points[0][1]) <= reach;
      for (let i = 1; i < mark.points.length; i += 1) {
        const [ax, ay] = mark.points[i - 1]; const [bx, by] = mark.points[i];
        if (segmentDistance(point, ax, ay, bx, by) <= reach) return true;
      }
      return false;
    }
    case 'line': return segmentDistance(point, mark.x1, mark.y1, mark.x2, mark.y2) <= reach;
    case 'arrow': {
      const head = arrowHead(mark);
      return segmentDistance(point, mark.x1, mark.y1, mark.x2, mark.y2) <= reach
        || segmentDistance(point, ...head[0], ...head[1]) <= reach
        || segmentDistance(point, ...head[2], ...head[1]) <= reach;
    }
    case 'rect': {
      const inside = point.x >= mark.x - reach && point.x <= mark.x + mark.w + reach && point.y >= mark.y - reach && point.y <= mark.y + mark.h + reach;
      if (!inside) return false;
      if (mark.fill) return true;
      const deep = point.x > mark.x + reach && point.x < mark.x + mark.w - reach && point.y > mark.y + reach && point.y < mark.y + mark.h - reach;
      return !deep;
    }
    case 'ellipse': {
      const rx = mark.w / 2; const ry = mark.h / 2;
      if (rx <= 0 || ry <= 0) return false;
      const nx = (point.x - (mark.x + rx)) / rx; const ny = (point.y - (mark.y + ry)) / ry;
      const radial = Math.hypot(nx, ny);
      const band = reach / Math.min(rx, ry);
      return mark.fill ? radial <= 1 + band : Math.abs(radial - 1) <= band;
    }
    default: {
      // Text, emoji and pixelated areas are solid: anywhere in their box counts.
      const b = bounds(mark);
      return point.x >= b.x - slack && point.x <= b.x + b.w + slack && point.y >= b.y - slack && point.y <= b.y + b.h + slack;
    }
  }
}

function hit(point) {
  const slack = 6 / view.zoom;
  for (let index = doc.marks.length - 1; index >= 0; index -= 1) {
    if (touches(doc.marks[index], point, slack)) return index;
  }
  return null;
}

function moveMark(mark, dx, dy) {
  const moved = structuredClone(mark);
  if (moved.points) moved.points = moved.points.map(([x, y]) => [x + dx, y + dy]);
  if ('x1' in moved) { moved.x1 += dx; moved.y1 += dy; moved.x2 += dx; moved.y2 += dy; }
  if ('x' in moved) { moved.x += dx; moved.y += dy; }
  return moved;
}

// ---------- drawing ----------

const textFont = (size) => `600 ${size}px "Segoe UI", "Segoe UI Emoji", sans-serif`;

/** A contrasting halo colour for the outline option: dark around light colours and vice versa. */
function haloFor(color) {
  const hex = color.replace('#', '');
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(hex.slice(i, i + 2), 16));
  return (0.299 * r + 0.587 * g + 0.114 * b) > 150 ? 'rgba(0,0,0,0.85)' : 'rgba(255,255,255,0.95)';
}

function strokePath(target, mark, build) {
  target.lineCap = 'round';
  target.lineJoin = 'round';
  if (mark.outline) {
    target.strokeStyle = haloFor(mark.color);
    target.lineWidth = mark.width + Math.max(3, mark.width * 0.8);
    build(); target.stroke();
  }
  target.strokeStyle = mark.color;
  target.lineWidth = mark.width;
  build(); target.stroke();
}

function arrowHead(mark) {
  const angle = Math.atan2(mark.y2 - mark.y1, mark.x2 - mark.x1);
  const size = Math.max(10, mark.width * 3.2);
  return [
    [mark.x2 - size * Math.cos(angle - Math.PI / 7), mark.y2 - size * Math.sin(angle - Math.PI / 7)],
    [mark.x2, mark.y2],
    [mark.x2 - size * Math.cos(angle + Math.PI / 7), mark.y2 - size * Math.sin(angle + Math.PI / 7)],
  ];
}

function drawMark(target, mark) {
  target.save();
  switch (mark.type) {
    case 'pen':
    case 'highlighter': {
      if (mark.type === 'highlighter') { target.globalCompositeOperation = 'multiply'; target.globalAlpha = 0.45; }
      strokePath(target, mark.type === 'highlighter' ? { ...mark, outline: false } : mark, () => {
        target.beginPath();
        mark.points.forEach(([x, y], i) => (i ? target.lineTo(x, y) : target.moveTo(x, y)));
        if (mark.points.length === 1) target.lineTo(mark.points[0][0] + 0.01, mark.points[0][1]);
      });
      break;
    }
    case 'line':
      strokePath(target, mark, () => { target.beginPath(); target.moveTo(mark.x1, mark.y1); target.lineTo(mark.x2, mark.y2); });
      break;
    case 'arrow': {
      const head = arrowHead(mark);
      strokePath(target, mark, () => {
        target.beginPath(); target.moveTo(mark.x1, mark.y1); target.lineTo(mark.x2, mark.y2);
        target.moveTo(...head[0]); target.lineTo(...head[1]); target.lineTo(...head[2]);
      });
      break;
    }
    case 'rect':
    case 'ellipse': {
      const build = () => {
        target.beginPath();
        if (mark.type === 'rect') target.rect(mark.x, mark.y, mark.w, mark.h);
        else target.ellipse(mark.x + mark.w / 2, mark.y + mark.h / 2, mark.w / 2, mark.h / 2, 0, 0, Math.PI * 2);
      };
      if (mark.fill) { build(); target.fillStyle = mark.color; target.globalAlpha = 0.35; target.fill(); target.globalAlpha = 1; }
      strokePath(target, mark, build);
      break;
    }
    case 'text': {
      target.font = textFont(mark.size);
      target.textBaseline = 'top';
      mark.text.split('\n').forEach((line, i) => {
        const y = mark.y + i * mark.size * 1.2;
        if (mark.outline) {
          target.lineJoin = 'round';
          target.strokeStyle = haloFor(mark.color);
          target.lineWidth = Math.max(3, mark.size / 7);
          target.strokeText(line, mark.x, y);
        }
        target.fillStyle = mark.color;
        target.fillText(line, mark.x, y);
      });
      break;
    }
    case 'emoji':
      // Segoe UI Emoji is a colour font; Chromium draws it in full colour, as Windows shows it.
      target.font = `${mark.size}px "Segoe UI Emoji", sans-serif`;
      target.textAlign = 'center';
      target.textBaseline = 'middle';
      target.fillText(mark.char, mark.x, mark.y);
      break;
    case 'blur':
      pixelate(target, mark);
      break;
    default:
      break;
  }
  target.restore();
}

/**
 * Hides an area by replacing it with coarse blocks, each the average of the original pixels under
 * it. Each block first clears what is there, so marks drawn earlier under it are hidden too, and
 * the alpha is kept: the transparent corners of a free-form snip must not turn into black blocks.
 */
const pixelBlocks = new WeakMap(); // mark -> its blocks, worked out once per mark
function pixelate(target, mark) {
  let blocks = pixelBlocks.get(mark);
  if (!blocks || blocks.key !== `${mark.x},${mark.y},${mark.w},${mark.h}`) {
    blocks = pixelBlocksOf(mark);
    pixelBlocks.set(mark, blocks);
  }
  for (const { x, y, w, h, fill } of blocks.list) {
    target.clearRect(x, y, w, h);
    if (fill) { target.fillStyle = fill; target.fillRect(x, y, w, h); }
  }
}

/** Every pixel of each block averaged (colour weighted by alpha, so clear pixels add no colour). */
function pixelBlocksOf(mark) {
  const x0 = Math.max(0, Math.floor(mark.x)); const y0 = Math.max(0, Math.floor(mark.y));
  const x1 = Math.min(doc.width, Math.ceil(mark.x + mark.w)); const y1 = Math.min(doc.height, Math.ceil(mark.y + mark.h));
  const block = Math.max(PIXEL_BLOCK, Math.round(Math.min(mark.w, mark.h) / 12));
  const d = doc.image.data;
  const list = [];
  for (let y = y0; y < y1; y += block) {
    for (let x = x0; x < x1; x += block) {
      const w = Math.min(block, x1 - x); const h = Math.min(block, y1 - y);
      let r = 0; let g = 0; let b = 0; let a = 0;
      for (let sy = y; sy < y + h; sy += 1) {
        for (let sx = x, i = (sy * doc.width + x) * 4; sx < x + w; sx += 1, i += 4) {
          const alpha = d[i + 3];
          r += d[i] * alpha; g += d[i + 1] * alpha; b += d[i + 2] * alpha; a += alpha;
        }
      }
      const fill = a > 0
        ? `rgba(${Math.round(r / a)},${Math.round(g / a)},${Math.round(b / a)},${a / (w * h) / 255})`
        : null;
      list.push({ x, y, w, h, fill });
    }
  }
  return { key: `${mark.x},${mark.y},${mark.w},${mark.h}`, list };
}

/** Paints the document (and editing chrome) at the current zoom. */
function render() {
  if (!doc.base) return;
  view.zoom = Math.min(view.zoom, maxZoom()); // a monitor with a higher scale may lower the cap
  const a = area();
  const dpr = window.devicePixelRatio || 1;
  const cssW = Math.max(1, Math.round(a.w * view.zoom));
  const cssH = Math.max(1, Math.round(a.h * view.zoom));
  if (el.canvas.width !== Math.round(cssW * dpr) || el.canvas.height !== Math.round(cssH * dpr)) {
    el.canvas.width = Math.round(cssW * dpr); el.canvas.height = Math.round(cssH * dpr);
    el.canvas.style.width = `${cssW}px`; el.canvas.style.height = `${cssH}px`;
    el.sheet.style.width = `${cssW}px`; el.sheet.style.height = `${cssH}px`;
  }
  ctx.setTransform(dpr * view.zoom, 0, 0, dpr * view.zoom, -a.x * dpr * view.zoom, -a.y * dpr * view.zoom);
  ctx.imageSmoothingEnabled = view.zoom < 1;
  ctx.clearRect(a.x, a.y, a.w, a.h);
  ctx.drawImage(doc.base, 0, 0);
  doc.marks.forEach((mark) => drawMark(ctx, mark));
  if (draft?.mark) drawMark(ctx, draft.mark);

  const px = 1 / view.zoom;
  if (selection !== null && doc.marks[selection]) {
    const b = bounds(doc.marks[selection]);
    ctx.save(); ctx.setLineDash([4 * px, 3 * px]); ctx.lineWidth = px; ctx.strokeStyle = '#4cc2ff';
    ctx.strokeRect(b.x - 3 * px, b.y - 3 * px, b.w + 6 * px, b.h + 6 * px); ctx.restore();
  }
  if (cropDraft) {
    ctx.save();
    ctx.fillStyle = 'rgba(0,0,0,0.5)';
    ctx.beginPath(); ctx.rect(a.x, a.y, a.w, a.h);
    ctx.rect(cropDraft.x + cropDraft.w, cropDraft.y, -cropDraft.w, cropDraft.h);
    ctx.fill('evenodd');
    ctx.strokeStyle = '#ffffff'; ctx.lineWidth = 2 * px; ctx.strokeRect(cropDraft.x, cropDraft.y, cropDraft.w, cropDraft.h);
    ctx.restore();
  }
  el.zoomLabel.textContent = `${Math.round(view.zoom * 100)}%  ·  ${Math.round(a.w)} × ${Math.round(a.h)}`;
  if (ocr.active) layoutTextLayer();
  placeTextEditor();
}

// ---------- history ----------

function snapshot() { return { marks: structuredClone(doc.marks), crop: doc.crop && { ...doc.crop }, revision: nextRevision++ }; }
function commit() {
  ocr.result = null; // the picture changed, so earlier recognition is stale
  history.past.push(snapshot());
  if (history.past.length > HISTORY_LIMIT) history.past.shift();
  history.future = [];
  updateHistoryButtons();
  reportUnsaved();
}
const currentRevision = () => history.past.at(-1)?.revision ?? 0;
/** Tells Snippy whether closing the editor or opening another picture would lose work. */
function reportUnsaved() {
  const unsaved = Boolean(doc.base) && (currentRevision() !== savedRevision || (!el.textEditor.hidden && el.textEditor.value.trim() !== ''));
  if (unsaved === reportedUnsaved) return;
  reportedUnsaved = unsaved;
  invoke('editor_unsaved', { unsaved }).catch(() => {});
}
function restore(state) {
  const sameCrop = JSON.stringify(state.crop) === JSON.stringify(doc.crop);
  cancelDraft();
  doc.marks = state.marks; doc.crop = state.crop; selection = null; cropDraft = null; ocr.result = null;
  // Recognised words belong to the picture before the undo; take them off the screen.
  if (ocr.active) void setOcr(false);
  updateOptions();
  // Keep the zoom unless the visible area changed.
  if (sameCrop) render(); else fit();
  reportUnsaved();
}
function undo() { cancelDraft(); if (history.past.length < 2) return; history.future.push(history.past.pop()); restore(structuredClone(history.past.at(-1))); updateHistoryButtons(); }
function redo() { cancelDraft(); const next = history.future.pop(); if (!next) return; history.past.push(next); restore(structuredClone(next)); updateHistoryButtons(); }
function updateHistoryButtons() { el.undo.disabled = history.past.length < 2; el.redo.disabled = history.future.length === 0; }

// ---------- tools and options ----------

function setTool(tool) {
  if (!TOOLS.includes(tool)) return;
  cancelDraft();
  commitTextEditor();
  if (prefs.tool === 'crop' && tool !== 'crop') { cropDraft = null; }
  prefs.tool = tool; savePrefs();
  if (tool !== 'select') selection = null;
  if (ocr.active) setOcr(false);
  for (const button of document.querySelectorAll('[data-tool]')) button.setAttribute('aria-checked', String(button.dataset.tool === tool));
  el.stage.dataset.tool = tool;
  updateOptions();
  render();
}

/** The mark the options bar edits: the selected one, while the select tool is in use. */
function selectedMark() {
  return prefs.tool === 'select' && selection !== null ? doc.marks[selection] ?? null : null;
}

/** The highlighter is drawn four times wider than the width setting (at least 12 px). */
const highlighterWidth = (setting) => Math.max(12, setting * 4);

function updateOptions() {
  const mark = selectedMark();
  // With a mark selected, the bar shows that mark's options and values.
  const tool = mark ? mark.type : prefs.tool;
  const sized = STROKE_TOOLS.has(tool) || tool === 'text' || tool === 'emoji';
  el.colors.hidden = !(STROKE_TOOLS.has(tool) || tool === 'text');
  el.widthOption.hidden = !sized || ocr.active;
  el.outlineOption.hidden = !OUTLINE_TOOLS.has(tool);
  el.fillOption.hidden = !(tool === 'rect' || tool === 'ellipse');
  el.emojiOption.hidden = tool !== 'emoji';
  el.cropOption.hidden = tool !== 'crop';
  el.ocrOption.hidden = !ocr.active;
  for (const node of [el.colors, el.outlineOption, el.fillOption, el.emojiOption, el.cropOption]) if (ocr.active) node.hidden = true;

  const markWidth = !mark ? prefs.width
    : mark.type === 'highlighter' ? mark.setting ?? Math.min(24, Math.max(1, Math.round(mark.width / 4)))
      : mark.width ?? prefs.width;
  const [label, min, max, value] = tool === 'text' ? ['Size', 12, 120, mark?.size ?? prefs.textSize]
    : tool === 'emoji' ? ['Size', 24, 256, mark?.size ?? prefs.emojiSize]
      : ['Width', 1, 24, markWidth];
  el.widthLabel.textContent = label;
  el.widthRange.min = String(min); el.widthRange.max = String(max); el.widthRange.value = String(value);
  el.widthValue.textContent = String(value);
  el.outlineSwitch.checked = mark && 'outline' in mark ? mark.outline : prefs.outline;
  el.fillSwitch.checked = mark && 'fill' in mark ? mark.fill : prefs.fill;
  const color = mark?.color ?? prefs.color;
  for (const swatch of el.colors.querySelectorAll('.swatch[data-color]')) swatch.setAttribute('aria-checked', String(swatch.dataset.color === color));
  if (el.customColor && /^#[0-9a-f]{6}$/i.test(color)) el.customColor.value = color;
  el.emojiCurrent.textContent = mark?.type === 'emoji' ? mark.char : prefs.emoji;
}

/** Which size preference the slider changes for the tool (or selected mark) in use. */
function setSizePreference(value) {
  const type = selectedMark()?.type ?? prefs.tool;
  if (type === 'text') prefs.textSize = value;
  else if (type === 'emoji') prefs.emojiSize = value;
  else prefs.width = value;
  savePrefs();
}

function buildPalette() {
  for (const color of COLORS) {
    const swatch = document.createElement('button');
    swatch.type = 'button'; swatch.className = 'swatch'; swatch.dataset.color = color;
    swatch.style.background = color; swatch.setAttribute('role', 'radio'); swatch.title = color;
    swatch.addEventListener('click', () => setColor(color));
    el.colors.append(swatch);
  }
  const custom = document.createElement('label');
  custom.className = 'swatch swatch-custom'; custom.title = 'Any colour';
  const input = document.createElement('input'); input.type = 'color';
  el.customColor = input;
  // While the picker is dragged the selected mark only previews; the choice is one undo step.
  let before = null;
  input.addEventListener('input', () => {
    if (selection !== null && doc.marks[selection] && 'color' in doc.marks[selection]) {
      before ??= doc.marks[selection].color;
      doc.marks[selection] = { ...doc.marks[selection], color: input.value };
      render();
    }
  });
  input.addEventListener('change', () => {
    if (before !== null && selection !== null && doc.marks[selection]) doc.marks[selection] = { ...doc.marks[selection], color: before };
    before = null;
    setColor(input.value);
  });
  custom.append(input); el.colors.append(custom);
}

/** Changing an option also restyles the selected mark, like any drawing app. */
function setColor(color) { prefs.color = color; savePrefs(); updateOptions(); restyleSelection({ color }); }

function restyleSelection(change) {
  if (selection === null) return;
  const mark = doc.marks[selection];
  const patch = {};
  if ('color' in change && 'color' in mark) patch.color = change.color;
  if ('outline' in change && 'outline' in mark) patch.outline = change.outline;
  if ('fill' in change && 'fill' in mark) patch.fill = change.fill;
  if ('size' in change) {
    if (mark.type === 'text' || mark.type === 'emoji') patch.size = change.size;
    else if (mark.type === 'highlighter') { patch.width = highlighterWidth(change.size); patch.setting = change.size; }
    else if ('width' in mark) patch.width = change.size;
  }
  if ('char' in change && mark.type === 'emoji') patch.char = change.char;
  const changed = Object.keys(patch).some((key) => mark[key] !== patch[key]);
  if (!changed) return;
  doc.marks[selection] = { ...mark, ...patch };
  commit(); updateOptions(); render();
}

function buildEmojiPicker() {
  for (const char of EMOJI) {
    const button = document.createElement('button');
    button.type = 'button'; button.textContent = char; button.title = char;
    button.addEventListener('click', () => chooseEmoji(char));
    el.emojiGrid.append(button);
  }
  el.emojiButton.addEventListener('click', () => {
    const rect = el.emojiButton.getBoundingClientRect();
    el.emojiPicker.style.left = `${rect.left}px`; el.emojiPicker.style.top = `${rect.bottom + 6}px`;
    setEmojiPicker(el.emojiPicker.hidden);
  });
  el.emojiPicker.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !event.isComposing) { event.stopPropagation(); setEmojiPicker(false); el.emojiButton.focus(); }
  });
  // Win + . inserts any system emoji into this field; the last grapheme becomes the stamp. Text
  // still being composed by an input method is not taken yet.
  el.emojiInput.addEventListener('input', (event) => {
    if (event.isComposing) return;
    const graphemes = [...new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(el.emojiInput.value)];
    const last = graphemes.at(-1)?.segment.trim();
    if (last) chooseEmoji(last);
  });
  document.addEventListener('pointerdown', (event) => {
    if (!el.emojiPicker.hidden && !el.emojiPicker.contains(event.target) && !el.emojiButton.contains(event.target)) setEmojiPicker(false);
  });
}

function setEmojiPicker(open) {
  el.emojiPicker.hidden = !open;
  el.emojiButton.setAttribute('aria-expanded', String(open));
  if (open) el.emojiGrid.querySelector('button')?.focus();
}

function chooseEmoji(char) {
  prefs.emoji = char; savePrefs();
  setEmojiPicker(false); el.emojiInput.value = '';
  // A selected stamp changes in place; otherwise the next click stamps the new emoji.
  if (selectedMark()?.type === 'emoji') { restyleSelection({ char }); return; }
  updateOptions();
  setTool('emoji');
}

// ---------- pointer input ----------

function newMark(tool, p) {
  const base = { color: prefs.color, width: prefs.width, outline: prefs.outline };
  switch (tool) {
    case 'pen': return { type: 'pen', points: [[p.x, p.y]], ...base };
    case 'highlighter': return { type: 'highlighter', points: [[p.x, p.y]], color: prefs.color, width: highlighterWidth(prefs.width), setting: prefs.width, outline: false };
    case 'arrow': case 'line': return { type: tool, x1: p.x, y1: p.y, x2: p.x, y2: p.y, ...base };
    case 'rect': case 'ellipse': return { type: tool, x: p.x, y: p.y, w: 0, h: 0, fill: prefs.fill, ...base };
    case 'blur': return { type: 'blur', x: p.x, y: p.y, w: 0, h: 0 };
    default: return null;
  }
}

let activePointer = null; // the one pointer a gesture follows; others are ignored meanwhile

el.canvas.addEventListener('pointerdown', (event) => {
  if (event.button !== 0 || !doc.base || ocr.active) return;
  if (activePointer !== null && activePointer !== event.pointerId) return;
  const p = toImage(event);
  const tool = prefs.tool;
  if (tool === 'text') { openTextEditor(p); event.preventDefault(); return; }
  commitTextEditor();
  if (tool === 'emoji') {
    doc.marks.push({ type: 'emoji', x: p.x, y: p.y, char: prefs.emoji, size: prefs.emojiSize });
    commit(); render(); return;
  }
  el.canvas.setPointerCapture(event.pointerId);
  activePointer = event.pointerId;
  if (tool === 'select') {
    selection = hit(p);
    draft = selection === null ? null : { kind: 'move', start: p, original: doc.marks[selection], moved: false };
    updateOptions(); render(); return;
  }
  if (tool === 'crop') { draft = { kind: 'crop', start: p }; cropDraft = { x: p.x, y: p.y, w: 0, h: 0 }; render(); return; }
  draft = { kind: 'draw', start: p, mark: newMark(tool, p) };
});

el.canvas.addEventListener('pointermove', (event) => {
  if (!draft || event.pointerId !== activePointer) return;
  updateDraft(event);
  render();
});

/** Moves the gesture in progress to the pointer's position. */
function updateDraft(event) {
  const p = toImage(event);
  if (draft.kind === 'move') {
    const dx = p.x - draft.start.x; const dy = p.y - draft.start.y;
    doc.marks[selection] = moveMark(draft.original, dx, dy);
    if (dx !== 0 || dy !== 0) draft.moved = true; // a plain click selects without a history step
  } else if (draft.kind === 'crop') {
    // Only what the editor shows can be kept: a crop of a cropped picture stays inside it.
    // Both corners are rounded to whole pixels inside the area first, so the crop can never take
    // a column or row the previous crop left out.
    const a = area();
    const clamp = (value, low, high) => Math.min(Math.max(Math.round(value), low), high);
    const [sx, sy] = [clamp(draft.start.x, a.x, a.x + a.w), clamp(draft.start.y, a.y, a.y + a.h)];
    cropDraft = normalizeRect(sx, sy, clamp(p.x, a.x, a.x + a.w), clamp(p.y, a.y, a.y + a.h));
  } else {
    const mark = draft.mark;
    if (mark.points) mark.points.push([p.x, p.y]);
    else if ('x1' in mark) {
      mark.x2 = p.x; mark.y2 = p.y;
      if (event.shiftKey) { // snap to 45°
        const angle = Math.round(Math.atan2(p.y - mark.y1, p.x - mark.x1) / (Math.PI / 4)) * (Math.PI / 4);
        const length = Math.hypot(p.x - mark.x1, p.y - mark.y1);
        mark.x2 = mark.x1 + length * Math.cos(angle); mark.y2 = mark.y1 + length * Math.sin(angle);
      }
    } else {
      let w = p.x - draft.start.x; let h = p.y - draft.start.y;
      if (event.shiftKey) { const s = Math.max(Math.abs(w), Math.abs(h)); w = Math.sign(w || 1) * s; h = Math.sign(h || 1) * s; }
      Object.assign(mark, normalizeRect(draft.start.x, draft.start.y, draft.start.x + w, draft.start.y + h));
    }
  }
}

/** A mark too small to mean anything: a click with a line, arrow, shape or pixelate tool. */
function tooSmall(mark) {
  if (mark.points) return false; // a pen or highlighter dot is intended
  if ('x1' in mark) return Math.hypot(mark.x2 - mark.x1, mark.y2 - mark.y1) < 3;
  return mark.w < 3 || mark.h < 3;
}

function finishPointer(event) {
  if (event.pointerId !== activePointer) return;
  activePointer = null;
  if (!draft) return;
  updateDraft(event); // the release point counts, not only the last move
  if (draft.kind === 'move') { if (draft.moved) commit(); }
  else if (draft.kind === 'draw') {
    const mark = draft.mark;
    if (!tooSmall(mark)) { doc.marks.push(mark); commit(); }
  }
  draft = null;
  render();
}

/** Drops the gesture in progress: a move puts the mark back; nothing reaches the history. */
function cancelDraft() {
  if (draft?.kind === 'move' && selection !== null && doc.marks[selection]) doc.marks[selection] = draft.original;
  if (draft?.kind === 'crop') cropDraft = null;
  if (activePointer !== null && el.canvas.hasPointerCapture(activePointer)) el.canvas.releasePointerCapture(activePointer);
  activePointer = null;
  draft = null;
}
el.canvas.addEventListener('pointerup', finishPointer);
el.canvas.addEventListener('pointercancel', (event) => {
  if (event.pointerId !== activePointer) return;
  cancelDraft(); render();
});
el.canvas.addEventListener('lostpointercapture', (event) => {
  if (event.pointerId === activePointer && draft) { cancelDraft(); render(); }
});
el.canvas.addEventListener('dblclick', (event) => {
  // Only the select and text tools reopen text: with a drawing tool the clicks drew something.
  if (ocr.active || (prefs.tool !== 'select' && prefs.tool !== 'text')) return;
  const index = hit(toImage(event));
  if (index !== null && doc.marks[index].type === 'text') {
    const mark = doc.marks.splice(index, 1)[0];
    selection = null; updateOptions(); render();
    openTextEditor({ x: mark.x, y: mark.y }, mark, index);
  }
});

// ---------- text ----------

let textAnchor = null;
/** `index` is where an edited text mark came from, so it goes back to the same layer. */
function openTextEditor(p, existing = null, index = null) {
  commitTextEditor();
  textAnchor = { x: p.x, y: p.y, existing, index };
  el.textEditor.style.color = existing?.color ?? prefs.color;
  el.textEditor.value = existing?.text ?? '';
  el.textEditor.hidden = false;
  placeTextEditor();
  el.textEditor.focus();
}
/** Keeps the text box over its anchor and at the text's size when the zoom changes. */
function placeTextEditor() {
  if (el.textEditor.hidden || !textAnchor) return;
  const a = area();
  const size = textAnchor.existing?.size ?? prefs.textSize;
  Object.assign(el.textEditor.style, {
    left: `${(textAnchor.x - a.x) * view.zoom}px`, top: `${(textAnchor.y - a.y) * view.zoom}px`,
    fontSize: `${size * view.zoom}px`,
  });
  autosizeText();
}
function autosizeText() {
  el.textEditor.style.width = '0px'; el.textEditor.style.height = '0px';
  el.textEditor.style.width = `${el.textEditor.scrollWidth + 8}px`;
  el.textEditor.style.height = `${el.textEditor.scrollHeight}px`;
}
function commitTextEditor() {
  if (el.textEditor.hidden || !textAnchor) return;
  const text = el.textEditor.value.replace(/\s+$/, '');
  const { x, y, existing, index } = textAnchor;
  textAnchor = null; // first, so the blur that hiding the box causes does not commit twice
  el.textEditor.hidden = true;
  if (text) {
    const mark = existing
      ? { ...existing, text }
      : { type: 'text', x, y, text, color: prefs.color, size: prefs.textSize, outline: prefs.outline };
    doc.marks.splice(index ?? doc.marks.length, 0, mark);
  }
  // Unchanged text needs no history entry; removed text (emptied box) does.
  if (existing ? text !== existing.text : Boolean(text)) commit();
  reportUnsaved();
  render();
}
el.textEditor.addEventListener('input', () => { autosizeText(); reportUnsaved(); });
el.textEditor.addEventListener('keydown', (event) => {
  // An input method (Chinese, Japanese…) uses Enter and Escape for its own candidates.
  if (event.isComposing || event.keyCode === 229) return;
  // Saving and copying still work while typing: the text is finished first.
  if (event.ctrlKey && (event.key.toLowerCase() === 's')) { event.preventDefault(); commitTextEditor(); void exportImage(event.shiftKey ? 'save-as' : 'save'); return; }
  event.stopPropagation();
  if (event.key === 'Escape') { el.textEditor.value = textAnchor?.existing?.text ?? ''; commitTextEditor(); }
  if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); commitTextEditor(); }
});
el.textEditor.addEventListener('blur', commitTextEditor);

// ---------- crop ----------

function applyCrop() {
  if (!cropDraft || cropDraft.w < 4 || cropDraft.h < 4) { setStatus('Drag over the image to choose the area to keep.'); return; }
  doc.crop = { x: cropDraft.x, y: cropDraft.y, w: cropDraft.w, h: cropDraft.h }; // already whole pixels inside the area
  cropDraft = null; commit(); fit(); setTool('select');
}
function resetCrop() { cropDraft = null; if (doc.crop) { doc.crop = null; commit(); } fit(); }

// ---------- text recognition ----------

async function loadLanguages() {
  try {
    const languages = await invoke('ocr_languages');
    el.ocrLanguage.replaceChildren(new Option('Auto', ''));
    for (const language of languages) el.ocrLanguage.append(new Option(language.name || language.tag, language.tag));
    el.ocrLanguage.value = languages.some((l) => l.tag === prefs.ocrLanguage) ? prefs.ocrLanguage : '';
    if (!languages.length) el.textButton.title = 'Windows has no text recognition language installed';
  } catch (error) {
    el.textButton.disabled = true;
    el.textButton.title = errorText(error);
  }
}

async function setOcr(active) {
  ocr.active = active;
  el.textButton.setAttribute('aria-pressed', String(active));
  el.textLayer.hidden = !active;
  updateOptions();
  const request = ++ocrRequest; // any earlier request's answer is now stale
  if (!active) { el.textLayer.replaceChildren(); window.getSelection()?.removeAllRanges(); return; }
  cancelDraft(); commitTextEditor(); selection = null; render();
  const language = el.ocrLanguage.value;
  if (ocr.result && ocr.language === language) { layoutTextLayer(); return; }
  el.ocrStatus.textContent = 'Reading text…';
  $('copyTextButton').disabled = true;
  try {
    // Read what the editor shows, marks and crop included, like the picture that would be saved.
    const a = area();
    const result = await invoke('editor_ocr', await flatten(), { headers: { 'snippy-language': language } });
    // Turned off, asked again (another language) or another picture loaded meanwhile: drop it.
    if (!ocr.active || request !== ocrRequest) return;
    ocr.result = result;
    ocr.origin = { x: a.x, y: a.y };
    ocr.language = language;
    const words = ocr.result.lines.reduce((sum, line) => sum + line.words.length, 0);
    const lines = ocr.result.lines.length;
    el.ocrStatus.textContent = words
      ? `${lines} ${lines === 1 ? 'line' : 'lines'} · select words with the mouse, then Ctrl+C`
      : 'No text found';
    layoutTextLayer();
    $('copyTextButton').disabled = !words;
  } catch (error) {
    if (request !== ocrRequest) return;
    el.ocrStatus.textContent = '';
    setStatus(errorText(error), true);
  }
}

/** Places recognised words as transparent, selectable text over the picture. */
function layoutTextLayer() {
  if (!ocr.result) return;
  const a = area();
  const z = view.zoom;
  const fragment = document.createDocumentFragment();
  ocr.result.lines.forEach((line, lineIndex) => {
    line.words.forEach((word, wordIndex) => {
      const span = document.createElement('span');
      span.className = 'ocr-word';
      span.textContent = word.text + (wordIndex < line.words.length - 1 ? ' ' : '');
      const o = ocr.origin ?? { x: 0, y: 0 };
      Object.assign(span.style, { left: `${(word.x + o.x - a.x) * z}px`, top: `${(word.y + o.y - a.y) * z}px`, height: `${word.height * z}px`, fontSize: `${word.height * z}px` });
      span.dataset.width = String(word.width * z);
      fragment.append(span);
    });
    if (lineIndex < ocr.result.lines.length - 1) fragment.append(document.createTextNode('\n'));
  });
  el.textLayer.replaceChildren(fragment);
  // Stretch each word to its measured box so the selection matches the picture.
  for (const span of el.textLayer.querySelectorAll('.ocr-word')) {
    const natural = span.getBoundingClientRect().width || 1;
    span.style.transform = `scaleX(${Number(span.dataset.width) / natural})`;
  }
}

async function copyAllText() {
  if (!ocr.result) return;
  const text = ocr.result.lines.map((line) => line.text).join('\n');
  try { await navigator.clipboard.writeText(text); setStatus('Text copied'); } catch (error) { setStatus(errorText(error), true); }
}

// ---------- export ----------

async function flatten() {
  const a = area();
  const out = document.createElement('canvas');
  out.width = a.w; out.height = a.h;
  const o = out.getContext('2d');
  o.translate(-a.x, -a.y);
  o.drawImage(doc.base, 0, 0);
  doc.marks.forEach((mark) => drawMark(o, mark));
  const blob = await new Promise((resolve) => out.toBlob(resolve, 'image/png'));
  // The browser gives no picture back when the canvas is too large for it.
  if (!blob) throw new Error('The picture is too large to export');
  return new Uint8Array(await blob.arrayBuffer());
}

async function exportImage(action) {
  commitTextEditor();
  if (!doc.base || exporting) return;
  exporting = true;
  // What is being exported, fixed now: edits or another picture may follow while it encodes.
  const revision = currentRevision();
  const documentId = doc.id;
  try {
    setStatus(action === 'copy' ? 'Copying…' : 'Saving…');
    const png = await flatten();
    const path = await invoke('editor_export', png, {
      headers: { 'snippy-action': action, 'snippy-document': String(documentId) },
    });
    if (doc.id !== documentId) return; // another picture is open now; this result is not its
    if (action === 'copy') setStatus('Image copied to the clipboard');
    else setStatus(path ? `Saved ${path}` : 'Not saved');
    // Copied or saved, this state of the picture is safe to close without asking.
    if (action === 'copy' || path) { savedRevision = revision; reportUnsaved(); }
  } catch (error) {
    setStatus(errorText(error), true);
  } finally {
    exporting = false;
  }
}

// ---------- zoom ----------

function fit() {
  const a = area();
  const box = el.stage.getBoundingClientRect();
  view.zoom = Math.min(1, (box.width - 48) / a.w, (box.height - 48) / a.h);
  view.zoom = Math.min(maxZoom(), Math.max(0.01, view.zoom)); // a very wide picture still fits
  view.fit = true;
  render();
}
/** Canvas limits in Chromium: 32767 px a side and 2^28 pixels in all; stay below both. */
const CANVAS_SIDE_LIMIT = 16384;
const CANVAS_AREA_LIMIT = 2 ** 27;
function maxZoom() {
  const a = area();
  const dpr = window.devicePixelRatio || 1;
  const bySide = CANVAS_SIDE_LIMIT / (Math.max(a.w, a.h) * dpr);
  const byArea = Math.sqrt(CANVAS_AREA_LIMIT / (a.w * a.h)) / dpr;
  return Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, bySide, byArea));
}
function zoomBy(factor) { view.zoom = Math.min(maxZoom(), Math.max(MIN_ZOOM, view.zoom * factor)); view.fit = false; render(); }

el.stage.addEventListener('wheel', (event) => {
  if (!event.ctrlKey) return;
  event.preventDefault();
  zoomBy(event.deltaY < 0 ? 1.15 : 1 / 1.15);
}, { passive: false });
window.addEventListener('resize', () => { if (view.fit) fit(); });

// ---------- keyboard ----------

const TYPING_INPUTS = new Set(['text', 'search', 'email', 'url', 'number', 'password']);
document.addEventListener('keydown', (event) => {
  const target = event.target;
  if ((target instanceof HTMLInputElement && TYPING_INPUTS.has(target.type)) || target instanceof HTMLSelectElement || event.isComposing) return;
  const key = event.key.toLowerCase();
  if (event.ctrlKey && key === 'z') { event.preventDefault(); event.shiftKey ? redo() : undo(); return; }
  if (event.ctrlKey && key === 'y') { event.preventDefault(); redo(); return; }
  if (event.ctrlKey && key === 's') { event.preventDefault(); void exportImage(event.shiftKey ? 'save-as' : 'save'); return; }
  if (event.ctrlKey && key === 'c') {
    if (ocr.active && String(window.getSelection())) return; // native copy of the selected words
    event.preventDefault(); void exportImage('copy'); return;
  }
  if (event.ctrlKey && key === 'a' && ocr.active) { event.preventDefault(); window.getSelection()?.selectAllChildren(el.textLayer); return; }
  if (event.ctrlKey && (key === '=' || key === '+')) { event.preventDefault(); zoomBy(1.25); return; }
  if (event.ctrlKey && key === '-') { event.preventDefault(); zoomBy(0.8); return; }
  if (event.ctrlKey && key === '0') { event.preventDefault(); fit(); return; }
  if (event.ctrlKey || event.altKey || event.metaKey) return;
  if ((key === 'delete' || key === 'backspace') && selection !== null) { cancelDraft(); doc.marks.splice(selection, 1); selection = null; commit(); updateOptions(); render(); return; }
  // Enter on a focused button presses that button; on the picture it applies the crop.
  if (key === 'enter' && prefs.tool === 'crop' && !(target instanceof HTMLButtonElement)) { applyCrop(); return; }
  if (key === 'escape') {
    if (!el.emojiPicker.hidden) { setEmojiPicker(false); return; }
    if (ocr.active) { void setOcr(false); return; }
    cancelDraft(); selection = null; cropDraft = null; updateOptions(); render(); return;
  }
  if (key === 'o') { if (!el.textButton.disabled) void setOcr(!ocr.active); return; }
  if (TOOL_KEYS[key] && !(target instanceof HTMLInputElement && target.type === 'color')) setTool(TOOL_KEYS[key]);
});
// Moving between monitors changes the pixel ratio: redraw sharp at the same zoom.
matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`).addEventListener?.('change', function onRatio() {
  render();
  matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`).addEventListener('change', onRatio, { once: true });
}, { once: true });

// ---------- wiring ----------

function bind() {
  for (const button of document.querySelectorAll('[data-tool]')) button.addEventListener('click', () => setTool(button.dataset.tool));
  el.undo.addEventListener('click', undo);
  el.redo.addEventListener('click', redo);
  el.textButton.addEventListener('click', () => void setOcr(!ocr.active));
  el.ocrLanguage.addEventListener('change', () => { prefs.ocrLanguage = el.ocrLanguage.value; savePrefs(); ocr.result = null; void setOcr(true); });
  $('copyTextButton').addEventListener('click', copyAllText);
  $('copyButton').addEventListener('click', () => void exportImage('copy'));
  $('saveButton').addEventListener('click', () => void exportImage('save'));
  $('saveAsButton').addEventListener('click', () => void exportImage('save-as'));
  $('applyCropButton').addEventListener('click', applyCrop);
  $('resetCropButton').addEventListener('click', resetCrop);
  el.widthRange.addEventListener('input', () => {
    const value = Number(el.widthRange.value);
    setSizePreference(value);
    el.widthValue.textContent = String(value);
  });
  el.widthRange.addEventListener('change', () => restyleSelection({ size: Number(el.widthRange.value) }));
  el.outlineSwitch.addEventListener('change', () => { prefs.outline = el.outlineSwitch.checked; savePrefs(); restyleSelection({ outline: prefs.outline }); });
  el.fillSwitch.addEventListener('change', () => { prefs.fill = el.fillSwitch.checked; savePrefs(); restyleSelection({ fill: prefs.fill }); });
}

async function load() {
  const generation = ++loadGeneration;
  const info = await invoke('editor_info');
  // The pixels of exactly this document; if another picture was opened meanwhile, that load wins.
  const buffer = await invoke('editor_pixels', { document: info.id });
  if (generation !== loadGeneration) return;
  const pixels = new Uint8ClampedArray(buffer);
  if (pixels.length !== info.width * info.height * 4) throw new Error('The image data is incomplete');
  doc.image = new ImageData(pixels, info.width, info.height);
  doc.base = document.createElement('canvas');
  doc.base.width = info.width; doc.base.height = info.height;
  doc.base.getContext('2d').putImageData(doc.image, 0, 0);
  // A text box still open belongs to the previous picture: close it without keeping its text.
  textAnchor = null; el.textEditor.hidden = true; cancelDraft(); setEmojiPicker(false);
  Object.assign(doc, { id: info.id, width: info.width, height: info.height, name: info.name, marks: [], crop: null });
  history.past = [snapshot()]; history.future = []; updateHistoryButtons();
  savedRevision = currentRevision(); reportedUnsaved = true; reportUnsaved();
  ocrRequest += 1;
  selection = null; cropDraft = null; ocr = { active: false, result: null, language: null };
  el.textLayer.hidden = true; el.textLayer.replaceChildren(); el.textButton.setAttribute('aria-pressed', 'false');
  el.ocrStatus.textContent = '';
  updateOptions();
  document.title = 'Snippy | Editor';
  setStatus(info.path ?? 'Unsaved snip — Save stores it in your screenshots folder.');
  fit();
}

async function start() {
  if (typeof invoke !== 'function') { setStatus('Open the editor from Snippy.', true); return; }
  buildPalette(); buildEmojiPicker(); bind();
  setTool(prefs.tool);
  void loadLanguages();
  await listen('editor-load', () => load().catch((error) => setStatus(errorText(error), true)));
  try { await load(); } catch (error) { setStatus(errorText(error), true); }
  // Shown only once the picture is painted, so the window never opens empty.
  const current = tauri.window.getCurrentWindow();
  await current.show(); await current.setFocus();
}

start().catch((error) => setStatus(errorText(error), true));
