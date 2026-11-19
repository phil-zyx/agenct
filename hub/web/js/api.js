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

export function renderMarkdown(raw) {
  let text = esc(raw || '');
  // Fenced code blocks
  text = text.replace(/```[\w-]*\n([\s\S]*?)```/g, (_, code) => `<pre><code>${code}</code></pre>`);
  // Inline code
  text = text.replace(/`([^`\n]+)`/g, '<code>$1</code>');
  // Bold
  text = text.replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>');
  // Attachment images (same-origin /api/attachments/att_... only)
  text = text.replace(
    /!\[([^\]]*)\]\((\/api\/attachments\/att_[0-9a-f]{32})\)/g,
    (_, alt, url) => `<img src="${url}" alt="${alt}" loading="lazy">`,
  );
  // Attachment links
  text = text.replace(
    /\[([^\]]+)\]\((\/api\/attachments\/att_[0-9a-f]{32})\)/g,
    (_, label, url) => `<a href="${url}" target="_blank" rel="noopener">${label}</a>`,
  );
  // Line breaks outside <pre>
  return text.replace(/\n/g, '<br>');
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
  { id: 'sakura', name: '樱粉巫女', desc: '动态微动', src: 'avatars/sakura.webp?v=2', animated: true },
  { id: 'violet', name: '冷紫和风', desc: '动态微动', src: 'avatars/violet.webp?v=2', animated: true },
  { id: 'cheshire', name: '猫耳女仆', desc: '动态微动', src: 'avatars/cheshire.webp?v=2', animated: true },
];

export function avatarById(id) {
  return AVATAR_CATALOG.find((a) => a.id === id) || AVATAR_CATALOG[0];
}
