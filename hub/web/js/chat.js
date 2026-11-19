// Chat message rendering, attachment upload, and single-brain message submission.
import { api, esc, fmtTime, renderMarkdown, showToast } from './api.js';

function randomId() {
  if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
  const b = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

export function initChat({ state, onRefresh }) {
  const listEl = document.getElementById('messages');
  const formEl = document.getElementById('chat-form');
  const inputEl = document.getElementById('chat-input');
  const sendBtn = document.getElementById('send-btn');
  const fileEl = document.getElementById('file-input');
  const attsEl = document.getElementById('pending-atts');
  const stageEl = document.getElementById('stage-bar');

  let pendingAtts = [];
  let sending = false;

  function renderPendingAtts() {
    if (!pendingAtts.length) {
      attsEl.classList.add('hide');
      attsEl.innerHTML = '';
      return;
    }
    attsEl.classList.remove('hide');
    attsEl.innerHTML = pendingAtts
      .map(
        (a, i) =>
          `<span class="chip">${esc(a.name)} <button type="button" class="btn ghost sm" data-rm="${i}">×</button></span>`,
      )
      .join('');
    attsEl.querySelectorAll('[data-rm]').forEach((btn) => {
      btn.onclick = () => {
        pendingAtts.splice(Number(btn.dataset.rm), 1);
        renderPendingAtts();
      };
    });
  }

  fileEl.onchange = async () => {
    const file = fileEl.files?.[0];
    fileEl.value = '';
    if (!file) return;
    if (file.size > 5 * 1024 * 1024) {
      showToast('附件不能超过 5 MiB');
      return;
    }
    try {
      const buf = await file.arrayBuffer();
      let binary = '';
      const bytes = new Uint8Array(buf);
      for (let i = 0; i < bytes.length; i += 8192) {
        binary += String.fromCharCode(...bytes.subarray(i, i + 8192));
      }
      const meta = await api('/api/attachments', {
        method: 'POST',
        body: {
          name: file.name,
          mime_type: file.type || 'text/plain',
          data_base64: btoa(binary),
        },
      });
      pendingAtts.push(meta);
      renderPendingAtts();
      showToast(`已上传：${meta.name}`);
    } catch (err) {
      showToast(`上传失败：${err.message}`);
    }
  };

  inputEl.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter' && !ev.shiftKey && !ev.isComposing) {
      ev.preventDefault();
      formEl.requestSubmit();
    }
  });

  formEl.onsubmit = async (ev) => {
    ev.preventDefault();
    if (sending) return;
    let text = inputEl.value.trim();
    if (!text && !pendingAtts.length) return;
    if (pendingAtts.length) {
      const refs = pendingAtts
        .map((a) =>
          a.mime_type.startsWith('image/')
            ? `![${a.name}](/api/attachments/${a.id})`
            : `[${a.name}](/api/attachments/${a.id})`,
        )
        .join('\n');
      text = text ? `${text}\n\n${refs}` : refs;
    }
    sending = true;
    sendBtn.disabled = true;
    inputEl.value = '';
    pendingAtts = [];
    renderPendingAtts();

    try {
      await api('/api/chat', {
        method: 'POST',
        body: { message: text, request_id: `req-${randomId()}` },
      });
      await onRefresh();
    } catch (err) {
      inputEl.value = text;
      showToast(`发送失败：${err.message}`);
    } finally {
      sending = false;
      sendBtn.disabled = false;
      inputEl.focus();
    }
  };

  function render() {
    const msgs = state.messages || [];
    if (!msgs.length) {
      listEl.innerHTML =
        '<div class="msg system">你好，我是你的个人超级智能体。可以直接聊天、管理台账与定时任务、<code>@同事</code> 协作，或调度 PC / 云端执行节点。</div>';
      return;
    }
    listEl.innerHTML = msgs
      .map(
        (m) => `
      <article class="msg ${esc(m.role)}">
        <div>${renderMarkdown(m.content)}</div>
        <div class="msg-meta">${esc(fmtTime(m.created_at))}</div>
      </article>`,
      )
      .join('');
    listEl.scrollTop = listEl.scrollHeight;
  }

  function setStage(stage) {
    if (stage) {
      stageEl.textContent = stage;
      stageEl.classList.remove('hide');
    } else {
      stageEl.textContent = '';
      stageEl.classList.add('hide');
    }
  }

  return { render, setStage };
}
