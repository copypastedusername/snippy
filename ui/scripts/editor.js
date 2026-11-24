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

const doc = { image: null, base: null, width: 0, height: 0, name: '', marks: [], crop: null };
const view = { zoom: 1, fit: true };
const history = { past: [], future: [] };
let selection = null; // index into doc.marks
let draft = null; // mark being drawn, or { kind: 'crop' | 'move', ... }
let cropDraft = null;
let ocr = { active: false, result: null, language: null };

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
      const xs = mark.points.map((p) => p[0]); const ys = mark.points.map((p) => p[1]);
      const pad = mark.width / 2;
      return { x: Math.min(...xs) - pad, y: Math.min(...ys) - pad, w: Math.max(...xs) - Math.min(...xs) + 2 * pad, h: Math.max(...ys) - Math.min(...ys) + 2 * pad };
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

function hit(point) {
  for (let index = doc.marks.length - 1; index >= 0; index -= 1) {
    const b = bounds(doc.marks[index]);
    const slack = 6 / view.zoom;
    if (point.x >= b.x - slack && point.x <= b.x + b.w + slack && point.y >= b.y - slack && point.y <= b.y + b.h + slack) return index;
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

/** Hides an area by painting it in coarse blocks taken from the original pixels. */
function pixelate(target, mark) {
  const x0 = Math.max(0, Math.floor(mark.x)); const y0 = Math.max(0, Math.floor(mark.y));
  const x1 = Math.min(doc.width, Math.ceil(mark.x + mark.w)); const y1 = Math.min(doc.height, Math.ceil(mark.y + mark.h));
  const block = Math.max(PIXEL_BLOCK, Math.round(Math.min(mark.w, mark.h) / 12));
  for (let y = y0; y < y1; y += block) {
    for (let x = x0; x < x1; x += block) {
      const i = ((Math.min(y + (block >> 1), y1 - 1)) * doc.width + Math.min(x + (block >> 1), x1 - 1)) * 4;
      const d = doc.image.data;
      target.fillStyle = `rgb(${d[i]},${d[i + 1]},${d[i + 2]})`;
      target.fillRect(x, y, Math.min(block, x1 - x), Math.min(block, y1 - y));
    }
  }
}

/** Paints the document (and editing chrome) at the current zoom. */
function render() {
  if (!doc.base) return;
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
}

// ---------- history ----------

function snapshot() { return { marks: structuredClone(doc.marks), crop: doc.crop && { ...doc.crop } }; }
function commit() {
  ocr.result = null; // the picture changed, so earlier recognition is stale
  history.past.push(snapshot());
  if (history.past.length > HISTORY_LIMIT) history.past.shift();
  history.future = [];
  updateHistoryButtons();
}
function restore(state) { doc.marks = state.marks; doc.crop = state.crop; selection = null; ocr.result = null; fit(); }
function undo() { if (history.past.length < 2) return; history.future.push(history.past.pop()); restore(structuredClone(history.past.at(-1))); updateHistoryButtons(); }
function redo() { const next = history.future.pop(); if (!next) return; history.past.push(next); restore(structuredClone(next)); updateHistoryButtons(); }
function updateHistoryButtons() { el.undo.disabled = history.past.length < 2; el.redo.disabled = history.future.length === 0; }

// ---------- tools and options ----------

function setTool(tool) {
  if (!TOOLS.includes(tool)) return;
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

function updateOptions() {
  const tool = prefs.tool;
  const sized = STROKE_TOOLS.has(tool) || tool === 'text' || tool === 'emoji';
  el.colors.hidden = !(STROKE_TOOLS.has(tool) || tool === 'text');
  el.widthOption.hidden = !sized || ocr.active;
  el.outlineOption.hidden = !OUTLINE_TOOLS.has(tool);
  el.fillOption.hidden = !(tool === 'rect' || tool === 'ellipse');
  el.emojiOption.hidden = tool !== 'emoji';
  el.cropOption.hidden = tool !== 'crop';
  el.ocrOption.hidden = !ocr.active;
  for (const node of [el.colors, el.outlineOption, el.fillOption, el.emojiOption, el.cropOption]) if (ocr.active) node.hidden = true;

  const [label, min, max, value] = tool === 'text' ? ['Size', 12, 120, prefs.textSize]
    : tool === 'emoji' ? ['Size', 24, 256, prefs.emojiSize]
      : ['Width', 1, 24, prefs.width];
  el.widthLabel.textContent = label;
  el.widthRange.min = String(min); el.widthRange.max = String(max); el.widthRange.value = String(value);
  el.widthValue.textContent = String(value);
  el.outlineSwitch.checked = prefs.outline;
  el.fillSwitch.checked = prefs.fill;
  for (const swatch of el.colors.querySelectorAll('.swatch[data-color]')) swatch.setAttribute('aria-checked', String(swatch.dataset.color === prefs.color));
  el.emojiCurrent.textContent = prefs.emoji;
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
  input.addEventListener('input', () => setColor(input.value));
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
    else if ('width' in mark) patch.width = change.size;
  }
  if (!Object.keys(patch).length) return;
  doc.marks[selection] = { ...mark, ...patch };
  commit(); render();
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
    el.emojiPicker.hidden = !el.emojiPicker.hidden;
  });
  // Win + . inserts any system emoji into this field; the last grapheme becomes the stamp.
  el.emojiInput.addEventListener('input', () => {
    const graphemes = [...new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(el.emojiInput.value)];
    const last = graphemes.at(-1)?.segment.trim();
    if (last) chooseEmoji(last);
  });
  document.addEventListener('pointerdown', (event) => {
    if (!el.emojiPicker.hidden && !el.emojiPicker.contains(event.target) && !el.emojiButton.contains(event.target)) el.emojiPicker.hidden = true;
  });
}

function chooseEmoji(char) {
  prefs.emoji = char; savePrefs(); updateOptions();
  el.emojiPicker.hidden = true; el.emojiInput.value = '';
  setTool('emoji');
}

// ---------- pointer input ----------

function newMark(tool, p) {
  const base = { color: prefs.color, width: prefs.width, outline: prefs.outline };
  switch (tool) {
    case 'pen': return { type: 'pen', points: [[p.x, p.y]], ...base };
    case 'highlighter': return { type: 'highlighter', points: [[p.x, p.y]], color: prefs.color, width: Math.max(12, prefs.width * 4), outline: false };
    case 'arrow': case 'line': return { type: tool, x1: p.x, y1: p.y, x2: p.x, y2: p.y, ...base };
    case 'rect': case 'ellipse': return { type: tool, x: p.x, y: p.y, w: 0, h: 0, fill: prefs.fill, ...base };
    case 'blur': return { type: 'blur', x: p.x, y: p.y, w: 0, h: 0 };
    default: return null;
  }
}

el.canvas.addEventListener('pointerdown', (event) => {
  if (event.button !== 0 || !doc.base || ocr.active) return;
  const p = toImage(event);
  const tool = prefs.tool;
  if (tool === 'text') { openTextEditor(p); event.preventDefault(); return; }
  commitTextEditor();
  if (tool === 'emoji') {
    doc.marks.push({ type: 'emoji', x: p.x, y: p.y, char: prefs.emoji, size: prefs.emojiSize });
    commit(); render(); return;
  }
  el.canvas.setPointerCapture(event.pointerId);
  if (tool === 'select') {
    selection = hit(p);
    draft = selection === null ? null : { kind: 'move', start: p, original: doc.marks[selection], moved: false };
    render(); return;
  }
  if (tool === 'crop') { draft = { kind: 'crop', start: p }; cropDraft = { x: p.x, y: p.y, w: 0, h: 0 }; render(); return; }
  draft = { kind: 'draw', start: p, mark: newMark(tool, p) };
});

el.canvas.addEventListener('pointermove', (event) => {
  if (!draft) return;
  const p = toImage(event);
  if (draft.kind === 'move') {
    doc.marks[selection] = moveMark(draft.original, p.x - draft.start.x, p.y - draft.start.y);
    draft.moved = true;
  } else if (draft.kind === 'crop') {
    const a = { x: 0, y: 0, w: doc.width, h: doc.height };
    const r = normalizeRect(draft.start.x, draft.start.y, Math.min(Math.max(p.x, a.x), a.w), Math.min(Math.max(p.y, a.y), a.h));
    cropDraft = r;
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
  render();
});

function finishPointer() {
  if (!draft) return;
  if (draft.kind === 'move') { if (draft.moved) commit(); }
  else if (draft.kind === 'draw') {
    const mark = draft.mark;
    const b = bounds(mark);
    const tiny = mark.points ? false : (b.w < 3 && b.h < 3);
    if (!tiny) { doc.marks.push(mark); commit(); }
  }
  draft = null;
  render();
}
el.canvas.addEventListener('pointerup', finishPointer);
el.canvas.addEventListener('pointercancel', () => { draft = null; render(); });
el.canvas.addEventListener('dblclick', (event) => {
  const index = hit(toImage(event));
  if (index !== null && doc.marks[index].type === 'text') {
    const mark = doc.marks.splice(index, 1)[0];
    selection = null; render();
    openTextEditor({ x: mark.x, y: mark.y }, mark);
  }
});

// ---------- text ----------

let textAnchor = null;
function openTextEditor(p, existing = null) {
  commitTextEditor();
  const a = area();
  textAnchor = { x: p.x, y: p.y, existing };
  const size = existing?.size ?? prefs.textSize;
  Object.assign(el.textEditor.style, {
    left: `${(p.x - a.x) * view.zoom}px`, top: `${(p.y - a.y) * view.zoom}px`,
    fontSize: `${size * view.zoom}px`, color: existing?.color ?? prefs.color,
  });
  el.textEditor.value = existing?.text ?? '';
  el.textEditor.hidden = false;
  autosizeText();
  el.textEditor.focus();
}
function autosizeText() {
  el.textEditor.style.width = '0px'; el.textEditor.style.height = '0px';
  el.textEditor.style.width = `${el.textEditor.scrollWidth + 8}px`;
  el.textEditor.style.height = `${el.textEditor.scrollHeight}px`;
}
function commitTextEditor() {
  if (el.textEditor.hidden || !textAnchor) return;
  const text = el.textEditor.value.replace(/\s+$/, '');
  const existing = textAnchor.existing;
  el.textEditor.hidden = true;
  if (text) {
    doc.marks.push({ type: 'text', x: textAnchor.x, y: textAnchor.y, text, color: existing?.color ?? prefs.color,
      size: existing?.size ?? prefs.textSize, outline: existing?.outline ?? prefs.outline });
  }
  if (text || existing) commit();
  textAnchor = null;
  render();
}
el.textEditor.addEventListener('input', autosizeText);
el.textEditor.addEventListener('keydown', (event) => {
  event.stopPropagation();
  if (event.key === 'Escape') { el.textEditor.value = textAnchor?.existing?.text ?? ''; commitTextEditor(); }
  if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); commitTextEditor(); }
});
el.textEditor.addEventListener('blur', commitTextEditor);

// ---------- crop ----------

function applyCrop() {
  if (!cropDraft || cropDraft.w < 4 || cropDraft.h < 4) { setStatus('Drag over the image to choose the area to keep.'); return; }
  doc.crop = { x: Math.round(cropDraft.x), y: Math.round(cropDraft.y), w: Math.round(cropDraft.w), h: Math.round(cropDraft.h) };
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
  if (!active) { el.textLayer.replaceChildren(); window.getSelection()?.removeAllRanges(); return; }
  commitTextEditor(); selection = null; render();
  const language = el.ocrLanguage.value;
  if (ocr.result && ocr.language === language) { layoutTextLayer(); return; }
  el.ocrStatus.textContent = 'Reading text…';
  try {
    // Read what the editor shows, marks and crop included, like the picture that would be saved.
    const a = area();
    ocr.result = await invoke('editor_ocr', await flatten(), { headers: { 'snippy-language': language } });
    ocr.origin = { x: a.x, y: a.y };
    ocr.language = language;
    const words = ocr.result.lines.reduce((sum, line) => sum + line.words.length, 0);
    const lines = ocr.result.lines.length;
    el.ocrStatus.textContent = words
      ? `${lines} ${lines === 1 ? 'line' : 'lines'} · select words with the mouse, then Ctrl+C`
      : 'No text found';
    layoutTextLayer();
  } catch (error) {
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
  return new Uint8Array(await blob.arrayBuffer());
}

async function exportImage(action) {
  commitTextEditor();
  try {
    setStatus(action === 'copy' ? 'Copying…' : 'Saving…');
    const path = await invoke('editor_export', await flatten(), { headers: { 'snippy-action': action } });
    if (action === 'copy') setStatus('Image copied to the clipboard');
    else setStatus(path ? `Saved ${path}` : 'Not saved');
  } catch (error) {
    setStatus(errorText(error), true);
  }
}

// ---------- zoom ----------

function fit() {
  const a = area();
  const box = el.stage.getBoundingClientRect();
  view.zoom = Math.min(1, (box.width - 48) / a.w, (box.height - 48) / a.h);
  view.zoom = Math.max(MIN_ZOOM, view.zoom);
  view.fit = true;
  render();
}
function zoomBy(factor) { view.zoom = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, view.zoom * factor)); view.fit = false; render(); }

el.stage.addEventListener('wheel', (event) => {
  if (!event.ctrlKey) return;
  event.preventDefault();
  zoomBy(event.deltaY < 0 ? 1.15 : 1 / 1.15);
}, { passive: false });
window.addEventListener('resize', () => { if (view.fit) fit(); });

// ---------- keyboard ----------

document.addEventListener('keydown', (event) => {
  if (event.target instanceof HTMLInputElement || event.target instanceof HTMLSelectElement) return;
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
  if ((key === 'delete' || key === 'backspace') && selection !== null) { doc.marks.splice(selection, 1); selection = null; commit(); render(); return; }
  if (key === 'enter' && prefs.tool === 'crop') { applyCrop(); return; }
  if (key === 'escape') {
    if (ocr.active) { void setOcr(false); return; }
    selection = null; cropDraft = null; render(); return;
  }
  if (key === 'o') { void setOcr(!ocr.active); return; }
  if (TOOL_KEYS[key]) setTool(TOOL_KEYS[key]);
});

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
    if (prefs.tool === 'text') prefs.textSize = value;
    else if (prefs.tool === 'emoji') prefs.emojiSize = value;
    else prefs.width = value;
    el.widthValue.textContent = String(value);
    savePrefs();
  });
  el.widthRange.addEventListener('change', () => restyleSelection({ size: Number(el.widthRange.value) }));
  el.outlineSwitch.addEventListener('change', () => { prefs.outline = el.outlineSwitch.checked; savePrefs(); restyleSelection({ outline: prefs.outline }); });
  el.fillSwitch.addEventListener('change', () => { prefs.fill = el.fillSwitch.checked; savePrefs(); restyleSelection({ fill: prefs.fill }); });
}

async function load() {
  const info = await invoke('editor_info');
  const buffer = await invoke('editor_pixels');
  const pixels = new Uint8ClampedArray(buffer);
  if (pixels.length !== info.width * info.height * 4) throw new Error('The image data is incomplete');
  doc.image = new ImageData(pixels, info.width, info.height);
  doc.base = document.createElement('canvas');
  doc.base.width = info.width; doc.base.height = info.height;
  doc.base.getContext('2d').putImageData(doc.image, 0, 0);
  Object.assign(doc, { width: info.width, height: info.height, name: info.name, marks: [], crop: null });
  history.past = [snapshot()]; history.future = []; updateHistoryButtons();
  selection = null; cropDraft = null; ocr = { active: false, result: null, language: null };
  el.textLayer.hidden = true; el.textButton.setAttribute('aria-pressed', 'false');
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
