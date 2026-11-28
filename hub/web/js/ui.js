// Shared Bencho-inspired UI helpers: copy-field morph, hold-to-confirm, and liquid glide tooltips.
import { copyBtnHtml, esc, showToast } from './api.js';
import { playDone, playTap, playToggle, playWarn } from './sound.js';

export const COPY_BTN_HTML = copyBtnHtml('复制内容');

export async function writeClipboard(text) {
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // fall through to textarea copy fallback
    }
  }
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.style.cssText = 'position:fixed;left:-9999px;opacity:0;';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}

export function initDelegatedCopy(rootEl) {
  if (!rootEl || rootEl.dataset.copyBound) return;
  rootEl.dataset.copyBound = '1';
  rootEl.addEventListener('click', async (ev) => {
    const btn = ev.target?.closest?.('.copy-btn');
    if (!btn || !rootEl.contains(btn)) return;
    ev.preventDefault();
    ev.stopPropagation();
    const box = btn.closest('.md-code-wrap, .install-box');
    const codeEl = box?.querySelector('code');
    const text = codeEl?.textContent || '';
    if (!text) return;
    const ok = await writeClipboard(text);
    if (!ok) {
      playWarn();
      showToast('复制失败，请手动选择复制');
      return;
    }
    playTap();
    btn.classList.add('is-copied');
    clearTimeout(btn._copyTimer);
    btn._copyTimer = setTimeout(() => btn.classList.remove('is-copied'), 1600);
  });
}

let lastShortHintAt = 0;

export function bindHoldAction(btn, action, { duration = 650, shortHint = '请长按确认操作' } = {}) {
  if (!btn) return;
  btn.classList.add('hold-btn');
  btn.style.setProperty('--hold-ms', `${duration}ms`);
  if (!btn.title) btn.title = shortHint;

  let timer = null;
  let nearTimer = null;
  let startedAt = 0;
  let triggered = false;
  let busy = false;

  const clearTimers = () => {
    clearTimeout(timer);
    clearTimeout(nearTimer);
    timer = null;
    nearTimer = null;
    btn.classList.remove('is-holding', 'is-near');
  };

  const startHold = () => {
    if (busy || btn.disabled || timer) return;
    triggered = false;
    startedAt = performance.now();
    btn.classList.add('is-holding');
    nearTimer = setTimeout(() => {
      if (timer) btn.classList.add('is-near');
    }, Math.round(duration * 0.68));
    timer = setTimeout(async () => {
      triggered = true;
      clearTimers();
      busy = true;
      playDone();
      try {
        await action();
      } finally {
        busy = false;
      }
    }, duration);
  };

  const cancelHold = () => {
    if (!timer) return;
    const elapsed = performance.now() - startedAt;
    clearTimers();
    if (!triggered && elapsed >= 20 && performance.now() - lastShortHintAt > 1800) {
      lastShortHintAt = performance.now();
      playWarn();
      showToast(shortHint);
    }
  };

  btn.addEventListener('pointerdown', (ev) => {
    if (ev.button && ev.button !== 0) return;
    startHold();
  });
  btn.addEventListener('pointerup', cancelHold);
  btn.addEventListener('pointerleave', cancelHold);
  btn.addEventListener('pointercancel', cancelHold);
  btn.addEventListener('keydown', (ev) => {
    if ((ev.key === 'Enter' || ev.key === ' ') && !ev.repeat) {
      ev.preventDefault();
      startHold();
    }
  });
  btn.addEventListener('keyup', (ev) => {
    if (ev.key === 'Enter' || ev.key === ' ') cancelHold();
  });
  btn.addEventListener('click', (ev) => {
    ev.preventDefault();
  });
}

export function initRailGlideTip(railEl) {
  if (!railEl) return;
  let tip = railEl.querySelector('.glide-tip');
  if (!tip) {
    tip = document.createElement('div');
    tip.className = 'glide-tip';
    tip.setAttribute('aria-hidden', 'true');
    railEl.appendChild(tip);
  }
  const btns = Array.from(railEl.querySelectorAll('.rail-btn'));
  btns.forEach((btn) => {
    if (btn.title && !btn.dataset.tip) {
      btn.dataset.tip = btn.title;
      btn.removeAttribute('title');
    }
    const show = () => {
      if (window.innerWidth <= 900) return;
      if (btn.id === 'rail-more' && !document.getElementById('rail-menu')?.classList.contains('hide')) {
        tip.classList.remove('is-visible');
        return;
      }
      const text = btn.dataset.tip;
      if (!text) return;
      const rect = btn.getBoundingClientRect();
      tip.textContent = text;
      railEl.style.setProperty('--tip-y', `${Math.round(rect.top + rect.height / 2)}px`);
      railEl.style.setProperty('--tip-x', `${Math.round(rect.right + 12)}px`);
      tip.classList.add('is-visible');
    };
    btn.addEventListener('pointerenter', show);
    btn.addEventListener('focus', show);
    btn.addEventListener('blur', () => tip.classList.remove('is-visible'));
    btn.addEventListener('click', () => tip.classList.remove('is-visible'));
  });
  railEl.addEventListener('pointerleave', () => tip.classList.remove('is-visible'));
}

const THEME_STORAGE_KEY = 'agenct:theme';
const THEME_MODES = ['system', 'light', 'dark'];
const THEME_LABELS = {
  system: '跟随系统',
  light: '浅色',
  dark: '深色',
};

export function getThemeMode() {
  try {
    const raw = localStorage.getItem(THEME_STORAGE_KEY);
    return raw === 'light' || raw === 'dark' ? raw : 'system';
  } catch {
    return 'system';
  }
}

export function isEffectiveDark(mode = getThemeMode()) {
  if (mode === 'dark') return true;
  if (mode === 'light') return false;
  return Boolean(window.matchMedia?.('(prefers-color-scheme: dark)')?.matches);
}

export function themeModeLabel(mode = getThemeMode()) {
  return THEME_LABELS[mode] || THEME_LABELS.system;
}

export function applyThemeMode(mode) {
  const next = THEME_MODES.includes(mode) ? mode : 'system';
  const root = document.documentElement;
  if (next === 'system') delete root.dataset.theme;
  else root.dataset.theme = next;
  try {
    if (next === 'system') localStorage.removeItem(THEME_STORAGE_KEY);
    else localStorage.setItem(THEME_STORAGE_KEY, next);
  } catch {
    // ignore storage errors
  }
  return next;
}

function nextThemeMode(cur) {
  const systemDark = Boolean(window.matchMedia?.('(prefers-color-scheme: dark)')?.matches);
  if (cur === 'system') return systemDark ? 'light' : 'dark';
  if (cur === 'dark') return systemDark ? 'system' : 'light';
  return systemDark ? 'dark' : 'system';
}

export function cycleThemeMode(origin) {
  const prev = getThemeMode();
  const prevDark = isEffectiveDark(prev);
  const next = nextThemeMode(prev);
  const nextDark = isEffectiveDark(next);
  playToggle(nextDark);

  const reduced = Boolean(window.matchMedia?.('(prefers-reduced-motion: reduce)')?.matches);
  const startVT = typeof document.startViewTransition === 'function' ? document.startViewTransition.bind(document) : null;
  if (prevDark !== nextDark && startVT && !reduced) {
    let x = window.innerWidth / 2;
    let y = window.innerHeight / 2;
    if (origin && typeof origin.clientX === 'number' && typeof origin.clientY === 'number' && (origin.clientX || origin.clientY)) {
      x = origin.clientX;
      y = origin.clientY;
    } else if (origin?.getBoundingClientRect) {
      const r = origin.getBoundingClientRect();
      x = r.left + r.width / 2;
      y = r.top + r.height / 2;
    }
    const radius = Math.hypot(Math.max(x, window.innerWidth - x), Math.max(y, window.innerHeight - y));
    const root = document.documentElement;
    root.style.setProperty('--vt-x', `${Math.round(x)}px`);
    root.style.setProperty('--vt-y', `${Math.round(y)}px`);
    root.style.setProperty('--vt-r', `${Math.ceil(radius)}px`);
    root.classList.add('is-theme-transitioning');
    try {
      const transition = startVT(() => applyThemeMode(next));
      Promise.resolve(transition?.finished).finally(() => root.classList.remove('is-theme-transitioning'));
      return next;
    } catch {
      root.classList.remove('is-theme-transitioning');
    }
  }
  return applyThemeMode(next);
}

export function fmtBytes(bytes) {
  const n = Math.max(0, Number(bytes) || 0);
  if (n < 1024) return `${Math.round(n)} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(n < 10240 ? 1 : 0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

let ctxCleanup = null;

export function closeContextMenu() {
  const menu = document.getElementById('ctx-menu');
  if (menu) {
    menu.classList.add('hide');
    menu.innerHTML = '';
  }
  if (ctxCleanup) {
    ctxCleanup();
    ctxCleanup = null;
  }
}

export function openContextMenu({ x = null, y = null, anchorEl = null, items = [] } = {}) {
  const menu = document.getElementById('ctx-menu');
  if (!menu) return;
  closeContextMenu();
  const validItems = (Array.isArray(items) ? items : []).filter((it) => it && (it.separator || it.label));
  if (!validItems.length) return;

  const runners = [];
  menu.innerHTML = validItems
    .map((it) => {
      if (it.separator) return '<hr class="ctx-sep">';
      const idx = runners.length;
      runners.push(it);
      const icHtml = it.icon
        ? `<svg class="ic" viewBox="0 0 24 24" aria-hidden="true"><use href="${esc(it.icon)}"/></svg>`
        : '';
      const kbdHtml = it.shortcut ? `<kbd class="ctx-kbd">${esc(it.shortcut)}</kbd>` : '';
      return `<button type="button" class="ctx-item${it.danger ? ' danger' : ''}" data-ctx-idx="${idx}" role="menuitem"${it.disabled ? ' disabled' : ''}>${icHtml}<span class="ctx-item-label">${esc(it.label)}</span>${kbdHtml}</button>`;
    })
    .join('');

  menu.classList.remove('hide');
  const rect = menu.getBoundingClientRect();
  let rawX = x;
  let rawY = y;
  if ((rawX === null || rawY === null) && anchorEl?.getBoundingClientRect) {
    const ar = anchorEl.getBoundingClientRect();
    rawX = ar.right - rect.width;
    rawY = ar.bottom + 6;
  }
  const left = Math.max(8, Math.min(Number(rawX) || 12, window.innerWidth - rect.width - 8));
  const top = Math.max(8, Math.min(Number(rawY) || 12, window.innerHeight - rect.height - 8));
  menu.style.setProperty('--ctx-x', `${Math.round(left)}px`);
  menu.style.setProperty('--ctx-y', `${Math.round(top)}px`);

  let selectedIdx = -1;
  const syncFocus = () => {
    const btns = Array.from(menu.querySelectorAll('.ctx-item:not(:disabled)'));
    btns.forEach((b, i) => b.classList.toggle('is-active', i === selectedIdx));
    if (selectedIdx >= 0 && btns[selectedIdx]) btns[selectedIdx].focus();
  };

  const onClick = (ev) => {
    const btn = ev.target?.closest?.('.ctx-item');
    if (!btn || btn.disabled) return;
    ev.stopPropagation();
    const item = runners[Number(btn.dataset.ctxIdx)];
    closeContextMenu();
    playTap();
    item?.onSelect?.();
  };
  const onPointerDown = (ev) => {
    if (!menu.contains(ev.target)) closeContextMenu();
  };
  const onKeyDown = (ev) => {
    const btns = Array.from(menu.querySelectorAll('.ctx-item:not(:disabled)'));
    if (ev.key === 'Escape') {
      ev.preventDefault();
      ev.stopPropagation();
      closeContextMenu();
    } else if (ev.key === 'ArrowDown' && btns.length) {
      ev.preventDefault();
      selectedIdx = (selectedIdx + 1) % btns.length;
      syncFocus();
    } else if (ev.key === 'ArrowUp' && btns.length) {
      ev.preventDefault();
      selectedIdx = (selectedIdx - 1 + btns.length) % btns.length;
      syncFocus();
    } else if (ev.key === 'Enter' && selectedIdx >= 0 && btns[selectedIdx]) {
      ev.preventDefault();
      btns[selectedIdx].click();
    }
  };
  const onDismiss = () => closeContextMenu();
  let scrollArmed = false;
  requestAnimationFrame(() => {
    scrollArmed = true;
  });
  const onScroll = (ev) => {
    if (!scrollArmed) return;
    if (anchorEl && ev.target && ev.target !== document && ev.target !== window && !ev.target.contains?.(anchorEl)) {
      return;
    }
    closeContextMenu();
  };

  menu.addEventListener('click', onClick);
  document.addEventListener('pointerdown', onPointerDown, true);
  document.addEventListener('keydown', onKeyDown, true);
  window.addEventListener('resize', onDismiss);
  window.addEventListener('scroll', onScroll, true);

  ctxCleanup = () => {
    menu.removeEventListener('click', onClick);
    document.removeEventListener('pointerdown', onPointerDown, true);
    document.removeEventListener('keydown', onKeyDown, true);
    window.removeEventListener('resize', onDismiss);
    window.removeEventListener('scroll', onScroll, true);
  };
}
