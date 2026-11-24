(() => {
  "use strict";

  document.addEventListener("contextmenu", (event) => {
    event.preventDefault();
  }, true);

  const DEFAULT_PREVIEW_SECONDS = 5;
  const ERROR_DISMISS_MS = 8000;
  const UPDATE_DISMISS_MS = 15000;
  const COPY_CONFIRM_MS = 1200;

  const tauri = window.__TAURI__;
  const invoke = tauri?.core?.invoke;
  const listen = tauri?.event?.listen;

  const card = document.getElementById("flyout");
  const preview = document.getElementById("preview");
  const thumbnail = document.getElementById("thumbnail");
  const errorVisual = document.getElementById("error-visual");
  const title = document.getElementById("flyout-title");
  const detail = document.getElementById("detail");
  const closeButton = document.getElementById("close");
  const actions = document.getElementById("actions");
  const editButton = document.getElementById("edit");
  const openButton = document.getElementById("open");
  const folderButton = document.getElementById("folder");
  const copyButton = document.getElementById("copy");
  const deleteButton = document.getElementById("delete");
  const progress = document.getElementById("progress");
  const updateVisual = document.getElementById("update-visual");
  const updateActions = document.getElementById("update-actions");
  const updateNowButton = document.getElementById("update-now");
  const updateLaterButton = document.getElementById("update-later");

  // How long a finished snip stays on screen, from Settings (3, 5 or 10 seconds).
  let previewMs = DEFAULT_PREVIEW_SECONDS * 1000;

  function applyConfig(config) {
    const seconds = Number(config?.behavior?.preview_seconds);
    previewMs = (Number.isFinite(seconds) && seconds > 0 ? seconds : DEFAULT_PREVIEW_SECONDS) * 1000;
  }

  let capturePath = null;
  let dismissDuration = previewMs;
  let dismissTimer = null;
  let pointerInside = false;
  let focusInside = false;
  let autoDismiss = false;
  let actionInFlight = false;

  function errorMessage(error) {
    if (typeof error === "string" && error.trim()) {
      return error;
    }

    if (error instanceof Error && error.message) {
      return error.message;
    }

    return "Something went wrong.";
  }

  async function call(command, args) {
    if (typeof invoke !== "function") {
      return { ok: false, error: "Snippy backend is unavailable." };
    }

    try {
      return { ok: true, value: await invoke(command, args) };
    } catch (error) {
      return { ok: false, error: errorMessage(error) };
    }
  }

  function clearDismissTimer() {
    if (dismissTimer !== null) {
      window.clearTimeout(dismissTimer);
      dismissTimer = null;
    }
  }

  function resetProgress(run) {
    progress.style.setProperty("--dismiss-duration", `${dismissDuration}ms`);
    progress.classList.remove("is-running");
    void progress.offsetWidth;

    if (run) {
      progress.classList.add("is-running");
    }
  }

  function isPaused() {
    return pointerInside || focusInside;
  }

  function startDismissTimer(duration = dismissDuration) {
    dismissDuration = duration;
    clearDismissTimer();

    const shouldRun = autoDismiss && !isPaused();
    resetProgress(shouldRun);

    if (shouldRun) {
      dismissTimer = window.setTimeout(() => {
        void dismiss();
      }, dismissDuration);
    }
  }

  function pauseDismissTimer() {
    clearDismissTimer();
    resetProgress(false);
  }

  function replayEntrance() {
    card.classList.remove("entering");
    void card.offsetWidth;
    card.classList.add("entering");
  }

  function setDetail(message, danger = false) {
    detail.textContent = message;
    detail.classList.toggle("is-danger", danger);
  }

  function setActionAvailability(hasPath) {
    // The editor also opens unsaved snips (the latest capture is kept in memory).
    editButton.disabled = false;
    openButton.disabled = !hasPath;
    folderButton.disabled = !hasPath;
    copyButton.disabled = !hasPath;
    deleteButton.disabled = !hasPath;
  }

  function fileName(path) {
    if (!path) {
      return "Unsaved capture";
    }

    const parts = path.split(/[\\/]/);
    return parts.at(-1) || path;
  }

  function makePreviewInteractive(path) {
    if (path) {
      preview.setAttribute("role", "button");
      preview.setAttribute("tabindex", "0");
      preview.setAttribute("aria-label", "Open captured screenshot");
    } else {
      preview.removeAttribute("role");
      preview.removeAttribute("tabindex");
      preview.removeAttribute("aria-label");
    }
  }

  function renderSuccess(result) {
    capturePath = typeof result?.path === "string" && result.path ? result.path : null;
    const width = Number.isFinite(result?.width) ? result.width : 0;
    const height = Number.isFinite(result?.height) ? result.height : 0;

    title.textContent = result?.saved === false ? "Copied to clipboard" : "Snippet saved";
    if (result?.save_error) {
      // The snip is safe on the clipboard (and in the editor), but the file was not written.
      setDetail(`Not saved: ${result.save_error}`, true);
    } else {
      setDetail(`${width} × ${height} · ${fileName(capturePath)}`);
    }
    detail.title = capturePath || "";

    thumbnail.src = typeof result?.thumbnail === "string" ? result.thumbnail : "";
    thumbnail.hidden = false;
    errorVisual.hidden = true;
    updateVisual.hidden = true;
    updateActions.hidden = true;
    makePreviewInteractive(capturePath);

    actions.hidden = false;
    setActionAvailability(Boolean(capturePath));
    card.classList.remove("is-empty", "is-error", "is-update");

    actionInFlight = false;
    autoDismiss = true;
    dismissDuration = previewMs;
    replayEntrance();
    startDismissTimer();
  }

  function renderError(message) {
    capturePath = null;
    title.textContent = "Capture failed";
    setDetail(errorMessage(message), true);
    detail.title = errorMessage(message);

    thumbnail.removeAttribute("src");
    thumbnail.hidden = true;
    errorVisual.hidden = false;
    updateVisual.hidden = true;
    updateActions.hidden = true;
    makePreviewInteractive(null);

    actions.hidden = true;
    setActionAvailability(false);
    card.classList.remove("is-empty", "is-update");
    card.classList.add("is-error");

    actionInFlight = false;
    autoDismiss = true;
    dismissDuration = Math.max(previewMs, ERROR_DISMISS_MS);
    replayEntrance();
    startDismissTimer();
  }

  function renderUpdate(notice) {
    capturePath = null;
    title.textContent = "Update available";
    setDetail(notice?.rebuild
      ? `A fixed build of Snippy ${notice?.version ?? ""}`
      : `Snippy ${notice?.version ?? ""} · you have ${notice?.current ?? ""}`);
    detail.title = "";

    thumbnail.removeAttribute("src");
    thumbnail.hidden = true;
    errorVisual.hidden = true;
    updateVisual.hidden = false;
    makePreviewInteractive(null);

    actions.hidden = true;
    updateActions.hidden = false;
    updateNowButton.disabled = false;
    updateNowButton.textContent = "Update now";
    updateLaterButton.disabled = false;
    card.classList.remove("is-empty", "is-error");
    card.classList.add("is-update");

    actionInFlight = false;
    autoDismiss = true;
    dismissDuration = UPDATE_DISMISS_MS;
    replayEntrance();
    startDismissTimer();
  }

  async function installUpdate() {
    if (actionInFlight) {
      return;
    }
    actionInFlight = true;
    autoDismiss = false;
    pauseDismissTimer();
    updateNowButton.disabled = true;
    updateNowButton.textContent = "Updating…";
    updateLaterButton.disabled = true;
    setDetail("Downloading the update…");

    // On success Snippy restarts on the new version, so only a failure returns here.
    const result = await call("install_update");
    actionInFlight = false;
    updateLaterButton.disabled = false;
    if (!result.ok) {
      updateNowButton.disabled = false;
      updateNowButton.textContent = "Update now";
      keepOpenWithError(result.error);
    }
  }

  function keepOpenWithError(message) {
    autoDismiss = false;
    pauseDismissTimer();
    setDetail(message, true);
    detail.title = message;
  }

  async function dismiss() {
    autoDismiss = false;
    pauseDismissTimer();
    const result = await call("hide_flyout");

    if (!result.ok) {
      keepOpenWithError(result.error);
    }
  }

  async function runAndDismiss(command) {
    if (!capturePath || actionInFlight) {
      return;
    }

    actionInFlight = true;
    pauseDismissTimer();
    const result = await call(command, { path: capturePath });
    if (!result.ok) {
      // The file may be gone or its folder unreachable: say so instead of closing silently.
      actionInFlight = false;
      keepOpenWithError(result.error);
      return;
    }
    await dismiss();
  }

  async function copyCapture() {
    if (!capturePath || actionInFlight) {
      return;
    }

    actionInFlight = true;
    pauseDismissTimer();
    const result = await call("copy_capture", { path: capturePath });

    if (!result.ok) {
      actionInFlight = false;
      keepOpenWithError(result.error);
      return;
    }

    setDetail("Copied");
    detail.title = "Copied";
    actionInFlight = false;
    autoDismiss = true;
    startDismissTimer(COPY_CONFIRM_MS);
  }

  async function deleteCapture() {
    if (!capturePath || actionInFlight) {
      return;
    }

    actionInFlight = true;
    pauseDismissTimer();
    const result = await call("delete_capture", { path: capturePath });

    if (!result.ok) {
      actionInFlight = false;
      keepOpenWithError(result.error);
      return;
    }

    await dismiss();
  }

  function openPreview() {
    void runAndDismiss("open_path");
  }

  preview.addEventListener("click", openPreview);
  preview.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      openPreview();
    }
  });

  editButton.addEventListener("click", async () => {
    if (actionInFlight) {
      return;
    }
    actionInFlight = true;
    pauseDismissTimer();
    const result = await call("open_editor", { path: capturePath });
    if (!result.ok) {
      actionInFlight = false;
      keepOpenWithError(result.error);
      return;
    }
    await dismiss();
  });
  openButton.addEventListener("click", () => {
    void runAndDismiss("open_path");
  });
  folderButton.addEventListener("click", () => {
    void runAndDismiss("reveal_path");
  });
  copyButton.addEventListener("click", () => {
    void copyCapture();
  });
  deleteButton.addEventListener("click", () => {
    void deleteCapture();
  });
  updateNowButton.addEventListener("click", () => {
    void installUpdate();
  });
  updateLaterButton.addEventListener("click", () => {
    void dismiss();
  });
  closeButton.addEventListener("click", () => {
    void dismiss();
  });

  card.addEventListener("pointerenter", () => {
    pointerInside = true;
    pauseDismissTimer();
  });

  card.addEventListener("pointerleave", () => {
    pointerInside = false;
    if (!focusInside && autoDismiss) {
      startDismissTimer();
    }
  });

  card.addEventListener("focusin", () => {
    focusInside = true;
    pauseDismissTimer();
  });

  card.addEventListener("focusout", (event) => {
    if (!card.contains(event.relatedTarget)) {
      focusInside = false;
      if (!pointerInside && autoDismiss) {
        startDismissTimer();
      }
    }
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      void dismiss();
    }
  });

  function subscribe(eventName, handler) {
    if (typeof listen !== "function") {
      return;
    }

    try {
      Promise.resolve(listen(eventName, (event) => handler(event.payload))).catch((error) => {
        renderError(errorMessage(error));
      });
    } catch (error) {
      renderError(errorMessage(error));
    }
  }

  subscribe("capture-finished", renderSuccess);
  subscribe("capture-failed", renderError);
  subscribe("update-available", renderUpdate);
  subscribe("config-changed", applyConfig);
  void call("get_config").then((result) => {
    if (result.ok) {
      applyConfig(result.value);
    }
  });
})();
