# Snippy — internal contract (Rust backend <-> WebView frontends)

Windows-only Tauri 2 app. No bundler, no framework: plain HTML/CSS/JS served from `ui/`,
`withGlobalTauri: true`, so the frontend uses `window.__TAURI__.core.invoke` and
`window.__TAURI__.event.listen`.

Everything in this file is normative. Backend and frontends are written in separate passes and
must agree on it exactly.

## Windows (Tauri labels)

| label     | html            | role                                                     |
|-----------|-----------------|----------------------------------------------------------|
| `main`    | `index.html`    | control panel + settings                                  |
| `overlay` | `overlay.html`  | fullscreen selection surface over the virtual screen      |
| `flyout`  | `flyout.html`   | post-capture preview toast (bottom-right, no focus steal) |
| `editor`  | `editor.html`   | annotation editor and text recognition; created on first use |

`overlay` and `flyout` are pre-created hidden at startup and reused (never closed).
`main` is created hidden with a background in the theme colour; at startup it is shown when the
page calls `main_ready` after its first render (3 s fallback), so it never opens blank or white.

## Coordinate system

All geometry crossing the IPC boundary is in **physical pixels of the Windows virtual screen**,
translated so that the top-left of the virtual screen is `(0, 0)`.
The backend keeps the real virtual-screen origin internally and never exposes it.

The overlay window is positioned/sized with `PhysicalPosition`/`PhysicalSize`. The overlay canvas
uses `canvas.width/height = capture size` and CSS size = window size, so the frontend converts
mouse coordinates with `canvas.width / canvas.getBoundingClientRect().width` — never with
`devicePixelRatio`.

## Config

Stored at `%APPDATA%\Snippy\config.json` (created on first run). Every struct and field is
`#[serde(default)]` so partial/older files load. Saves are atomic (temp file + rename).

```jsonc
{
  "hotkeys": {
    "rectangle":  "Ctrl+Shift+S",
    "freeform":   "Ctrl+Shift+D",
    "window":     "Ctrl+Shift+W",
    "fullscreen": "Ctrl+Shift+F"
  },
  "capture": {
    "default_mode": "rectangle",   // rectangle | freeform | window | fullscreen
    "delay_seconds": 0,            // 0..=10
    "include_cursor": false,
    "sound": "off"                 // off | shutter | click | click-high | tick | ding
  },
  "output": {
    "directory": "<Pictures>\\Snippy",
    "format": "png",               // png | jpeg
    "jpeg_quality": 90,            // 1..=100
    "filename_pattern": "Snippy_%Y%m%d_%H%M%S",
    "auto_save": true,
    "copy_to_clipboard": true
  },
  "behavior": {
    "run_as_admin": false,
    "close_to_tray": true,
    "start_minimized": false,
    "show_flyout": true,
    "handle_screenclip": false,    // Win+Shift+S snips (see "Windows keys")
    "handle_print_screen": false,  // Print Screen snips
    "windows_shortcuts_only": false, // own hotkeys off; reset to false when neither key is on
    "windows_notifications": false, // announce snips as Windows notifications instead of the flyout
    "preview_seconds": 5,          // 3 | 5 | 10: how long the flyout stays on screen
    "check_updates": true          // check GitHub for a newer signed release
  },
  "appearance": {
    "show_magnifier": true
  }
}
```

JSON keys are snake_case in both directions; the frontend sends back the same shape it received.

Hotkeys need Ctrl, Shift or Alt unless the key is F1–F24, PrintScreen, Pause or ScrollLock; the
backend skips (and logs) any other bare key. `output.filename_pattern` must expand to a valid
Windows file name: no `\ / : * ? " < > |` or control characters, no trailing dot or space, at most
200 characters, and not a reserved device name such as `CON` or `NUL.backup`.

## Commands

```ts
get_config()                                  -> Config
set_config(config: Config)                    -> Config    // persists, re-registers hotkeys, emits config-changed; re-seeds recents when output.directory changes
reset_config()                                -> Config
pick_output_directory()                       -> string | null   // native folder picker

preview_sound(sound: string)                 -> void      // plays one capture sound once
check_for_updates()                          -> { current, latest|null, available, page|null }
install_update()                             -> void      // downloads, verifies, swaps the exe, restarts
                                                          // events: update-available { version, current }
start_snip(mode: string, delay: number|null)  -> void      // delay null => config.capture.delay_seconds
cancel_snip()                                 -> void

overlay_metadata()                            -> OverlayMeta
overlay_pixels()                              -> ArrayBuffer     // RGBA8, width*height*4
overlay_ready()                               -> void            // overlay painted the frame; show it

finish_rect(x, y, width, height: number)      -> CaptureResult
finish_freeform(points: [number, number][])   -> CaptureResult
finish_window(windowId: number)               -> CaptureResult   // id from OverlayMeta.windows
finish_monitor(monitorIndex: number)          -> CaptureResult

recent_captures()                             -> CaptureItem[]   // newest first, max 8
open_path(path: string)                       -> void            // open file with default app
reveal_path(path: string)                     -> void            // explorer /select,
open_output_dir()                             -> void
copy_capture(path: string)                    -> void
delete_capture(path: string)                  -> void            // moves the file to the Recycle Bin
// open_path, reveal_path, copy_capture and delete_capture only accept an existing .png/.jpg/.jpeg
// file directly inside output.directory; any other path is rejected with an error.

pause_shortcuts()                             -> void
resume_shortcuts()                            -> void
get_autostart()                               -> boolean
set_autostart(enabled: boolean)               -> void
is_elevated()                                 -> boolean
hide_flyout()                                 -> void
show_main_window()                            -> void
open_default_apps()                           -> void            // Settings > Default apps, Snippy page
windows_shortcut_status()                     -> { win_shift_s_taken, print_screen_taken, print_screen_by_windows: boolean, closed_handler: string | null, closed_handler_is_snippy: boolean }
open_keyboard_settings()                      -> void            // Windows keyboard settings (Print Screen option)
main_ready()                                  -> void            // main page rendered; show the window

open_editor(path: string | null)              -> void            // capture in the output folder, or the latest snip
editor_info()                                 -> { name, path: string | null, width, height }
editor_pixels()                               -> ArrayBuffer     // RGBA8 of the open image
editor_ocr(<raw PNG body>)                    -> { language, angle: number | null, lines: { text, words: { text, x, y, width, height }[] }[] }   // header snippy-language; the editor sends what it shows, marks included
ocr_languages()                               -> { tag, name }[] // OCR languages Windows has installed
editor_export(<raw PNG body>)                 -> string | null   // header snippy-action: copy | save | save-as
```

```ts
type OverlayMeta = {
  mode: "rectangle" | "freeform" | "window" | "fullscreen";
  width: number; height: number;               // capture size, physical px
  scale: number;                               // scale factor of the primary monitor, informational
  monitors: { index: number; x: number; y: number; width: number; height: number;
              scale: number; primary: boolean }[];
  windows:  { id: number; title: string; x: number; y: number;
              width: number; height: number }[];   // top-most first
};

type CaptureResult = {
  path: string | null;      // null when auto_save is off
  width: number; height: number;
  saved: boolean; copied: boolean;
  thumbnail: string;        // data:image/png;base64,... max 320x200
};

type CaptureItem = { path: string; name: string; width: number; height: number;
                     timestamp: number; thumbnail: string };
```

On the first `recent_captures` call, the backend scans `output.directory` on a blocking worker and
seeds its in-memory cache from the eight newest `.png`, `.jpg`, or `.jpeg` files by modification
time. It performs the same scan when `set_config` changes `output.directory`. Dimensions,
timestamps (mtime in milliseconds), and PNG data-URL thumbnails come from the files themselves;
unreadable or corrupt files are logged and skipped. Session captures remain at the front, missing
cached files are dropped, and the directory remains the source of truth (there is no persisted
recent-capture index).

Command errors are returned as `Err(String)` (human-readable, no `unwrap`/panic paths).

## Events

Backend -> frontend:

| event              | payload         | consumers      |
|--------------------|-----------------|----------------|
| `capture-started`  | `string` (mode) | main           |
| `capture-finished` | `CaptureResult` | main, flyout   |
| `capture-failed`   | `string`        | main, flyout   |
| `capture-cancelled`| `null`          | main           |
| `config-changed`   | `Config`        | main, overlay  |
| `overlay-refresh`  | `null`          | overlay        |
| `editor-load`      | `null`          | editor         |
| `recents-changed`  | `null`          | main           |

## Capture flow

1. `start_snip(mode, delay)` returns immediately; work happens on a worker thread.
2. Visible Snippy windows are hidden; the thread waits only if something was actually visible.
3. `delay` seconds are slept, then the whole virtual screen is grabbed once (GDI `BitBlt`
   `SRCCOPY | CAPTUREBLT`, optional cursor drawn with `DrawIconEx`).
4. `fullscreen` mode finishes immediately from the monitor under the cursor.
   Other modes store the capture session (frame, mode, windows, monitors) in `AppState`,
   size/position the still-hidden overlay over the whole virtual screen and emit `overlay-refresh`.
5. The overlay loads metadata and pixels, paints the frame, then calls `overlay_ready`, which shows
   and focuses the window. If it has not done so within 1.5 s the backend shows it anyway. Showing
   only after the paint keeps the previous capture from flashing.
6. `finish_*` hides the overlay **before** encoding/saving, then saves, copies, pushes to recents,
   shows the flyout and emits `capture-finished`. A clipboard failure is reported only when the
   image was not saved either.
7. Any failure hides the overlay, emits `capture-failed` and (when `show_flyout` is on) shows the
   flyout with the error.

Window and monitor snips are cropped out of the frame captured in step 3, so what the overlay
highlights is exactly what gets saved.

## Windows keys (Win+Shift+S, Print Screen)

Snippy uses only supported mechanisms here: no keyboard hooks (they look like keyloggers to
anti-cheat and antivirus software) and no writes to Windows' protected default-app choice.

- **Win+Shift+S.** Explorer holds it and opens an `ms-screenclip:` link with the default app,
  normally the Snipping Tool. With the option on, Snippy registers itself per user (HKCU) as an app
  for that link; the user makes it the default once in Settings > Default apps (Windows requires the
  user's choice; it cannot be set silently). The settings page names the current app
  (`closed_handler`), opens Snippy's Default apps page and lists the steps. Windows then starts
  `snippy.exe --screenclip "<link>"`, which snips in `capture.default_mode`. Where Explorer leaves
  the chord free (trimmed builds), Snippy registers it as an ordinary hotkey instead.
- **Print Screen.** Registered as an ordinary hotkey. When Windows' "Use the Print Screen key to open
  screen capture" is on (`HKCU\Control Panel\Keyboard\PrintScreenKeyForSnippingEnabled`; when unset,
  on from Windows 11 build 22621, off on Windows 10), Windows wins the key; the settings page says so
  and opens Keyboard settings for the user to turn it off. Snippy never changes that value itself.
- An elevated instance admits `WM_COPYDATA` from lower integrity on the single-instance window only,
  so link launches still reach it.

## Editor and text recognition

The editor keeps every mark (pen, highlighter, arrow, line, rectangle, ellipse, text, emoji,
pixelate) as a vector object in image coordinates and flattens them only when copying or saving.
Save writes `<name>-edited.png` next to the original (never overwriting); Save as… asks for a path.
Text recognition uses `Windows.Media.Ocr`, built into Windows 10/11, with the OCR languages Windows
installed; nothing is downloaded. "Auto" prefers an installed non-English engine, because those
also read Latin script while the English engine misreads other scripts.

## Windows notifications

With `behavior.windows_notifications` on, Snippy describes itself under
`HKCU\Software\Classes\AppUserModelId\com.snippy.tool` and registers the `snippy:` link scheme
(`HKCU\Software\Classes\snippy`). The notification and its buttons open `snippy://edit?path=…`
and `snippy://reveal?path=…`; Windows starts `snippy.exe <link>` and the running instance follows
it. Paths are re-validated against the output folder. Turning the option off removes both keys.

Snippy has a single dark theme: `color-scheme: dark` for every page, dark native title bars and a
dark window background before the first paint. There is no theme setting.
