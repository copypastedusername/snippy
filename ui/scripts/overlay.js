const tauri = window.__TAURI__;
const invoke = tauri?.core?.invoke;
const listen = tauri?.event?.listen;

const canvas = document.getElementById('overlayCanvas');
const context = canvas.getContext('2d', { alpha: false });
const toolbar = document.getElementById('toolbar');
const closeButton = document.getElementById('closeButton');
const modeButtons = [...document.querySelectorAll('.tool-button[data-mode]')];

const MODES = new Set(['rectangle', 'freeform', 'window', 'fullscreen']);
const LOUPE_SIZE = 108;
const LOUPE_OFFSET = 16;
const LOUPE_ZOOM = 8;
// Coordinates and colour sit in a label under the circle, where nothing crops them.
const LOUPE_LABEL_GAP = 6;
const LOUPE_LABEL_HEIGHT = 22;
const LOUPE_HEIGHT = LOUPE_SIZE + LOUPE_LABEL_GAP + LOUPE_LABEL_HEIGHT;
const BADGE_GAP = 8;

const state = {
  meta: null,
  frameCanvas: null,
  framePixels: null,
  mode: 'rectangle',
  metrics: {
    rect: { left: 0, top: 0, width: 1, height: 1 },
    sx: 1,
    sy: 1,
  },
  pointerImage: null,
  pointerEdge: null,
  pointerCss: null,
  dragPointerId: null,
  clickPointerId: null,
  dragStart: null,
  // Alt held while dragging a rectangle: its sides snap to edges in the frame (snap.js).
  snapping: false,
  rectangle: null,
  freeformPoints: [],
  hoveredWindow: null,
  hoveredMonitor: null,
  showMagnifier: true,
  inFlight: false,
  animationFrame: 0,
  refreshSequence: 0,
  accent: '',
};

function clamp(value, minimum, maximum) {
  return Math.min(Math.max(value, minimum), maximum);
}

function clampImagePoint(point) {
  if (!state.meta) {
    return { x: 0, y: 0 };
  }

  return {
    x: clamp(Math.round(point.x), 0, Math.max(0, state.meta.width - 1)),
    y: clamp(Math.round(point.y), 0, Math.max(0, state.meta.height - 1)),
  };
}

function updateCanvasMetrics() {
  const rect = canvas.getBoundingClientRect();
  state.metrics = {
    rect,
    sx: rect.width > 0 ? canvas.width / rect.width : 1,
    sy: rect.height > 0 ? canvas.height / rect.height : 1,
  };
}

/**
 * The pointer as a selection boundary: 0..width, so a drag to the very edge keeps the last row and
 * column (a selection's right and bottom are exclusive). `toImage` is the pixel under the pointer.
 */
function toEdge(event) {
  const { rect, sx, sy } = state.metrics;
  const width = state.meta?.width ?? 0;
  const height = state.meta?.height ?? 0;
  return {
    x: clamp(Math.round((event.clientX - rect.left) * sx), 0, width),
    y: clamp(Math.round((event.clientY - rect.top) * sy), 0, height),
  };
}

function toImage(event) {
  const { rect, sx, sy } = state.metrics;
  return clampImagePoint({
    x: Math.round((event.clientX - rect.left) * sx),
    y: Math.round((event.clientY - rect.top) * sy),
  });
}

function toCanvasCss(event) {
  const { rect } = state.metrics;
  return {
    x: clamp(event.clientX - rect.left, 0, rect.width),
    y: clamp(event.clientY - rect.top, 0, rect.height),
  };
}

function imageRectToCss(rectangle) {
  return {
    x: rectangle.x / state.metrics.sx,
    y: rectangle.y / state.metrics.sy,
    width: rectangle.width / state.metrics.sx,
    height: rectangle.height / state.metrics.sy,
  };
}

function imagePointToCss(point) {
  return {
    x: point.x / state.metrics.sx,
    y: point.y / state.metrics.sy,
  };
}

function isDragging() {
  return state.dragPointerId !== null;
}

function requestRedraw() {
  if (state.animationFrame) {
    return;
  }

  state.animationFrame = requestAnimationFrame(() => {
    state.animationFrame = 0;
    redraw();
  });
}

// `--accent` is a light-dark() expression, which canvas cannot parse; resolving it through the
// computed `color` of the root yields a concrete rgb() value for the active scheme.
function refreshAccent() {
  const root = document.documentElement;
  const previous = root.style.color;
  root.style.color = 'var(--accent)';
  state.accent = getComputedStyle(root).color;
  root.style.color = previous;
}

function setCssDrawingSpace() {
  context.setTransform(state.metrics.sx, 0, 0, state.metrics.sy, 0, 0);
}

function beginRoundedRect(x, y, width, height, radius) {
  const safeRadius = Math.min(radius, width / 2, height / 2);
  context.beginPath();
  context.moveTo(x + safeRadius, y);
  context.arcTo(x + width, y, x + width, y + height, safeRadius);
  context.arcTo(x + width, y + height, x, y + height, safeRadius);
  context.arcTo(x, y + height, x, y, safeRadius);
  context.arcTo(x, y, x + width, y, safeRadius);
  context.closePath();
}

function pointInRect(point, rectangle) {
  return point.x >= rectangle.x
    && point.x < rectangle.x + rectangle.width
    && point.y >= rectangle.y
    && point.y < rectangle.y + rectangle.height;
}

/**
 * Whether the point is on the window itself: inside its rectangle but not in the cut-away part of a
 * rounded corner (Windows 11), where the window underneath shows through.
 */
function pointOnWindow(point, windowInfo) {
  if (!pointInRect(point, windowInfo)) return false;
  const radius = state.meta?.corner_radius ?? 0;
  if (radius <= 0) return true;
  const left = point.x - windowInfo.x;
  const right = windowInfo.x + windowInfo.width - 1 - point.x;
  const top = point.y - windowInfo.y;
  const bottom = windowInfo.y + windowInfo.height - 1 - point.y;
  const dx = Math.min(left, right);
  const dy = Math.min(top, bottom);
  if (dx >= radius || dy >= radius) return true;
  return (radius - dx) ** 2 + (radius - dy) ** 2 <= radius ** 2;
}

/** Every window under the point, topmost first. */
function windowsAt(point) {
  return state.meta?.windows?.filter((windowInfo) => pointOnWindow(point, windowInfo)) ?? [];
}

function firstWindowAt(point) {
  return windowsAt(point)[0] ?? null;
}

function monitorAt(point) {
  return state.meta?.monitors?.find((monitor) => pointInRect(point, monitor)) ?? null;
}

function preferredMonitor() {
  return state.hoveredMonitor
    ?? state.meta?.monitors?.find((monitor) => monitor.primary)
    ?? state.meta?.monitors?.[0]
    ?? null;
}

function positionToolbar(monitor = preferredMonitor()) {
  if (!monitor || !state.meta) {
    return;
  }

  const { rect, sx, sy } = state.metrics;
  const monitorCenter = rect.left + (monitor.x + monitor.width / 2) / sx;
  const monitorTop = rect.top + monitor.y / sy + 18;
  const halfWidth = toolbar.offsetWidth / 2;
  const viewportLeft = rect.left + halfWidth + 4;
  const viewportRight = rect.left + rect.width - halfWidth - 4;

  toolbar.style.left = String(clamp(monitorCenter, viewportLeft, viewportRight)) + 'px';
  toolbar.style.top = String(
    clamp(monitorTop, rect.top + 4, rect.top + rect.height - toolbar.offsetHeight - 4),
  ) + 'px';
}

function updateHover(point) {
  state.hoveredWindow = state.mode === 'window' ? firstWindowAt(point) : null;
  state.hoveredMonitor = monitorAt(point);
}

/** The rectangle being dragged, snapped to visible edges while Alt is held. */
function dragRectangle(end) {
  const rectangle = normalizedRectangle(state.dragStart, end);
  if (!state.snapping || !state.framePixels || !state.meta) {
    return rectangle;
  }
  return snapRectangle(rectangle, {
    pixels: state.framePixels,
    width: state.meta.width,
    height: state.meta.height,
  });
}

function normalizedRectangle(first, second) {
  return {
    x: Math.min(first.x, second.x),
    y: Math.min(first.y, second.y),
    width: Math.abs(second.x - first.x),
    height: Math.abs(second.y - first.y),
  };
}

function freeformBounds(points) {
  let minimumX = points[0].x;
  let maximumX = points[0].x;
  let minimumY = points[0].y;
  let maximumY = points[0].y;

  for (let index = 1; index < points.length; index += 1) {
    const point = points[index];
    minimumX = Math.min(minimumX, point.x);
    maximumX = Math.max(maximumX, point.x);
    minimumY = Math.min(minimumY, point.y);
    maximumY = Math.max(maximumY, point.y);
  }

  return {
    x: minimumX,
    y: minimumY,
    width: maximumX - minimumX,
    height: maximumY - minimumY,
  };
}

function revealRectangle(rectangle) {
  if (!state.frameCanvas || rectangle.width <= 0 || rectangle.height <= 0) {
    return;
  }

  context.drawImage(
    state.frameCanvas,
    rectangle.x,
    rectangle.y,
    rectangle.width,
    rectangle.height,
    rectangle.x,
    rectangle.y,
    rectangle.width,
    rectangle.height,
  );
}

function beginImagePath(points, closePath) {
  context.beginPath();
  context.moveTo(points[0].x, points[0].y);
  for (let index = 1; index < points.length; index += 1) {
    context.lineTo(points[index].x, points[index].y);
  }
  if (closePath) {
    context.closePath();
  }
}

function revealFreeform(points) {
  if (!state.frameCanvas || points.length < 2) {
    return;
  }

  context.save();
  context.setTransform(1, 0, 0, 1, 0, 0);
  beginImagePath(points, true);
  // Even-odd, like the backend's mask: a loop drawn twice shows exactly what will be saved.
  context.clip('evenodd');
  context.drawImage(state.frameCanvas, 0, 0);
  context.restore();
}

function drawSelectionBorder(rectangle) {
  const cssRect = imageRectToCss(rectangle);

  context.save();
  setCssDrawingSpace();
  context.lineJoin = 'miter';
  context.strokeStyle = state.accent;
  context.lineWidth = 2;
  context.strokeRect(cssRect.x, cssRect.y, cssRect.width, cssRect.height);
  context.strokeStyle = '#ffffff';
  context.lineWidth = 1;
  context.strokeRect(
    cssRect.x - 1.5,
    cssRect.y - 1.5,
    cssRect.width + 3,
    cssRect.height + 3,
  );
  context.restore();
}

function beginCssFreeformPath(points) {
  const first = imagePointToCss(points[0]);
  context.beginPath();
  context.moveTo(first.x, first.y);
  for (let index = 1; index < points.length; index += 1) {
    const point = imagePointToCss(points[index]);
    context.lineTo(point.x, point.y);
  }
}

function drawFreeformOutline(points) {
  if (points.length < 2) {
    return;
  }

  context.save();
  setCssDrawingSpace();
  context.lineCap = 'round';
  context.lineJoin = 'round';

  beginCssFreeformPath(points);
  context.strokeStyle = 'rgba(255, 255, 255, 0.62)';
  context.lineWidth = 4;
  context.stroke();

  beginCssFreeformPath(points);
  context.strokeStyle = state.accent;
  context.lineWidth = 2;
  context.stroke();

  const first = imagePointToCss(points[0]);
  const last = imagePointToCss(points[points.length - 1]);
  context.beginPath();
  context.moveTo(last.x, last.y);
  context.lineTo(first.x, first.y);
  context.setLineDash([5, 4]);
  context.strokeStyle = state.accent;
  context.lineWidth = 1;
  context.stroke();
  context.restore();
}

function drawWindowBorder(windowInfo) {
  const cssRect = imageRectToCss(windowInfo);
  context.save();
  setCssDrawingSpace();
  context.strokeStyle = state.accent;
  context.lineWidth = 2;
  context.strokeRect(cssRect.x, cssRect.y, cssRect.width, cssRect.height);
  context.restore();
}

function badgeTextWidth(text, maximumWidth = Infinity) {
  context.save();
  setCssDrawingSpace();
  context.font = '12px "Segoe UI Variable Text", "Segoe UI", sans-serif';
  const width = Math.min(maximumWidth, Math.ceil(context.measureText(text).width) + 16);
  context.restore();
  return width;
}

function layoutSelectionBadge(rectangle, text) {
  const canvasWidth = state.metrics.rect.width;
  const canvasHeight = state.metrics.rect.height;
  const cssRect = imageRectToCss(rectangle);
  const width = badgeTextWidth(text);
  const height = 24;

  let x = cssRect.x + cssRect.width - width;
  let y = cssRect.y + cssRect.height + BADGE_GAP;
  x = clamp(x, 4, Math.max(4, canvasWidth - width - 4));

  if (y + height > canvasHeight - 4) {
    y = cssRect.y - height - BADGE_GAP;
  }
  if (y < 4) {
    y = clamp(
      cssRect.y + BADGE_GAP,
      4,
      Math.max(4, canvasHeight - height - 4),
    );
  }

  return { x, y, width, height, text };
}

function layoutTitleBadge(rectangle, text) {
  const canvasWidth = state.metrics.rect.width;
  const canvasHeight = state.metrics.rect.height;
  const cssRect = imageRectToCss(rectangle);
  const width = badgeTextWidth(text, Math.min(320, canvasWidth - 8));
  const height = 24;
  const x = clamp(cssRect.x, 4, Math.max(4, canvasWidth - width - 4));
  let y = cssRect.y - height - BADGE_GAP;

  if (y < 4) {
    y = cssRect.y + BADGE_GAP;
  }
  y = clamp(y, 4, Math.max(4, canvasHeight - height - 4));

  return { x, y, width, height, text };
}

function fitText(text, maximumWidth) {
  if (context.measureText(text).width <= maximumWidth) {
    return text;
  }

  const ellipsis = '…';
  let end = text.length;
  while (
    end > 0
    && context.measureText(text.slice(0, end) + ellipsis).width > maximumWidth
  ) {
    end -= 1;
  }
  return text.slice(0, end) + ellipsis;
}

function drawBadge(badge) {
  context.save();
  setCssDrawingSpace();
  beginRoundedRect(badge.x, badge.y, badge.width, badge.height, 4);
  context.fillStyle = 'rgba(32, 32, 32, 0.9)';
  context.fill();
  context.strokeStyle = 'rgba(255, 255, 255, 0.1)';
  context.lineWidth = 1;
  context.stroke();

  context.beginPath();
  context.rect(badge.x + 7, badge.y, badge.width - 14, badge.height);
  context.clip();
  context.fillStyle = '#ffffff';
  context.font = '12px "Segoe UI Variable Text", "Segoe UI", sans-serif';
  context.textAlign = 'center';
  context.textBaseline = 'middle';
  const fittedText = fitText(badge.text, badge.width - 16);
  context.fillText(fittedText, badge.x + badge.width / 2, badge.y + badge.height / 2);
  context.restore();
}

function drawCrosshair() {
  if (!state.pointerCss || isDragging()) {
    return;
  }

  context.save();
  setCssDrawingSpace();
  context.beginPath();
  context.moveTo(0, state.pointerCss.y + 0.5);
  context.lineTo(state.metrics.rect.width, state.pointerCss.y + 0.5);
  context.moveTo(state.pointerCss.x + 0.5, 0);
  context.lineTo(state.pointerCss.x + 0.5, state.metrics.rect.height);
  context.strokeStyle = 'rgba(255, 255, 255, 0.35)';
  context.lineWidth = 1;
  context.stroke();
  context.restore();
}

function rectanglesOverlap(first, second) {
  return first.x < second.x + second.width
    && first.x + first.width > second.x
    && first.y < second.y + second.height
    && first.y + first.height > second.y;
}

function loupePosition(badge) {
  const pointer = state.pointerCss;
  const width = state.metrics.rect.width;
  const height = state.metrics.rect.height;
  const rawCandidates = [
    { x: pointer.x + LOUPE_OFFSET, y: pointer.y + LOUPE_OFFSET },
    { x: pointer.x - LOUPE_OFFSET - LOUPE_SIZE, y: pointer.y + LOUPE_OFFSET },
    { x: pointer.x + LOUPE_OFFSET, y: pointer.y - LOUPE_OFFSET - LOUPE_HEIGHT },
    { x: pointer.x - LOUPE_OFFSET - LOUPE_SIZE, y: pointer.y - LOUPE_OFFSET - LOUPE_HEIGHT },
  ];
  const maximumX = Math.max(0, width - LOUPE_SIZE);
  const maximumY = Math.max(0, height - LOUPE_HEIGHT);
  const candidates = rawCandidates.map((candidate) => ({
    x: clamp(candidate.x, 0, maximumX),
    y: clamp(candidate.y, 0, maximumY),
    width: LOUPE_SIZE,
    height: LOUPE_HEIGHT,
  })).concat([
    { x: 0, y: 0, width: LOUPE_SIZE, height: LOUPE_HEIGHT },
    { x: maximumX, y: 0, width: LOUPE_SIZE, height: LOUPE_HEIGHT },
    { x: 0, y: maximumY, width: LOUPE_SIZE, height: LOUPE_HEIGHT },
    { x: maximumX, y: maximumY, width: LOUPE_SIZE, height: LOUPE_HEIGHT },
  ]);

  if (badge) {
    const separated = candidates.find((candidate) => !rectanglesOverlap(candidate, badge));
    if (separated) {
      return separated;
    }
  }

  return candidates[0];
}

function pixelHex(point) {
  if (!state.framePixels || !state.meta) {
    return '#000000';
  }

  const offset = (point.y * state.meta.width + point.x) * 4;
  return '#' + [0, 1, 2]
    .map((channel) => state.framePixels[offset + channel].toString(16).padStart(2, '0'))
    .join('')
    .toUpperCase();
}

function drawMagnifier(selectionBadge) {
  if (
    !state.showMagnifier
    || !state.frameCanvas
    || !state.pointerImage
    || !state.pointerCss
    || isDragging()
    || (state.mode !== 'rectangle' && state.mode !== 'freeform')
  ) {
    return;
  }

  const position = loupePosition(selectionBadge);
  const centerX = position.x + LOUPE_SIZE / 2;
  const centerY = position.y + LOUPE_SIZE / 2;
  const sourceLeft = Math.max(0, state.pointerImage.x - 8);
  const sourceTop = Math.max(0, state.pointerImage.y - 8);
  const sourceRight = Math.min(state.meta.width, state.pointerImage.x + 9);
  const sourceBottom = Math.min(state.meta.height, state.pointerImage.y + 9);
  const destinationX = centerX + (sourceLeft - state.pointerImage.x - 0.5) * LOUPE_ZOOM;
  const destinationY = centerY + (sourceTop - state.pointerImage.y - 0.5) * LOUPE_ZOOM;

  context.save();
  setCssDrawingSpace();
  context.beginPath();
  context.arc(centerX, centerY, LOUPE_SIZE / 2 - 1, 0, Math.PI * 2);
  context.clip();
  context.fillStyle = 'rgba(32, 32, 32, 0.96)';
  context.fillRect(position.x, position.y, LOUPE_SIZE, LOUPE_SIZE);
  context.imageSmoothingEnabled = false;
  context.drawImage(
    state.frameCanvas,
    sourceLeft,
    sourceTop,
    sourceRight - sourceLeft,
    sourceBottom - sourceTop,
    destinationX,
    destinationY,
    (sourceRight - sourceLeft) * LOUPE_ZOOM,
    (sourceBottom - sourceTop) * LOUPE_ZOOM,
  );

  context.beginPath();
  for (let x = centerX - 4; x >= position.x; x -= LOUPE_ZOOM) {
    context.moveTo(x + 0.5, position.y);
    context.lineTo(x + 0.5, position.y + LOUPE_SIZE);
  }
  for (let x = centerX + 4; x <= position.x + LOUPE_SIZE; x += LOUPE_ZOOM) {
    context.moveTo(x + 0.5, position.y);
    context.lineTo(x + 0.5, position.y + LOUPE_SIZE);
  }
  for (let y = centerY - 4; y >= position.y; y -= LOUPE_ZOOM) {
    context.moveTo(position.x, y + 0.5);
    context.lineTo(position.x + LOUPE_SIZE, y + 0.5);
  }
  for (let y = centerY + 4; y <= position.y + LOUPE_SIZE; y += LOUPE_ZOOM) {
    context.moveTo(position.x, y + 0.5);
    context.lineTo(position.x + LOUPE_SIZE, y + 0.5);
  }
  context.strokeStyle = 'rgba(255, 255, 255, 0.1)';
  context.lineWidth = 1;
  context.stroke();

  context.beginPath();
  context.moveTo(centerX - 12, centerY);
  context.lineTo(centerX + 12, centerY);
  context.moveTo(centerX, centerY - 12);
  context.lineTo(centerX, centerY + 12);
  context.strokeStyle = state.accent;
  context.lineWidth = 1.5;
  context.stroke();
  context.strokeRect(centerX - 4, centerY - 4, 8, 8);

  context.restore();

  context.save();
  setCssDrawingSpace();
  context.beginPath();
  context.arc(centerX, centerY, LOUPE_SIZE / 2 - 1, 0, Math.PI * 2);
  context.strokeStyle = state.accent;
  context.lineWidth = 2;
  context.stroke();
  context.beginPath();
  context.arc(centerX, centerY, LOUPE_SIZE / 2, 0, Math.PI * 2);
  context.strokeStyle = '#ffffff';
  context.lineWidth = 1;
  context.stroke();
  context.restore();

  drawLoupeLabel(centerX, position.y + LOUPE_SIZE + LOUPE_LABEL_GAP);
}

/** "x, y  #RRGGBB" in a pill centred under the magnifier, with a swatch of the pixel's colour. */
function drawLoupeLabel(centerX, top) {
  const colour = pixelHex(state.pointerImage);
  const text = `${state.pointerImage.x}, ${state.pointerImage.y}  ${colour}`;
  const swatch = 10;
  context.save();
  setCssDrawingSpace();
  context.font = '11px "Segoe UI Variable Text", "Segoe UI", sans-serif';
  const width = Math.ceil(context.measureText(text).width) + swatch + 24;
  const screenWidth = state.metrics.rect.width;
  const left = clamp(Math.round(centerX - width / 2), 2, Math.max(2, screenWidth - width - 2));
  context.beginPath();
  context.roundRect(left + 0.5, top + 0.5, width - 1, LOUPE_LABEL_HEIGHT - 1, LOUPE_LABEL_HEIGHT / 2);
  context.fillStyle = 'rgba(32, 32, 32, 0.94)';
  context.fill();
  context.strokeStyle = 'rgba(255, 255, 255, 0.16)';
  context.lineWidth = 1;
  context.stroke();
  const middle = top + LOUPE_LABEL_HEIGHT / 2;
  context.fillStyle = colour;
  context.fillRect(left + 10, middle - swatch / 2, swatch, swatch);
  context.strokeStyle = 'rgba(255, 255, 255, 0.5)';
  context.strokeRect(left + 10.5, middle - swatch / 2 + 0.5, swatch - 1, swatch - 1);
  context.fillStyle = '#ffffff';
  context.textAlign = 'left';
  context.textBaseline = 'middle';
  context.fillText(text, left + 10 + swatch + 6, middle + 0.5);
  context.restore();
}

function redraw() {
  if (!state.meta || !state.frameCanvas) {
    return;
  }

  context.save();
  context.setTransform(1, 0, 0, 1, 0, 0);
  context.imageSmoothingEnabled = false;
  context.drawImage(state.frameCanvas, 0, 0);
  context.fillStyle = 'rgba(0, 0, 0, 0.42)';
  context.fillRect(0, 0, canvas.width, canvas.height);
  context.restore();

  let selectionBadge = null;

  if (state.mode === 'rectangle' && state.rectangle) {
    revealRectangle(state.rectangle);
    drawSelectionBorder(state.rectangle);
    selectionBadge = layoutSelectionBadge(
      state.rectangle,
      String(state.rectangle.width) + ' × ' + String(state.rectangle.height)
        + (state.snapping ? '  ·  snapped' : ''),
    );
    drawBadge(selectionBadge);
  } else if (state.mode === 'freeform' && state.freeformPoints.length > 1) {
    revealFreeform(state.freeformPoints);
    drawFreeformOutline(state.freeformPoints);
    const bounds = freeformBounds(state.freeformPoints);
    selectionBadge = layoutSelectionBadge(
      bounds,
      String(bounds.width) + ' × ' + String(bounds.height),
    );
    drawBadge(selectionBadge);
  } else if (state.mode === 'window' && state.hoveredWindow) {
    revealRectangle(state.hoveredWindow);
    drawWindowBorder(state.hoveredWindow);
    drawBadge(layoutTitleBadge(state.hoveredWindow, state.hoveredWindow.title || 'Window'));
  } else if (state.mode === 'fullscreen' && state.hoveredMonitor) {
    revealRectangle(state.hoveredMonitor);
    drawWindowBorder(state.hoveredMonitor);
    drawBadge(layoutSelectionBadge(
      state.hoveredMonitor,
      'Full screen — ' + String(state.hoveredMonitor.width)
        + ' × ' + String(state.hoveredMonitor.height),
    ));
  }

  drawCrosshair();
  drawMagnifier(selectionBadge);
}

function releasePointer(pointerId) {
  if (pointerId !== null && canvas.hasPointerCapture(pointerId)) {
    canvas.releasePointerCapture(pointerId);
  }
}

function clearSelection() {
  releasePointer(state.dragPointerId);
  releasePointer(state.clickPointerId);
  state.dragPointerId = null;
  state.clickPointerId = null;
  state.dragStart = null;
  state.rectangle = null;
  state.freeformPoints = [];
  state.snapping = false;
}

function updateModeControls() {
  document.body.dataset.mode = state.mode;
  for (const button of modeButtons) {
    button.setAttribute('aria-pressed', String(button.dataset.mode === state.mode));
  }
}

function setMode(nextMode) {
  if (state.inFlight || !MODES.has(nextMode)) {
    return;
  }

  clearSelection();
  state.mode = nextMode;
  state.hoveredWindow = state.pointerImage && nextMode === 'window'
    ? firstWindowAt(state.pointerImage)
    : null;
  state.hoveredMonitor = state.pointerImage
    ? monitorAt(state.pointerImage)
    : preferredMonitor();
  updateModeControls();
  positionToolbar();
  requestRedraw();
}

function setInFlight(value) {
  state.inFlight = value;
  toolbar.setAttribute('aria-disabled', String(value));
}

async function finishCapture(command, argumentsObject = {}) {
  if (state.inFlight) {
    return;
  }

  setInFlight(true);
  try {
    // Every finish names the snip it belongs to; a stale overlay request is refused.
    const args = command === 'cancel_snip'
      ? argumentsObject
      : { ...argumentsObject, session: state.meta?.session };
    await invoke(command, args);
  } catch (error) {
    console.error(command + ' failed:', error);
    setInFlight(false);
    requestRedraw();
  }
}

function cancelSnip() {
  if (state.inFlight) {
    return;
  }
  if (typeof invoke !== 'function') {
    console.error('Cannot cancel snip: Tauri API unavailable.');
    return;
  }
  void finishCapture('cancel_snip');
}

function sanitizeRectangle(rectangle) {
  if (!state.meta) {
    return null;
  }

  const x = clamp(Math.round(rectangle.x), 0, Math.max(0, state.meta.width - 1));
  const y = clamp(Math.round(rectangle.y), 0, Math.max(0, state.meta.height - 1));
  const width = clamp(Math.round(rectangle.width), 0, state.meta.width - x);
  const height = clamp(Math.round(rectangle.height), 0, state.meta.height - y);
  return { x, y, width, height };
}

function finishRectangle(rectangle) {
  const safeRectangle = sanitizeRectangle(rectangle);
  if (!safeRectangle || safeRectangle.width < 3 || safeRectangle.height < 3) {
    cancelSnip();
    return;
  }

  void finishCapture('finish_rect', safeRectangle);
}

function simplifyFreeform(points) {
  if (points.length < 2) {
    return points.map(clampImagePoint);
  }

  const safePoints = points.map(clampImagePoint);
  const simplified = [safePoints[0]];
  for (let index = 1; index < safePoints.length - 1; index += 1) {
    const previous = simplified[simplified.length - 1];
    const point = safePoints[index];
    const deltaX = point.x - previous.x;
    const deltaY = point.y - previous.y;
    if (deltaX * deltaX + deltaY * deltaY >= 4) {
      simplified.push(point);
    }
  }
  simplified.push(safePoints[safePoints.length - 1]);
  return simplified;
}

function finishFreeform(points) {
  const simplified = simplifyFreeform(points);
  const bounds = simplified.length > 0 ? freeformBounds(simplified) : null;
  if (
    simplified.length < 3
    || !bounds
    || bounds.width < 3
    || bounds.height < 3
  ) {
    cancelSnip();
    return;
  }

  void finishCapture('finish_freeform', {
    points: simplified.map((point) => [point.x, point.y]),
  });
}

function finishHoveredTarget() {
  if (state.mode === 'window' && state.hoveredWindow) {
    void finishCapture('finish_window', { windowId: state.hoveredWindow.id });
  } else if (state.mode === 'fullscreen' && state.hoveredMonitor) {
    void finishCapture('finish_monitor', { monitorIndex: state.hoveredMonitor.index });
  }
}

function updatePointer(event) {
  if (!state.meta) {
    return;
  }
  state.pointerImage = toImage(event);
  state.pointerEdge = toEdge(event);
  state.pointerCss = toCanvasCss(event);
  updateHover(state.pointerImage);
}

canvas.addEventListener('pointerdown', (event) => {
  if (state.inFlight || !state.meta || event.button !== 0 || !event.isPrimary) {
    return;
  }

  event.preventDefault();
  updatePointer(event);

  if (state.mode === 'rectangle' || state.mode === 'freeform') {
    clearSelection();
    state.dragPointerId = event.pointerId;
    state.dragStart = state.pointerEdge;
    canvas.setPointerCapture(event.pointerId);

    if (state.mode === 'rectangle') {
      state.rectangle = normalizedRectangle(state.dragStart, state.dragStart);
    } else {
      state.freeformPoints = [state.dragStart];
    }
  } else {
    state.clickPointerId = event.pointerId;
    canvas.setPointerCapture(event.pointerId);
  }

  requestRedraw();
});

document.addEventListener('pointermove', (event) => {
  if (state.inFlight || !state.meta || !event.isPrimary) {
    return;
  }

  updatePointer(event);

  if (event.pointerId === state.dragPointerId && state.dragStart) {
    if (state.mode === 'rectangle') {
      state.snapping = event.altKey;
      state.rectangle = dragRectangle(state.pointerEdge);
    } else if (state.mode === 'freeform') {
      const previous = state.freeformPoints[state.freeformPoints.length - 1];
      if (!previous || previous.x !== state.pointerEdge.x || previous.y !== state.pointerEdge.y) {
        state.freeformPoints.push(state.pointerEdge);
      }
    }
  } else if (!isDragging()) {
    positionToolbar(state.hoveredMonitor ?? preferredMonitor());
  }

  requestRedraw();
});

canvas.addEventListener('pointerup', (event) => {
  if (state.inFlight || !state.meta || !event.isPrimary) {
    return;
  }

  updatePointer(event);

  if (event.pointerId === state.dragPointerId) {
    releasePointer(state.dragPointerId);
    state.dragPointerId = null;

    if (state.mode === 'rectangle' && state.dragStart) {
      state.snapping = event.altKey;
      state.rectangle = dragRectangle(state.pointerEdge);
      state.snapping = false;
      finishRectangle(state.rectangle);
    } else if (state.mode === 'freeform') {
      const previous = state.freeformPoints[state.freeformPoints.length - 1];
      if (!previous || previous.x !== state.pointerEdge.x || previous.y !== state.pointerEdge.y) {
        state.freeformPoints.push(state.pointerEdge);
      }
      finishFreeform(state.freeformPoints);
    }

    state.dragStart = null;
  } else if (event.pointerId === state.clickPointerId) {
    releasePointer(state.clickPointerId);
    state.clickPointerId = null;
    finishHoveredTarget();
  }

  requestRedraw();
});

canvas.addEventListener('pointercancel', (event) => {
  if (event.pointerId !== state.dragPointerId && event.pointerId !== state.clickPointerId) {
    return;
  }

  clearSelection();
  requestRedraw();
});

document.addEventListener('contextmenu', (event) => {
  event.preventDefault();
  cancelSnip();
});

/** Pressing or releasing Alt mid-drag re-snaps at once, without waiting for the pointer to move. */
function updateSnapping(event) {
  if (event.key !== 'Alt') {
    return;
  }
  // Alt alone would otherwise hand the keyboard to the (invisible) window menu.
  event.preventDefault();
  if (state.mode !== 'rectangle' || !isDragging() || !state.dragStart || !state.pointerEdge) {
    return;
  }
  state.snapping = event.type === 'keydown';
  state.rectangle = dragRectangle(state.pointerEdge);
  requestRedraw();
}

document.addEventListener('keyup', updateSnapping);

document.addEventListener('keydown', (event) => {
  updateSnapping(event);
  if (event.repeat || state.inFlight) {
    return;
  }

  if (event.key === 'Escape') {
    event.preventDefault();
    cancelSnip();
    return;
  }

  // Window mode: Tab / Shift+Tab steps through the windows stacked under the pointer.
  if (event.key === 'Tab' && state.mode === 'window' && state.pointerImage) {
    event.preventDefault();
    const stack = windowsAt(state.pointerImage);
    if (stack.length > 1) {
      const index = Math.max(0, stack.indexOf(state.hoveredWindow));
      const step = event.shiftKey ? -1 : 1;
      state.hoveredWindow = stack[(index + step + stack.length) % stack.length];
      requestRedraw();
    }
    return;
  }

  const keyboardModes = {
    1: 'rectangle',
    2: 'freeform',
    3: 'window',
    4: 'fullscreen',
  };
  if (keyboardModes[event.key]) {
    event.preventDefault();
    setMode(keyboardModes[event.key]);
    return;
  }

  if (event.ctrlKey && event.key.toLowerCase() === 'a' && state.mode === 'rectangle') {
    event.preventDefault();
    clearSelection();
    const monitor = preferredMonitor();
    if (monitor) {
      state.rectangle = sanitizeRectangle(monitor);
      requestRedraw();
    }
    return;
  }

  if (event.key === 'Enter') {
    // A focused toolbar button (mode, Close) handles Enter itself.
    if (event.target instanceof HTMLButtonElement) {
      return;
    }
    event.preventDefault();
    if (state.mode === 'rectangle' && state.rectangle) {
      releasePointer(state.dragPointerId);
      state.dragPointerId = null;
      state.dragStart = null;
      finishRectangle(state.rectangle);
    } else if (state.mode === 'freeform' && state.freeformPoints.length > 0) {
      releasePointer(state.dragPointerId);
      state.dragPointerId = null;
      state.dragStart = null;
      finishFreeform(state.freeformPoints);
    } else {
      finishHoveredTarget();
    }
  }
});

for (const button of modeButtons) {
  button.addEventListener('click', () => {
    setMode(button.dataset.mode);
  });
}

closeButton.addEventListener('click', cancelSnip);

window.addEventListener('resize', () => {
  updateCanvasMetrics();
  positionToolbar();
  requestRedraw();
});

function applyConfig(config) {
  state.showMagnifier = config?.appearance?.show_magnifier !== false;
  refreshAccent();
  requestRedraw();
}

async function loadConfig() {
  try {
    applyConfig(await invoke('get_config'));
  } catch {
    // The default preference remains active until a config-changed event arrives.
  }
}

/** Drops the frame copies while the overlay is hidden; a pending refresh becomes stale. */
function releaseFrame() {
  state.refreshSequence += 1;
  clearSelection();
  state.frameCanvas = null;
  state.framePixels = null;
  state.pointerImage = null;
  state.pointerEdge = null;
  canvas.width = 1;
  canvas.height = 1;
  setInFlight(false);
}

async function refreshOverlay(isInitialLoad = false) {
  const sequence = state.refreshSequence + 1;
  state.refreshSequence = sequence;

  try {
    // Metadata first: its session number makes sure the pixels belong to the same snip.
    const meta = await invoke('overlay_metadata');
    const pixelBuffer = await invoke('overlay_pixels', { session: meta.session });

    if (sequence !== state.refreshSequence) {
      return;
    }

    const width = Math.max(1, Math.trunc(meta.width));
    const height = Math.max(1, Math.trunc(meta.height));
    const pixels = new Uint8ClampedArray(pixelBuffer);
    if (pixels.byteLength !== width * height * 4) {
      throw new Error(
        'Unexpected overlay pixel buffer length: ' + String(pixels.byteLength),
      );
    }

    const nextFrame = document.createElement('canvas');
    nextFrame.width = width;
    nextFrame.height = height;
    const frameContext = nextFrame.getContext('2d', { alpha: false });
    const imageData = new ImageData(pixels, width, height);
    frameContext.putImageData(imageData, 0, 0);

    clearSelection();
    state.meta = {
      ...meta,
      width,
      height,
      monitors: meta.monitors ?? [],
      windows: meta.windows ?? [],
    };
    state.frameCanvas = nextFrame;
    state.framePixels = imageData.data;
    state.mode = MODES.has(meta.mode) ? meta.mode : 'rectangle';
    state.pointerImage = null;
    state.pointerCss = null;
    state.hoveredWindow = null;
    state.hoveredMonitor = state.meta.monitors.find((monitor) => monitor.primary)
      ?? state.meta.monitors[0]
      ?? null;
    setInFlight(false);

    canvas.width = width;
    canvas.height = height;
    updateCanvasMetrics();
    refreshAccent();
    updateModeControls();
    positionToolbar();
    // Paint synchronously (the window is still hidden, where animation frames may be paused),
    // then let the backend show the window so the previous capture never flashes.
    redraw();
    await invoke('overlay_ready');
  } catch (error) {
    if (!isInitialLoad) {
      console.error('Failed to refresh the selection overlay:', error);
      try {
        await invoke('cancel_snip');
      } catch (cancelError) {
        console.error('Failed to cancel the broken selection overlay:', cancelError);
      }
    }
  }
}

document.addEventListener('DOMContentLoaded', async () => {
  if (typeof invoke !== 'function' || typeof listen !== 'function') {
    console.error('Tauri API unavailable. Escape and right-click cannot dismiss the overlay.');
    return;
  }

  try {
    await Promise.all([
      listen('overlay-refresh', () => {
        void refreshOverlay();
      }),
      listen('overlay-reset', () => {
        releaseFrame();
      }),
      listen('config-changed', (event) => {
        applyConfig(event.payload);
      }),
    ]);
  } catch (error) {
    console.error('Failed to register overlay listeners:', error);
  }

  await Promise.allSettled([
    refreshOverlay(true),
    loadConfig(),
  ]);
});
