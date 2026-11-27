// Shared Bencho-inspired UI helpers: copy-field morph, hold-to-confirm, and liquid glide tooltips.
import { copyBtnHtml, showToast } from './api.js';
import { playDone, playTap, playWarn } from './sound.js';

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
