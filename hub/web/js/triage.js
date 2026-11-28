import { esc } from './api.js';

export function stripNoise(raw) {
  return String(raw || '')
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '')
    .replace(/\u25af\[\d+(?:;\d+)*m/g, '')
    .replace(/\r\n/g, '\n')
    .trim();
}

function previewLines(raw) {
  return stripNoise(raw)
    .replace(/^[ \t]*```[^\n]*(?:\n[\s\S]*?^[ \t]*```[^\n]*|[\s\S]*$)/gm, '')
    .split('\n')
    .map((line) => line.trim().replace(/^(?:#+\s*|>\s*|[-*+]\s+|\d+[.)]\s+)+/, '').replace(/\*\*/g, '').replace(/`/g, '').trim())
    .filter((line) => line && !/^[-*_]{3,}$/.test(line));
}

export function cleanPreviewLine(raw, limit = 96) {
  const line = previewLines(raw)[0] || '';
  return line.length > limit ? `${line.slice(0, limit)}…` : line;
}

function externalLinks(task) {
  const links = new Set();
  for (const raw of [task.result, task.instruction]) {
    const matches = stripNoise(raw).matchAll(/https?:\/\/[^\s<>"'`()，。；！？、]+/gi);
    for (const [match] of matches) {
      try {
        const url = new URL(match.replace(/[.,;!?]+$/, ''));
        if (!url.username && !url.password) links.add(url.href);
      } catch {
        continue;
      }
      if (links.size === 3) return [...links];
    }
  }
  return [...links];
}

export function describeTask(task) {
  if (!task) return { preview: '', progress: null };
  const isLive = task.status === 'running' || task.status === 'pending';
  return {
    preview: cleanPreviewLine(task.blocked_reason || task.result || task.instruction),
    progress: isLive
      ? {
          stage: stripNoise(task.stage),
          elapsed: Number.isFinite(task.elapsed_secs) && task.elapsed_secs >= 0 ? task.elapsed_secs : null,
          logTail: stripNoise(task.log_tail).slice(0, 600),
          links: externalLinks(task),
        }
      : null,
  };
}

const STEP_TOKEN_RE = /`[^`\n]+`|(?:[A-Za-z0-9_.-]+\/)+[A-Za-z0-9_.:-]+|\b[A-Za-z0-9_-]+\.(?:rs|py|js|mjs|ts|tsx|json|toml|md|css|html|sh|yml|yaml)\b/g;

function highlightStepLine(line) {
  const raw = String(line || '');
  let out = '';
  let last = 0;
  for (const m of raw.matchAll(STEP_TOKEN_RE)) {
    const idx = m.index ?? 0;
    if (idx > last) out += esc(raw.slice(last, idx));
    out += `<span class="step-chip">${esc(m[0])}</span>`;
    last = idx + m[0].length;
  }
  if (last < raw.length) out += esc(raw.slice(last));
  return out;
}

export function renderStepTrailHtml(lines) {
  const clean = (Array.isArray(lines) ? lines : []).map((l) => String(l || '').trim()).filter(Boolean);
  if (!clean.length) return '';
  return clean
    .map((line, idx) => {
      const active = idx === clean.length - 1;
      return `<div class="step-row ${active ? 'is-active' : 'is-done'}"><span class="step-marker" aria-hidden="true"></span><span class="step-text">${highlightStepLine(line)}</span></div>`;
    })
    .join('\n');
}
