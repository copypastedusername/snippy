'use strict';

const INVOKE_FAILED = Symbol('invoke-failed');
/** Keys that may be bound without a modifier (nobody types text with them). */
const STANDALONE_KEYS = /^(F([1-9]|1\d|2[0-4])|PrintScreen|Pause|ScrollLock)$/;
const MODE_LABELS = {
  rectangle: 'Rectangle',
  freeform: 'Free-form',
  window: 'Window',
  fullscreen: 'Full screen',
};

let invokeCommand = null;
let listenEvent = null;
let config = null;
let autostartEnabled = false;
let currentMode = 'rectangle';
let configVersion = 0;
let pendingWrites = 0;
let saveQueue = Promise.resolve();
let statusTimer = null;
let statusKind = 'loading';
let windowsShortcutStatus = { win_shift_s_taken: false, print_screen_taken: false, print_screen_by_windows: false, windows_build: null, closed_handler: null, closed_handler_is_snippy: false };
let readyReported = false;
/** How long the first paint waits for the recent captures before showing the window anyway. */
const RECENTS_HEAD_START_MS = 400;

const delay = (ms) => new Promise((resolve) => { window.setTimeout(resolve, ms); });
// A hidden window may not run animation frames, so a timeout backs the frame up.
const nextFrame = () => Promise.race([new Promise((resolve) => requestAnimationFrame(resolve)), delay(50)]);

/** Tells the backend the panel has rendered, so it can show the (still hidden) window. */
function reportReady() {
  if (readyReported || !window.__TAURI__?.core?.invoke) return;
  readyReported = true;
  window.__TAURI__.core.invoke('main_ready').catch((error) => console.error('main_ready failed:', error));
}
const debounceTimers = new Map();
const recentCaptureItems = new WeakMap();
const elements = {
  captureView: document.getElementById('captureView'),
  settingsView: document.getElementById('settingsView'),
  statusLine: document.getElementById('statusLine'),
  modeTiles: [...document.querySelectorAll('.mode-tile')],
  captureDelay: document.getElementById('captureDelay'),
  selectedHotkey: document.getElementById('selectedHotkey'),
  recentCaptures: document.getElementById('recentCaptures'),
  recentEmpty: document.getElementById('recentEmpty'),
  autostartSwitch: document.getElementById('autostartSwitch'),
  adminNote: document.getElementById('adminNote'),
  elevatedNote: document.getElementById('elevatedNote'),
  adminSwitch: document.getElementById('adminSwitch'),
  closeTraySwitch: document.getElementById('closeTraySwitch'),
  startMinimizedSwitch: document.getElementById('startMinimizedSwitch'),
  previewSelect: document.getElementById('previewSelect'),
  updatesSwitch: document.getElementById('updatesSwitch'),
  versionLabel: document.getElementById('versionLabel'),
  updateNote: document.getElementById('updateNote'),
  checkUpdatesButton: document.getElementById('checkUpdatesButton'),
  installUpdateButton: document.getElementById('installUpdateButton'),
  windowsNotificationsSwitch: document.getElementById('windowsNotificationsSwitch'),
  screenclipSwitch: document.getElementById('screenclipSwitch'),
  screenclipNote: document.getElementById('screenclipNote'),
  printScreenSwitch: document.getElementById('printScreenSwitch'),
  printScreenNote: document.getElementById('printScreenNote'),
  defaultAppsNote: document.getElementById('defaultAppsNote'),
  keyboardSettingsButton: document.getElementById('keyboardSettingsButton'),
  defaultAppsSteps: document.getElementById('defaultAppsSteps'),
  windowsOnlySwitch: document.getElementById('windowsOnlySwitch'),
  defaultAppsRow: document.getElementById('defaultAppsRow'),
  hotkeysCard: document.getElementById('hotkeysCard'),
  restoreDefaultsButton: document.getElementById('restoreDefaultsButton'),
  defaultAppsButton: document.getElementById('defaultAppsButton'),
  defaultModeSelect: document.getElementById('defaultModeSelect'),
  settingsDelay: document.getElementById('settingsDelay'),
  cursorSwitch: document.getElementById('cursorSwitch'),
  soundSelect: document.getElementById('soundSelect'),
  soundPreviewButton: document.getElementById('soundPreviewButton'),
  magnifierSwitch: document.getElementById('magnifierSwitch'),
  outputFolder: document.getElementById('outputFolder'),
  formatSelect: document.getElementById('formatSelect'),
  qualityRow: document.getElementById('qualityRow'),
  qualityDivider: document.querySelector('.quality-divider'),
  qualityRange: document.getElementById('qualityRange'),
  qualityValue: document.getElementById('qualityValue'),
  filenamePattern: document.getElementById('filenamePattern'),
  filenamePreview: document.getElementById('filenamePreview'),
  autoSaveSwitch: document.getElementById('autoSaveSwitch'),
  clipboardSwitch: document.getElementById('clipboardSwitch'),
  hotkeyInputs: [...document.querySelectorAll('.hotkey-input')],
};

function errorMessage(error) {
  if (typeof error === 'string') return error;
  if (error && typeof error.message === 'string') return error.message;
  return String(error);
}

function setStatus(message, kind = 'info') {
  if (statusTimer !== null) {
    window.clearTimeout(statusTimer);
    statusTimer = null;
  }
  statusKind = kind;
  elements.statusLine.textContent = message;
  elements.statusLine.classList.toggle('error', kind === 'error');
}

function reportError(error) {
  setStatus(errorMessage(error), 'error');
}

/** The key that snips right now: the mode's own hotkey, or a Windows key in "only these keys". */
function activeHotkey() {
  const behavior = config?.behavior;
  if (behavior?.windows_shortcuts_only && (behavior.handle_screenclip || behavior.handle_print_screen)) {
    return behavior.handle_screenclip ? 'Win+Shift+S' : 'Print Screen';
  }
  return config?.hotkeys?.[currentMode] || '';
}

function idleMessage() {
  const hotkey = activeHotkey();
  return hotkey ? `Press ${hotkey} or click New snip` : 'Click New snip to capture';
}

function showIdleStatus() {
  setStatus(idleMessage(), 'idle');
}

function showSuccess(message) {
  setStatus(message, 'success');
  statusTimer = window.setTimeout(showIdleStatus, 6000);
}

async function call(cmd, args = undefined) {
  if (!invokeCommand) {
    setStatus('Tauri API unavailable. Open Snippy through the desktop application.', 'error');
    return INVOKE_FAILED;
  }

  try {
    return args === undefined ? await invokeCommand(cmd) : await invokeCommand(cmd, args);
  } catch (error) {
    console.error(`Command ${cmd} failed:`, error);
    setStatus(errorMessage(error), 'error');
    return INVOKE_FAILED;
  }
}

/** Default apps differs between Windows 10 and 11: give the steps for the one in use. */
function renderDefaultAppsSteps(build) {
  const windows10 = Number.isFinite(build) && build < 22000;
  const steps = windows10
    ? [
        'Click <b>Open Default apps</b>.',
        'Scroll down and click <b>Choose default apps by protocol</b>.',
        'Find <b>MS-SCREENCLIP</b>, click the app next to it and choose <b>Snippy</b>.',
      ]
    : [
        'Click <b>Open Default apps</b> — Windows opens Snippy\'s page.',
        'Find <b>MS-SCREENCLIP</b> in the list and click the app shown next to it.',
        'Choose <b>Snippy</b>.',
      ];
  elements.defaultAppsSteps.replaceChildren(...steps.map((html) => {
    const item = document.createElement('li');
    item.innerHTML = html;
    return item;
  }));
}

/** The preview select's value: "off" or the number of seconds. */
function previewChoice(behavior) {
  return behavior.show_flyout ? String(behavior.preview_seconds ?? 5) : 'off';
}

function describeUpdate(status) {
  elements.versionLabel.textContent = `Snippy ${status.current}`;
  elements.installUpdateButton.hidden = !status.available;
  let note = 'You have the latest version.';
  if (status.available) {
    note = status.rebuild
      ? `A fixed build of ${status.latest} is available.`
      : `Version ${status.latest} is available.`;
  }
  elements.updateNote.textContent = note;
}

async function checkForUpdates() {
  elements.checkUpdatesButton.disabled = true;
  elements.updateNote.textContent = 'Checking…';
  const status = await call('check_for_updates');
  elements.checkUpdatesButton.disabled = false;
  if (status === INVOKE_FAILED) {
    elements.updateNote.textContent = 'Could not reach the update server.';
    return;
  }
  describeUpdate(status);
}

async function installUpdate() {
  elements.installUpdateButton.disabled = true;
  elements.installUpdateButton.textContent = 'Updating…';
  elements.checkUpdatesButton.disabled = true;
  elements.updateNote.textContent = 'Downloading and checking the update…';
  // On success Snippy restarts on the new version, so only a failure returns here.
  const result = await call('install_update');
  elements.installUpdateButton.disabled = false;
  elements.installUpdateButton.textContent = 'Update now';
  elements.checkUpdatesButton.disabled = false;
  if (result === INVOKE_FAILED) {
    elements.updateNote.textContent = 'The update was not installed.';
  }
}

async function showVersion() {
  try {
    const version = await window.__TAURI__?.app?.getVersion?.();
    // Shown as "0.1" for 0.1.0, matching the release names.
    if (version) elements.versionLabel.textContent = `Snippy ${version.replace(/\.0$/, '')}`;
  } catch (error) {
    console.error('Could not read the app version:', error);
  }
}

function setControlValue(control, value, preserveActive) {
  if (!preserveActive || document.activeElement !== control) control.value = String(value ?? '');
}

function setControlChecked(control, value, preserveActive) {
  if (!preserveActive || document.activeElement !== control) control.checked = Boolean(value);
}

function updateModeUi() {
  for (const tile of elements.modeTiles) {
    const selected = tile.dataset.mode === currentMode;
    tile.setAttribute('aria-checked', String(selected));
    tile.tabIndex = selected ? 0 : -1;
  }
  const binding = activeHotkey();
  elements.selectedHotkey.textContent = binding || 'No hotkey';
  elements.selectedHotkey.classList.toggle('is-empty', !binding);
  elements.selectedHotkey.title = `${MODE_LABELS[currentMode]} hotkey: ${binding || 'not bound'}`;
  if (statusKind === 'idle') showIdleStatus();
}

function filenameExample(pattern, format) {
  const now = new Date();
  const pad = (value) => String(value).padStart(2, '0');
  const tokens = {
    '%Y': String(now.getFullYear()),
    '%m': pad(now.getMonth() + 1),
    '%d': pad(now.getDate()),
    '%H': pad(now.getHours()),
    '%M': pad(now.getMinutes()),
    '%S': pad(now.getSeconds()),
  };
  const stem = pattern.replace(/%[YmdHMS]/g, (token) => tokens[token]);
  return `${stem}.${format === 'jpeg' ? 'jpg' : 'png'}`;
}

function updateOutputDetails() {
  const isJpeg = config.output.format === 'jpeg';
  elements.qualityRow.hidden = !isJpeg;
  elements.qualityDivider.hidden = !isJpeg;
  elements.qualityValue.value = String(config.output.jpeg_quality);
  elements.qualityValue.textContent = String(config.output.jpeg_quality);
  elements.filenamePreview.textContent = `Preview: ${filenameExample(config.output.filename_pattern, config.output.format)}`;
}

// Each Windows key reaches Snippy one of two ways: Snippy holds it directly (Windows left it free),
// or Windows opens an ms-screenclip: link and the user picks Snippy as its handler once.
function updateWindowsShortcuts() {
  const behavior = config?.behavior;
  if (!behavior) return;
  const status = windowsShortcutStatus;
  const winShiftS = Boolean(behavior.handle_screenclip);
  const printScreen = Boolean(behavior.handle_print_screen);
  const handler = status.closed_handler || 'the Snipping Tool';

  // Win+Shift+S: held by Windows, which opens the ms-screenclip: link with the default app.
  const winShiftSReady = status.win_shift_s_taken || status.closed_handler_is_snippy;
  elements.screenclipNote.textContent = !winShiftS
    ? 'Snip with the Windows screenshot shortcut.'
    : status.win_shift_s_taken
      ? 'Windows left this key free, so Snippy handles it directly.'
      : status.closed_handler_is_snippy
        ? 'Windows passes this key to Snippy.'
        : `Windows still opens ${handler}. Make Snippy the default app below.`;

  // Print Screen: an ordinary hotkey, unless Windows' own screen capture takes the key.
  // `print_screen_by_windows` is true, false, or null when Windows does not say.
  const windowsTakesPrintScreen = status.print_screen_by_windows;
  elements.printScreenNote.textContent = !printScreen
    ? 'Snip with the Print Screen key.'
    : windowsTakesPrintScreen === true
      ? 'Windows takes this key for its screen capture. In Keyboard settings, turn off "Use the Print Screen key to open screen capture".'
      : !status.print_screen_taken
        ? 'Another app holds Print Screen.'
        : windowsTakesPrintScreen === null
          ? 'Snippy holds Print Screen. If the key still opens the Snipping Tool, turn off "Use the Print Screen key to open screen capture" in Keyboard settings.'
          : 'Snippy handles Print Screen.';
  elements.keyboardSettingsButton.hidden = !(printScreen && windowsTakesPrintScreen !== false);

  elements.defaultAppsRow.hidden = !(winShiftS && !winShiftSReady);
  elements.defaultAppsNote.textContent = `Windows opens ${handler} for Win+Shift+S. Make Snippy the default app for it:`;
  renderDefaultAppsSteps(status.windows_build);

  const anyWindowsKey = winShiftS || printScreen;
  elements.windowsOnlySwitch.disabled = !anyWindowsKey;
  const ownHotkeysOff = anyWindowsKey && Boolean(behavior.windows_shortcuts_only);
  elements.hotkeysCard.classList.toggle('is-disabled', ownHotkeysOff);
  for (const input of elements.hotkeyInputs) input.disabled = ownHotkeysOff;
  elements.restoreDefaultsButton.disabled = ownHotkeysOff;
  updateModeUi();
}

async function refreshWindowsShortcutStatus() {
  const status = await call('windows_shortcut_status');
  if (status === INVOKE_FAILED || !status) return;
  windowsShortcutStatus = status;
  updateWindowsShortcuts();
}

// Elevation is silent when the logon task exists, because Task Scheduler already registers it with
// the highest available privileges; the UAC prompt is only the fallback without autostart.
function updateElevationNote() {
  if (!config?.behavior.run_as_admin) {
    elements.adminNote.textContent = 'Needed for hotkeys inside full-screen games.';
  } else if (autostartEnabled) {
    elements.adminNote.textContent =
      'Started elevated by the Windows logon task, without a UAC prompt.';
  } else {
    elements.adminNote.textContent =
      'Asks for UAC on every start. Enable "Start with Windows" to elevate silently instead.';
  }
}

function renderConfig(preserveActive = true) {
  if (!config) return;

  currentMode = config.capture.default_mode;
  updateModeUi();

  setControlValue(elements.captureDelay, config.capture.delay_seconds, preserveActive);
  setControlChecked(elements.autostartSwitch, autostartEnabled, preserveActive);
  setControlChecked(elements.adminSwitch, config.behavior.run_as_admin, preserveActive);
  updateElevationNote();
  setControlChecked(elements.closeTraySwitch, config.behavior.close_to_tray, preserveActive);
  setControlChecked(elements.startMinimizedSwitch, config.behavior.start_minimized, preserveActive);
  setControlValue(elements.previewSelect, previewChoice(config.behavior), preserveActive);
  setControlChecked(elements.updatesSwitch, config.behavior.check_updates, preserveActive);
  setControlChecked(elements.windowsNotificationsSwitch, config.behavior.windows_notifications, preserveActive);
  elements.windowsNotificationsSwitch.disabled = !config.behavior.show_flyout;
  setControlChecked(elements.screenclipSwitch, config.behavior.handle_screenclip, preserveActive);
  setControlChecked(elements.printScreenSwitch, config.behavior.handle_print_screen, preserveActive);
  setControlChecked(elements.windowsOnlySwitch, config.behavior.windows_shortcuts_only, preserveActive);
  updateWindowsShortcuts();
  setControlValue(elements.defaultModeSelect, config.capture.default_mode, preserveActive);
  setControlValue(elements.settingsDelay, config.capture.delay_seconds, preserveActive);
  setControlChecked(elements.cursorSwitch, config.capture.include_cursor, preserveActive);
  setControlValue(elements.soundSelect, config.capture.sound ?? 'off', preserveActive);
  elements.soundPreviewButton.disabled = (config.capture.sound ?? 'off') === 'off';
  setControlChecked(elements.magnifierSwitch, config.appearance.show_magnifier, preserveActive);
  setControlValue(elements.outputFolder, config.output.directory, preserveActive);
  elements.outputFolder.title = config.output.directory;
  setControlValue(elements.formatSelect, config.output.format, preserveActive);
  setControlValue(elements.qualityRange, config.output.jpeg_quality, preserveActive);
  setControlValue(elements.filenamePattern, config.output.filename_pattern, preserveActive);
  setControlChecked(elements.autoSaveSwitch, config.output.auto_save, preserveActive);
  setControlChecked(elements.clipboardSwitch, config.output.copy_to_clipboard, preserveActive);

  for (const input of elements.hotkeyInputs) {
    setControlValue(input, config.hotkeys[input.dataset.hotkey] || '', preserveActive);
  }
  updateOutputDetails();
}

function preserveActiveEdit(incoming) {
  const active = document.activeElement;
  if (!active || !config) return incoming;

  const next = structuredClone(incoming);
  switch (active.id) {
    case 'filenamePattern': next.output.filename_pattern = elements.filenamePattern.value; break;
    case 'qualityRange': next.output.jpeg_quality = Number(elements.qualityRange.value); break;
    case 'defaultModeSelect': next.capture.default_mode = config.capture.default_mode; break;
    case 'captureDelay':
    case 'settingsDelay': next.capture.delay_seconds = config.capture.delay_seconds; break;
    case 'formatSelect': next.output.format = config.output.format; break;
    case 'soundSelect': next.capture.sound = config.capture.sound; break;
    case 'previewSelect':
      next.behavior.show_flyout = config.behavior.show_flyout;
      next.behavior.preview_seconds = config.behavior.preview_seconds;
      break;
    default:
      if (active.classList.contains('hotkey-input')) {
        next.hotkeys[active.dataset.hotkey] = config.hotkeys[active.dataset.hotkey];
      }
  }
  return next;
}

function saveConfig() {
  configVersion += 1;
  pendingWrites += 1;
  saveQueue = saveQueue.then(async () => {
    const sentVersion = configVersion;
    const snapshot = structuredClone(config);
    const stored = await call('set_config', { config: snapshot });
    pendingWrites -= 1;
    if (stored !== INVOKE_FAILED && sentVersion === configVersion) {
      config = stored;
      renderConfig(true);
    }
  });
  return saveQueue;
}

function debounceSave(key) {
  // The local config already holds an edit that is not saved yet: a save still in flight must not
  // overwrite it with the older stored copy when it returns.
  configVersion += 1;
  const existing = debounceTimers.get(key);
  if (existing !== undefined) window.clearTimeout(existing);
  debounceTimers.set(key, window.setTimeout(() => {
    debounceTimers.delete(key);
    void saveConfig();
  }, 400));
}

function selectMode(mode, focus = false) {
  if (!config || !Object.hasOwn(MODE_LABELS, mode)) return;
  currentMode = mode;
  config.capture.default_mode = mode;
  updateModeUi();
  setControlValue(elements.defaultModeSelect, mode, true);
  if (focus) document.querySelector(`.mode-tile[data-mode="${mode}"]`)?.focus();
  void saveConfig();
}

function showCaptureView() {
  elements.settingsView.hidden = true;
  elements.captureView.hidden = false;
  document.getElementById('settingsButton').focus();
}

function showSettingsView() {
  elements.captureView.hidden = true;
  elements.settingsView.hidden = false;
  document.getElementById('backButton').focus();
}

async function runRecentCommand(command, item, successMessage, refresh = false) {
  const result = await call(command, { path: item.path });
  if (result === INVOKE_FAILED) return;
  if (refresh && !(await refreshRecents())) return;
  showSuccess(successMessage);
}

async function copyRecentPath(path) {
  try {
    if (!navigator.clipboard?.writeText) throw new Error('Clipboard API unavailable.');
    await navigator.clipboard.writeText(path);
    showSuccess('Copied to clipboard');
  } catch (error) {
    console.error('Could not copy capture path:', error);
    setStatus(errorMessage(error), 'error');
  }
}

function recentContextMenuItems(item) {
  const name = item.name || baseName(item.path);
  return [
    { label: 'Edit', icon: 'edit', run: () => runRecentCommand('open_editor', item, `Editing ${name}`) },
    { label: 'Open', icon: 'open', run: () => runRecentCommand('open_path', item, `Opened ${name}`) },
    { label: 'Copy image', icon: 'image', run: () => runRecentCommand('copy_capture', item, 'Copied to clipboard') },
    { label: 'Copy path', icon: 'link', run: () => copyRecentPath(item.path) },
    { label: 'Show in folder', icon: 'folder', run: () => runRecentCommand('reveal_path', item, `Showing ${name} in folder`) },
    { label: 'Delete', icon: 'delete', danger: true, run: () => runRecentCommand('delete_capture', item, `Moved ${name} to the Recycle Bin`, true) },
  ];
}

function handleContextMenu(event) {
  event.preventDefault();
  const anchor = event.target instanceof Element ? event.target.closest('.recent-button') : null;
  const item = anchor ? recentCaptureItems.get(anchor) : null;
  if (item) openContextMenu(event, recentContextMenuItems(item), reportError);
}

function handleContextMenuKey(event) {
  if (event.key !== 'ContextMenu' && !(event.key === 'F10' && event.shiftKey)) return;
  const item = recentCaptureItems.get(event.currentTarget);
  if (!item) return;
  event.preventDefault();
  openContextMenu(event, recentContextMenuItems(item), reportError);
}

function handleRecentKey(event) {
  if (event.key === 'ContextMenu' || (event.key === 'F10' && event.shiftKey)) {
    handleContextMenuKey(event);
    return;
  }

  const item = recentCaptureItems.get(event.currentTarget);
  if (!item || event.repeat) return;
  const name = item.name || baseName(item.path);
  if (event.key === 'Enter') {
    event.preventDefault();
    void runRecentCommand('open_path', item, `Opened ${name}`);
  } else if (event.key === 'Delete') {
    event.preventDefault();
    void runRecentCommand('delete_capture', item, `Moved ${name} to the Recycle Bin`, true);
  }
}

function renderRecents(items) {
  closeContextMenu(false);
  elements.recentCaptures.replaceChildren();
  const captures = Array.isArray(items) ? items.slice(0, 8) : [];
  elements.recentEmpty.hidden = captures.length > 0;
  elements.recentCaptures.hidden = captures.length === 0;

  for (const item of captures) {
    const wrapper = document.createElement('div');
    wrapper.className = 'recent-item';

    const openButton = document.createElement('button');
    openButton.className = 'recent-button';
    openButton.type = 'button';
    openButton.title = `${item.name} — ${item.width} × ${item.height}`;
    openButton.setAttribute('aria-label', `Open ${item.name}`);
    openButton.setAttribute('aria-haspopup', 'menu');
    openButton.setAttribute('aria-expanded', 'false');
    openButton.setAttribute('aria-controls', 'contextMenu');
    openButton.addEventListener('click', () => { void call('open_path', { path: item.path }); });
    openButton.addEventListener('keydown', handleRecentKey);
    recentCaptureItems.set(openButton, item);

    const image = document.createElement('img');
    image.src = item.thumbnail;
    image.alt = '';
    openButton.append(image);

    const revealButton = document.createElement('button');
    revealButton.className = 'reveal-button';
    revealButton.type = 'button';
    revealButton.title = `Show ${item.name} in folder`;
    revealButton.setAttribute('aria-label', `Show ${item.name} in folder`);
    revealButton.append(spriteIcon('folder', 14));
    revealButton.addEventListener('click', () => { void call('reveal_path', { path: item.path }); });

    wrapper.append(openButton, revealButton);
    elements.recentCaptures.append(wrapper);
  }
}

let recentsRequest = 0;

async function refreshRecents() {
  // Several refreshes can overlap (a snip, a deletion, a folder change); only the newest one draws.
  const request = ++recentsRequest;
  const items = await call('recent_captures');
  if (items === INVOKE_FAILED) return false;
  if (request !== recentsRequest) return true;
  renderRecents(items);
  return true;
}

function baseName(path) {
  const parts = String(path).split(/[\\/]/);
  return parts.at(-1) || String(path);
}

function codeToKeyName(code) {
  if (code.startsWith('Key')) return code.slice(3);
  if (code.startsWith('Digit')) return code.slice(5);
  if (code.startsWith('Numpad')) {
    const key = code.slice(6);
    if (key >= '0' && key <= '9') return `Num${key}`;
    const numpadMap = {
      Add: 'NumAdd', Subtract: 'NumSubtract', Multiply: 'NumMultiply',
      Divide: 'NumDivide', Decimal: 'NumDecimal', Enter: 'NumEnter',
    };
    return numpadMap[key] || key;
  }
  if (/^F\d+$/.test(code)) return code;
  const codeMap = {
    Space: 'Space', Enter: 'Enter', Tab: 'Tab',
    ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right',
    Home: 'Home', End: 'End', PageUp: 'PageUp', PageDown: 'PageDown',
    Insert: 'Insert', PrintScreen: 'PrintScreen', ScrollLock: 'ScrollLock', Pause: 'Pause',
    Minus: '-', Equal: '=', BracketLeft: '[', BracketRight: ']',
    Semicolon: ';', Quote: "'", Backquote: '`', Backslash: '\\',
    Comma: ',', Period: '.', Slash: '/',
  };
  return codeMap[code] || null;
}

function hotkeyError(input, message = '') {
  input.parentElement.querySelector('.hotkey-error').textContent = message;
}

function setupHotkeyRecorder(input) {
  let confirmedValue = '';

  input.addEventListener('focus', () => {
    confirmedValue = config.hotkeys[input.dataset.hotkey] || '';
    input.value = confirmedValue;
    hotkeyError(input);
    void call('pause_shortcuts');
  });

  input.addEventListener('keydown', (event) => {
    // Tab and Shift+Tab keep moving focus: a hotkey field must not trap the keyboard.
    if (event.key === 'Tab' && !event.ctrlKey && !event.altKey && !event.metaKey) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();

    if (event.code === 'Delete' || event.code === 'Backspace') {
      confirmedValue = '';
      input.value = '';
      config.hotkeys[input.dataset.hotkey] = '';
      hotkeyError(input);
      void saveConfig();
      input.blur();
      return;
    }

    if (event.code === 'Escape') {
      input.value = confirmedValue;
      hotkeyError(input);
      input.blur();
      return;
    }

    const parts = [];
    if (event.ctrlKey) parts.push('Ctrl');
    if (event.shiftKey) parts.push('Shift');
    if (event.altKey) parts.push('Alt');

    const modifierCodes = [
      'ControlLeft', 'ControlRight', 'ShiftLeft', 'ShiftRight',
      'AltLeft', 'AltRight', 'MetaLeft', 'MetaRight',
    ];

    if (modifierCodes.includes(event.code)) {
      input.value = `${parts.join('+')}+...`;
      return;
    }

    const keyName = codeToKeyName(event.code);
    if (!keyName) {
      input.value = confirmedValue;
      hotkeyError(input, 'Choose a supported non-modifier key.');
      return;
    }

    // Mirrors the backend rule: a bare typing key would be swallowed in every application.
    if (parts.length === 0 && !STANDALONE_KEYS.test(keyName)) {
      input.value = confirmedValue;
      hotkeyError(input, 'Add Ctrl, Shift or Alt to this key.');
      return;
    }

    parts.push(keyName);
    const candidate = parts.join('+');
    const duplicate = Object.entries(config.hotkeys).find(([mode, binding]) => (
      mode !== input.dataset.hotkey && binding && binding.toLowerCase() === candidate.toLowerCase()
    ));

    if (duplicate) {
      input.value = candidate;
      hotkeyError(input, `Already used by ${MODE_LABELS[duplicate[0]]}.`);
      return;
    }

    confirmedValue = candidate;
    input.value = candidate;
    config.hotkeys[input.dataset.hotkey] = candidate;
    hotkeyError(input);
    void saveConfig();
    input.blur();
  });

  input.addEventListener('keyup', (event) => {
    event.preventDefault();
    if (!input.value.endsWith('+...')) return;

    const parts = [];
    if (event.ctrlKey) parts.push('Ctrl');
    if (event.shiftKey) parts.push('Shift');
    if (event.altKey) parts.push('Alt');
    if (parts.length > 0) {
      input.value = `${parts.join('+')}+...`;
    } else {
      input.value = confirmedValue;
      hotkeyError(input, 'Include a non-modifier key.');
    }
  });

  input.addEventListener('blur', async () => {
    if (input.value.endsWith('+...') || input.value !== confirmedValue) input.value = confirmedValue;
    await saveQueue;
    await call('resume_shortcuts');
  });
}

function bindConfigControls() {
  elements.adminSwitch.addEventListener('change', () => {
    config.behavior.run_as_admin = elements.adminSwitch.checked;
    updateElevationNote();
    void saveConfig();
  });
  elements.closeTraySwitch.addEventListener('change', () => { config.behavior.close_to_tray = elements.closeTraySwitch.checked; void saveConfig(); });
  elements.startMinimizedSwitch.addEventListener('change', () => { config.behavior.start_minimized = elements.startMinimizedSwitch.checked; void saveConfig(); });
  elements.updatesSwitch.addEventListener('change', () => { config.behavior.check_updates = elements.updatesSwitch.checked; void saveConfig(); });
  elements.checkUpdatesButton.addEventListener('click', () => { void checkForUpdates(); });
  elements.installUpdateButton.addEventListener('click', () => { void installUpdate(); });
  elements.previewSelect.addEventListener('change', () => {
    const choice = elements.previewSelect.value;
    config.behavior.show_flyout = choice !== 'off';
    if (choice !== 'off') config.behavior.preview_seconds = Number(choice);
    elements.windowsNotificationsSwitch.disabled = !config.behavior.show_flyout;
    void saveConfig();
  });
  elements.windowsNotificationsSwitch.addEventListener('change', () => { config.behavior.windows_notifications = elements.windowsNotificationsSwitch.checked; void saveConfig(); });
  const bindWindowsShortcut = (control, field) => {
    control.addEventListener('change', () => {
      config.behavior[field] = control.checked;
      updateWindowsShortcuts();
      void saveConfig().then(refreshWindowsShortcutStatus);
    });
  };
  bindWindowsShortcut(elements.screenclipSwitch, 'handle_screenclip');
  bindWindowsShortcut(elements.printScreenSwitch, 'handle_print_screen');
  bindWindowsShortcut(elements.windowsOnlySwitch, 'windows_shortcuts_only');
  elements.defaultAppsButton.addEventListener('click', () => { void call('open_default_apps'); });
  elements.keyboardSettingsButton.addEventListener('click', () => { void call('open_keyboard_settings'); });
  elements.cursorSwitch.addEventListener('change', () => { config.capture.include_cursor = elements.cursorSwitch.checked; void saveConfig(); });
  elements.soundSelect.addEventListener('change', () => {
    config.capture.sound = elements.soundSelect.value;
    elements.soundPreviewButton.disabled = config.capture.sound === 'off';
    if (config.capture.sound !== 'off') void call('preview_sound', { sound: config.capture.sound });
    void saveConfig();
  });
  elements.soundPreviewButton.addEventListener('click', () => { void call('preview_sound', { sound: elements.soundSelect.value }); });
  elements.magnifierSwitch.addEventListener('change', () => { config.appearance.show_magnifier = elements.magnifierSwitch.checked; void saveConfig(); });
  elements.autoSaveSwitch.addEventListener('change', () => { config.output.auto_save = elements.autoSaveSwitch.checked; void saveConfig(); });
  elements.clipboardSwitch.addEventListener('change', () => { config.output.copy_to_clipboard = elements.clipboardSwitch.checked; void saveConfig(); });

  elements.defaultModeSelect.addEventListener('change', () => selectMode(elements.defaultModeSelect.value));
  elements.captureDelay.addEventListener('change', () => {
    config.capture.delay_seconds = Number(elements.captureDelay.value);
    setControlValue(elements.settingsDelay, config.capture.delay_seconds, true);
    void saveConfig();
  });
  elements.settingsDelay.addEventListener('change', () => {
    config.capture.delay_seconds = Number(elements.settingsDelay.value);
    setControlValue(elements.captureDelay, config.capture.delay_seconds, true);
    void saveConfig();
  });
  elements.formatSelect.addEventListener('change', () => {
    config.output.format = elements.formatSelect.value;
    updateOutputDetails();
    void saveConfig();
  });
  elements.qualityRange.addEventListener('input', () => {
    config.output.jpeg_quality = Number(elements.qualityRange.value);
    elements.qualityValue.value = elements.qualityRange.value;
    elements.qualityValue.textContent = elements.qualityRange.value;
    debounceSave('jpeg-quality');
  });
  elements.filenamePattern.addEventListener('input', () => {
    config.output.filename_pattern = elements.filenamePattern.value;
    updateOutputDetails();
    debounceSave('filename-pattern');
  });
}

/** Wires the custom caption buttons. Close goes through the normal close request, so
 * "close to tray" still applies. */
function bindWindowControls() {
  const appWindow = window.__TAURI__?.window?.getCurrentWindow?.();
  if (!appWindow) return;
  document.getElementById('minimizeButton').addEventListener('click', () => {
    appWindow.minimize().catch(reportError);
  });
  document.getElementById('closeWindowButton').addEventListener('click', () => {
    appWindow.close().catch(reportError);
  });
}

function bindUi() {
  bindWindowControls();
  void showVersion();
  document.getElementById('settingsButton').addEventListener('click', showSettingsView);
  document.getElementById('backButton').addEventListener('click', showCaptureView);
  document.getElementById('openFolderButton').addEventListener('click', () => { void call('open_output_dir'); });
  document.getElementById('openSettingsFolderButton').addEventListener('click', () => { void call('open_output_dir'); });
  document.getElementById('recentFolderButton').addEventListener('click', () => { void call('open_output_dir'); });
  document.getElementById('newSnipButton').addEventListener('click', () => { void call('start_snip', { mode: currentMode, delay: null }); });
  document.getElementById('browseFolderButton').addEventListener('click', async () => {
    const path = await call('pick_output_directory');
    if (path !== INVOKE_FAILED && path !== null) {
      config.output.directory = path;
      renderConfig(true);
      await saveConfig();
      await refreshRecents();
    }
  });
  document.getElementById('restoreDefaultsButton').addEventListener('click', async () => {
    const restored = await call('reset_config');
    if (restored !== INVOKE_FAILED) {
      config = restored;
      configVersion += 1;
      renderConfig(false);
      await refreshRecents();
    }
  });
  elements.autostartSwitch.addEventListener('change', async () => {
    const previous = autostartEnabled;
    autostartEnabled = elements.autostartSwitch.checked;
    const result = await call('set_autostart', { enabled: autostartEnabled });
    if (result === INVOKE_FAILED) {
      autostartEnabled = previous;
      elements.autostartSwitch.checked = previous;
    }
    updateElevationNote();
  });

  for (const tile of elements.modeTiles) {
    tile.addEventListener('click', () => selectMode(tile.dataset.mode));
    tile.addEventListener('keydown', (event) => {
      const index = elements.modeTiles.indexOf(tile);
      let nextIndex = null;
      if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') nextIndex = (index + elements.modeTiles.length - 1) % elements.modeTiles.length;
      if (event.key === 'ArrowRight' || event.key === 'ArrowDown') nextIndex = (index + 1) % elements.modeTiles.length;
      if (event.key === 'Home') nextIndex = 0;
      if (event.key === 'End') nextIndex = elements.modeTiles.length - 1;
      if (nextIndex !== null) {
        event.preventDefault();
        selectMode(elements.modeTiles[nextIndex].dataset.mode, true);
      }
    });
  }

  for (const input of elements.hotkeyInputs) setupHotkeyRecorder(input);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !elements.settingsView.hidden) {
      event.preventDefault();
      showCaptureView();
    }
  });
  bindConfigControls();
}

document.addEventListener('contextmenu', handleContextMenu, true);

async function setupEventListeners() {
  const subscriptions = [
    listenEvent('capture-finished', (event) => {
      void refreshRecents();
      const result = event.payload;
      if (result.saved && result.path) showSuccess(`Saved ${baseName(result.path)}`);
      else if (result.copied) showSuccess('Copied to clipboard');
      else showSuccess('Capture finished');
    }),
    listenEvent('capture-failed', (event) => setStatus(errorMessage(event.payload), 'error')),
    listenEvent('recents-changed', () => { void refreshRecents(); }),
    listenEvent('capture-cancelled', () => showSuccess('Capture cancelled')),
    listenEvent('shortcut-errors', (event) => {
      const failed = Array.isArray(event.payload) ? event.payload.join(', ') : '';
      if (failed) setStatus(`Hotkey not available, another program may use it: ${failed}`, 'error');
    }),
    listenEvent('config-changed', (event) => {
      if (pendingWrites > 0) return;
      const outputDirectoryChanged = config?.output?.directory !== event.payload.output.directory;
      config = preserveActiveEdit(event.payload);
      configVersion += 1;
      renderConfig(true);
      if (outputDirectoryChanged) void refreshRecents();
    }),
  ];
  await Promise.all(subscriptions);
}

async function initialize() {
  const tauri = window.__TAURI__;
  if (!tauri?.core?.invoke || !tauri?.event?.listen || !tauri?.window) {
    setStatus('Tauri API unavailable. Open Snippy through the desktop application.', 'error');
    return;
  }

  invokeCommand = tauri.core.invoke;
  listenEvent = tauri.event.listen;
  bindUi();

  const loadedConfig = await call('get_config');
  if (loadedConfig === INVOKE_FAILED) return;
  config = loadedConfig;
  renderConfig(false);

  // Show the window once the panel is painted; recents get a short head start so the grid does not
  // pop in right after the window appears. Slower checks (autostart runs schtasks) come after.
  const recentsLoaded = refreshRecents();
  await Promise.race([recentsLoaded, delay(RECENTS_HEAD_START_MS)]);
  await nextFrame();
  reportReady();
  void refreshWindowsShortcutStatus();

  const [autostart, elevated] = await Promise.all([call('get_autostart'), call('is_elevated')]);
  if (autostart !== INVOKE_FAILED) {
    autostartEnabled = autostart;
    elements.autostartSwitch.checked = autostart;
  }
  if (elevated !== INVOKE_FAILED) {
    elements.elevatedNote.textContent = elevated ? 'This process is elevated.' : 'This process is not elevated.';
  }

  try {
    await setupEventListeners();
  } catch (error) {
    console.error('Could not subscribe to backend events:', error);
    setStatus(`Could not subscribe to backend events: ${errorMessage(error)}`, 'error');
  }

  await recentsLoaded;
  if (statusKind !== 'error') showIdleStatus();
}

initialize()
  .catch((error) => {
    console.error('Snippy initialization failed:', error);
    setStatus(`Snippy initialization failed: ${errorMessage(error)}`, 'error');
  })
  // Even a failed start must not leave the window hidden: it shows the error instead.
  .finally(reportReady);

// Coming back from Windows Settings: show whether Snippy is now the default app for the keys.
window.addEventListener('focus', () => { if (config) void refreshWindowsShortcutStatus(); });
