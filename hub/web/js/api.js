// CSRF-aware API client + safe Markdown renderer.
let csrfToken = '';

export function setCsrfToken(tok) {
  csrfToken = tok || '';
}

export async function api(path, { method = 'GET', body } = {}) {
  const headers = {};
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  if (method !== 'GET' && csrfToken) headers['X-CSRF-Token'] = csrfToken;

  const resp = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });

  const text = await resp.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = { error: text || `HTTP ${resp.status}` };
  }
  if (!resp.ok) {
    const err = new Error(data?.error || `HTTP ${resp.status}`);
    err.status = resp.status;
    throw err;
  }
  return data;
}

export function esc(str) {
  return String(str ?? '')
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

function renderInline(raw) {
  const codeSpans = [];
  let text = esc(raw || '').replace(/`([^`\n]+)`/g, (_, code) => {
    const idx = codeSpans.length;
    codeSpans.push(`<code>${code}</code>`);
    return `\u0000C${idx}\u0000`;
  });
  // Safe same-origin attachment images & links only
  text = text.replace(
    /!\[([^\]]*)\]\((\/api\/attachments\/att_[0-9a-f]{32})\)/g,
    (_, alt, url) => `<img src="${url}" alt="${alt}" loading="lazy">`,
  );
  text = text.replace(
    /\[([^\]]+)\]\((\/api\/attachments\/att_[0-9a-f]{32})\)/g,
    (_, label, url) => `<a href="${url}" target="_blank" rel="noopener">${label}</a>`,
  );
  text = text.replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>');
  text = text.replace(/(^|[^*\w])\*([^*\n]+)\*(?!\w)/g, '$1<em>$2</em>');
  return text.replace(/\u0000C(\d+)\u0000/g, (_, i) => codeSpans[Number(i)]);
}

function splitTableRow(line) {
  let trimmed = line.trim();
  if (trimmed.startsWith('|')) trimmed = trimmed.slice(1);
  if (trimmed.endsWith('|')) trimmed = trimmed.slice(0, -1);
  return trimmed.split('|').map((cell) => cell.trim());
}

function isTableSep(line) {
  return /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)+\|?\s*$/.test(line);
}

export function renderMarkdown(raw) {
  const normalized = String(raw || '').replace(/\r\n/g, '\n').trim();
  if (!normalized) return '';
  const lines = normalized.split('\n');
  const out = [];
  let i = 0;

  // Render leading Hub source tag like "**[节点 homelab · 标题]**" as a crisp pill badge.
  const tagMatch = lines[0].match(/^\*\*\[((?:节点|定时汇报|协作交付|来自)[^\]]+)\]\*\*$/);
  if (tagMatch) {
    out.push(`<div class="msg-source-pill">${esc(tagMatch[1])}</div>`);
    i = 1;
    while (i < lines.length && !lines[i].trim()) i++;
  }

  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      i++;
      continue;
    }

    // Fenced code block
    if (/^\s*```/.test(line)) {
      const codeLines = [];
      i++;
      while (i < lines.length && !/^\s*```\s*$/.test(lines[i])) {
        codeLines.push(lines[i]);
        i++;
      }
      if (i < lines.length) i++;
      out.push(`<pre class="md-pre"><code>${esc(codeLines.join('\n'))}</code></pre>`);
      continue;
    }

    // Markdown pipe table
    if (line.includes('|') && i + 1 < lines.length && isTableSep(lines[i + 1])) {
      const headers = splitTableRow(line);
      i += 2;
      const bodyRows = [];
      while (i < lines.length && lines[i].trim() && lines[i].includes('|')) {
        bodyRows.push(splitTableRow(lines[i]));
        i++;
      }
      const th = headers.map((c) => `<th>${renderInline(c)}</th>`).join('');
      const tb = bodyRows
        .map((row) => {
          const cells = headers.map((_, idx) => `<td>${renderInline(row[idx] ?? '')}</td>`).join('');
          return `<tr>${cells}</tr>`;
        })
        .join('');
      out.push(
        `<div class="md-table-wrap"><table class="md-table"><thead><tr>${th}</tr></thead><tbody>${tb}</tbody></table></div>`,
      );
      continue;
    }

    // Headings
    const hMatch = line.match(/^(#{1,4})\s+(.+)$/);
    if (hMatch) {
      const lvl = hMatch[1].length;
      out.push(`<div class="md-h md-h${lvl}">${renderInline(hMatch[2].trim())}</div>`);
      i++;
      continue;
    }

    // Horizontal rule
    if (/^\s*([-*_])\1{2,}\s*$/.test(line)) {
      out.push('<hr class="md-hr">');
      i++;
      continue;
    }

    // Blockquote
    if (/^\s*>\s?/.test(line)) {
      const qLines = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
        qLines.push(lines[i].replace(/^\s*>\s?/, ''));
        i++;
      }
      out.push(`<blockquote class="md-quote">${qLines.map(renderInline).join('<br>')}</blockquote>`);
      continue;
    }

    // Unordered list
    if (/^\s*[-*•]\s+/.test(line)) {
      const items = [];
      while (i < lines.length && /^\s*[-*•]\s+/.test(lines[i])) {
        items.push(`<li>${renderInline(lines[i].replace(/^\s*[-*•]\s+/, ''))}</li>`);
        i++;
      }
      out.push(`<ul class="md-list">${items.join('')}</ul>`);
      continue;
    }

    // Ordered list
    if (/^\s*\d+[.)]\s+/.test(line)) {
      const items = [];
      while (i < lines.length && /^\s*\d+[.)]\s+/.test(lines[i])) {
        items.push(`<li>${renderInline(lines[i].replace(/^\s*\d+[.)]\s+/, ''))}</li>`);
        i++;
      }
      out.push(`<ol class="md-list">${items.join('')}</ol>`);
      continue;
    }

    // Paragraph (stop before any block starter)
    const pLines = [];
    while (
      i < lines.length &&
      lines[i].trim() &&
      !/^\s*```/.test(lines[i]) &&
      !/^#{1,4}\s+/.test(lines[i]) &&
      !/^\s*([-*_])\1{2,}\s*$/.test(lines[i]) &&
      !/^\s*>\s?/.test(lines[i]) &&
      !/^\s*[-*•]\s+/.test(lines[i]) &&
      !/^\s*\d+[.)]\s+/.test(lines[i]) &&
      !(lines[i].includes('|') && i + 1 < lines.length && isTableSep(lines[i + 1]))
    ) {
      pLines.push(renderInline(lines[i]));
      i++;
    }
    if (pLines.length) {
      out.push(`<p class="md-p">${pLines.join('<br>')}</p>`);
    }
  }

  return `<div class="md-body">${out.join('')}</div>`;
}

export function fmtTime(unix) {
  if (!unix) return '';
  return new Date(unix * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

export function showToast(msg) {
  const el = document.getElementById('toast');
  if (!el) return;
  el.textContent = msg;
  el.classList.remove('hide');
  clearTimeout(showToast._t);
  showToast._t = setTimeout(() => el.classList.add('hide'), 3200);
}

export const AVATAR_CATALOG = [
  { id: 'agenct', name: '默认图标', desc: '极简矢量', src: 'brand/agenct-icon.svg', animated: false },
  { id: 'buddy', name: '动态管家', desc: '状态联动', src: '', animated: true, liveSvg: true },
  { id: 'sakura', name: '樱粉巫女', desc: '动态微动', src: 'avatars/sakura.webp?v=3', animated: true },
  { id: 'violet', name: '冷紫和风', desc: '动态微动', src: 'avatars/violet.webp?v=3', animated: true },
  { id: 'cheshire', name: '猫耳女仆', desc: '动态微动', src: 'avatars/cheshire.webp?v=3', animated: true },
];

export function avatarById(id) {
  return AVATAR_CATALOG.find((a) => a.id === id) || AVATAR_CATALOG[0];
}
