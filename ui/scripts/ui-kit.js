'use strict';

// Shared UI helpers for the main window: sprite icons and the accessible context menu.
// Loaded before main.js; everything here is self-contained.

const menuElement = document.getElementById('contextMenu');
const contextMenuState = {
  anchor: null,
  buttons: [],
  onError: (error) => console.error(error),
};


const SVG_NS = 'http://www.w3.org/2000/svg';

/** Builds an icon that references a glyph in icons.svg, which ships inside the binary. */
function spriteIcon(name, size = 16) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('class', 'icon');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('aria-hidden', 'true');
  const use = document.createElementNS(SVG_NS, 'use');
  use.setAttribute('href', `icons.svg#${name}`);
  svg.append(use);
  return svg;
}

function closeContextMenu(restoreFocus = false) {
  if (menuElement.hidden) return;

  const anchor = contextMenuState.anchor;
  menuElement.hidden = true;
  menuElement.setAttribute('aria-hidden', 'true');
  menuElement.classList.remove('is-open');
  menuElement.replaceChildren();
  if (anchor) anchor.setAttribute('aria-expanded', 'false');
  contextMenuState.anchor = null;
  contextMenuState.buttons = [];

  if (restoreFocus && anchor?.isConnected) anchor.focus({ preventScroll: true });
}

function activateContextMenuItem(item, restoreFocus) {
  let result;
  try {
    result = item.run();
  } catch (error) {
    closeContextMenu(restoreFocus);
    contextMenuState.onError(error);
    return;
  }

  closeContextMenu(restoreFocus);
  Promise.resolve(result).catch((error) => {
    console.error('Context menu action failed:', error);
    contextMenuState.onError(error);
  });
}

/** Opens the shared menu at the pointer (or below the anchor for keyboard). */
function openContextMenu(anchorEvent, items, onError) {
  const anchor = anchorEvent.currentTarget instanceof HTMLElement
    ? anchorEvent.currentTarget
    : anchorEvent.target instanceof Element
      ? anchorEvent.target.closest('.recent-button')
      : null;
  if (!(anchor instanceof HTMLElement) || items.length === 0) return;

  closeContextMenu(false);
  contextMenuState.anchor = anchor;
  contextMenuState.onError = onError;
  anchor.setAttribute('aria-expanded', 'true');
  menuElement.replaceChildren();

  let previousWasDanger = false;
  for (const [index, item] of items.entries()) {
    if (item.danger && index > 0 && !previousWasDanger) {
      const separator = document.createElement('div');
      separator.className = 'context-menu-separator';
      separator.setAttribute('role', 'separator');
      menuElement.append(separator);
    }

    const button = document.createElement('button');
    button.className = 'context-menu-item';
    if (item.danger) button.classList.add('danger');
    button.type = 'button';
    button.tabIndex = -1;
    button.setAttribute('role', 'menuitem');

    const icon = spriteIcon(item.icon);
    icon.classList.add('context-menu-icon');

    const label = document.createElement('span');
    label.textContent = item.label;
    button.append(icon, label);
    button.addEventListener('click', (event) => {
      activateContextMenuItem(item, event.detail === 0);
    });
    menuElement.append(button);
    contextMenuState.buttons.push(button);
    previousWasDanger = Boolean(item.danger);
  }

  menuElement.hidden = false;
  menuElement.setAttribute('aria-hidden', 'false');
  menuElement.classList.remove('is-open');
  menuElement.style.left = '0px';
  menuElement.style.top = '0px';

  const anchorRect = anchor.getBoundingClientRect();
  const keyboardAnchor = anchorEvent.type === 'keydown';
  const pointerX = keyboardAnchor ? anchorRect.left : anchorEvent.clientX;
  const pointerY = keyboardAnchor ? anchorRect.bottom : anchorEvent.clientY;
  const menuRect = menuElement.getBoundingClientRect();
  const flipX = pointerX + menuRect.width > window.innerWidth;
  const flipY = pointerY + menuRect.height > window.innerHeight;
  const maxLeft = Math.max(0, window.innerWidth - menuRect.width);
  const maxTop = Math.max(0, window.innerHeight - menuRect.height);
  const left = Math.min(Math.max(0, flipX ? pointerX - menuRect.width : pointerX), maxLeft);
  const top = Math.min(Math.max(0, flipY ? pointerY - menuRect.height : pointerY), maxTop);

  menuElement.style.left = `${left}px`;
  menuElement.style.top = `${top}px`;
  menuElement.style.transformOrigin = `${flipX ? 'right' : 'left'} ${flipY ? 'bottom' : 'top'}`;
  menuElement.style.setProperty('--context-menu-scale-x', String(Math.max(0, menuRect.width - 4) / menuRect.width));
  menuElement.style.setProperty('--context-menu-scale-y', String(Math.max(0, menuRect.height - 4) / menuRect.height));
  void menuElement.offsetWidth;
  menuElement.classList.add('is-open');
  contextMenuState.buttons[0].focus({ preventScroll: true });
}

function handleMenuKeydown(event) {
  const buttons = contextMenuState.buttons;
  if (buttons.length === 0) return;

  const currentIndex = Math.max(0, buttons.indexOf(document.activeElement));
  let nextIndex = null;
  if (event.key === 'ArrowUp') nextIndex = (currentIndex + buttons.length - 1) % buttons.length;
  if (event.key === 'ArrowDown') nextIndex = (currentIndex + 1) % buttons.length;
  if (event.key === 'Home') nextIndex = 0;
  if (event.key === 'End') nextIndex = buttons.length - 1;

  if (nextIndex !== null) {
    event.preventDefault();
    buttons[nextIndex].focus({ preventScroll: true });
    return;
  }

  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    buttons[currentIndex].click();
    return;
  }

  if (event.key === 'Escape') {
    event.preventDefault();
    event.stopPropagation();
    closeContextMenu(true);
  }
}

document.addEventListener('pointerdown', (event) => {
  if (!menuElement.hidden && !menuElement.contains(event.target)) closeContextMenu(false);
}, true);
document.addEventListener('scroll', () => closeContextMenu(false), true);
window.addEventListener('blur', () => closeContextMenu(false));
window.addEventListener('resize', () => closeContextMenu(false));
menuElement.addEventListener('keydown', handleMenuKeydown);
