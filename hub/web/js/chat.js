// Chat stream, typing indicator, attachment uploads, and floating pill composer.
import { api, esc, fmtTime, renderMarkdown, showToast } from './api.js';

function randomId() {
  if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
  const b = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

export function initChat({ state, onRefresh }) {
  const scrollerEl = document.getElementById('scroller');
  const listEl = document.getElementById('messages');
  const formEl = document.getElementById('chat-form');
  const inputEl = document.getElementById('chat-input');
  const sendBtn = document.getElementById('send-btn');
  const fileEl = document.getElementById('file-input');
  const attsEl = document.getElementById('pending-atts');
  const stageEl = document.getElementById('stage-bar');
  const typingEl = document.getElementById('typing');
  const streamWrapEl = document.getElementById('stream-live');
  const streamMsgEl = document.getElementById('streaming-msg');
  const loadOlderWrap = document.getElementById('load-older-wrap');
  const loadOlderBtn = document.getElementById('load-older-btn');
  const threadFilterBar = document.getElementById('thread-filter-bar');
  const threadFilterTitle = document.getElementById('thread-filter-title');
  const clearThreadFilterBtn = document.getElementById('clear-thread-filter');

  let pendingAtts = [];
  let lastFailed = null;
  let sending = false;
  let optimisticMsg = null;
  let optimisticBaseLastId = 0;
  let streamingText = '';
  let streamRaf = 0;
  let olderMessages = [];
  let hasMoreGlobal = false;
  let activeThread = null;
  let threadMessages = [];
  let threadHasMore = false;
  let loadingOlder = false;
  let scrollAfterRender = true;

  function clearStreaming() {
    if (streamRaf) {
      cancelAnimationFrame(streamRaf);
      streamRaf = 0;
    }
    streamingText = '';
    if (streamWrapEl) streamWrapEl.classList.add('hide');
    if (streamMsgEl) streamMsgEl.innerHTML = '';
  }

  function flushStreamRender() {
    streamRaf = 0;
    if (!streamWrapEl || !streamMsgEl) return;
    if (!streamingText) {
      streamWrapEl.classList.add('hide');
      streamMsgEl.innerHTML = '';
      return;
    }
    typingEl.classList.add('hide');
    const nearBottom =
      scrollerEl.scrollHeight - scrollerEl.scrollTop - scrollerEl.clientHeight < 120;
    streamMsgEl.innerHTML = renderMarkdown(streamingText);
    streamWrapEl.classList.remove('hide');
    if (nearBottom) {
      scrollerEl.scrollTop = scrollerEl.scrollHeight;
    }
  }

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
          `<span class="chat-attachment-chip">${esc(a.name)} <button type="button" class="text-btn sm" data-rm="${i}">×</button></span>`,
      )
      .join('');
    attsEl.querySelectorAll('[data-rm]').forEach((btn) => {
      btn.onclick = () => {
        pendingAtts.splice(Number(btn.dataset.rm), 1);
        renderPendingAtts();
      };
    });
  }

  async function uploadFile(file, name = file.name) {
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
          name,
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
  }

  fileEl.onchange = async () => {
    const file = fileEl.files?.[0];
    fileEl.value = '';
    if (file) await uploadFile(file);
  };

  // Paste screenshots / copied images straight into the composer. Plain-text paste is untouched.
  const PASTE_IMAGE_TYPES = { 'image/png': 'png', 'image/jpeg': 'jpg', 'image/gif': 'gif', 'image/webp': 'webp' };
  inputEl.addEventListener('paste', async (ev) => {
    const images = Array.from(ev.clipboardData?.items || [])
      .filter((it) => it.kind === 'file' && PASTE_IMAGE_TYPES[it.type])
      .map((it) => it.getAsFile())
      .filter(Boolean);
    if (!images.length) return;
    ev.preventDefault();
    const stamp = new Date().toISOString().replace(/[-:T]/g, '').slice(0, 14);
    for (const [i, file] of images.entries()) {
      const suffix = images.length > 1 ? `-${i + 1}` : '';
      await uploadFile(file, `paste-${stamp}${suffix}.${PASTE_IMAGE_TYPES[file.type]}`);
    }
  });

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
    // Retrying the exact same text after a failure reuses its request_id, so the server
    // revives the failed message instead of storing a duplicate.
    const requestId =
      lastFailed && lastFailed.text === text ? lastFailed.requestId : `req-${randomId()}`;
    optimisticBaseLastId = (state.messages || []).reduce((max, m) => Math.max(max, m.id || 0), 0);
    optimisticMsg = {
      role: 'user',
      content: text,
      request_id: requestId,
      created_at: Math.floor(Date.now() / 1000),
    };
    inputEl.value = '';
    pendingAtts = [];
    renderPendingAtts();
    clearStreaming();
    render();
    typingEl.classList.remove('hide');
    document.getElementById('muse-head-toggle')?.classList.add('is-thinking');
    document.getElementById('hero-avatar-wrap')?.classList.add('is-thinking');
    scrollerEl.scrollTop = scrollerEl.scrollHeight;

    try {
      await api('/api/chat', {
        method: 'POST',
        body: { message: text, request_id: requestId },
      });
      lastFailed = null;
      optimisticMsg = null;
      clearStreaming();
      await onRefresh();
    } catch (err) {
      lastFailed = { text, requestId };
      optimisticMsg = null;
      clearStreaming();
      await onRefresh().catch(() => render());
      inputEl.value = text;
      showToast(`发送失败：${err.message}`);
    } finally {
      sending = false;
      sendBtn.disabled = false;
      typingEl.classList.add('hide');
      const hasRunning = (state.tasks || []).some((t) => t.status === 'running');
      if (!hasRunning) {
        document.getElementById('muse-head-toggle')?.classList.remove('is-thinking');
        document.getElementById('hero-avatar-wrap')?.classList.remove('is-thinking');
      }
      inputEl.focus();
    }
  };

  function mergeById(older, newer) {
    if (!older.length) return newer;
    const seen = new Set(newer.map((m) => m.id));
    const head = older.filter((m) => m.id && !seen.has(m.id));
    return [...head, ...newer];
  }

  function currentMessages() {
    if (activeThread) {
      const liveThreadMsgs = (state.messages || []).filter((m) => m.thread_id === activeThread.id);
      return mergeById(threadMessages, liveThreadMsgs);
    }
    return mergeById(olderMessages, state.messages || []);
  }

  if (clearThreadFilterBtn) {
    clearThreadFilterBtn.onclick = () => {
      activeThread = null;
      threadMessages = [];
      threadHasMore = false;
      scrollAfterRender = true;
      render();
    };
  }

  if (loadOlderBtn) {
    loadOlderBtn.onclick = async () => {
      if (loadingOlder) return;
      const list = currentMessages();
      const oldestId = list[0]?.id;
      if (!oldestId) return;
      loadingOlder = true;
      loadOlderBtn.disabled = true;
      const prevHeight = scrollerEl.scrollHeight;
      const prevTop = scrollerEl.scrollTop;
      try {
        const params = new URLSearchParams({ before_id: String(oldestId), limit: '40' });
        if (activeThread) params.set('thread_id', activeThread.id);
        const res = await api(`/api/messages?${params.toString()}`);
        const older = res?.messages || [];
        if (activeThread) {
          threadMessages = mergeById(older, threadMessages);
          threadHasMore = Boolean(res?.has_more);
        } else {
          olderMessages = mergeById(older, olderMessages);
          hasMoreGlobal = Boolean(res?.has_more);
        }
        scrollAfterRender = false;
        render();
        scrollerEl.scrollTop = prevTop + (scrollerEl.scrollHeight - prevHeight);
      } catch (err) {
        showToast(`加载历史消息失败：${err.message}`);
      } finally {
        loadingOlder = false;
        loadOlderBtn.disabled = false;
      }
    };
  }

  function render() {
    if (!activeThread && !olderMessages.length) {
      hasMoreGlobal = Boolean(state.has_more_messages);
    }
    if (threadFilterBar && threadFilterTitle) {
      threadFilterBar.classList.toggle('hide', !activeThread);
      threadFilterTitle.textContent = activeThread ? `正在查看线程：${activeThread.title}` : '';
    }
    listEl.dataset.filtered = activeThread ? 'true' : 'false';

    const showOlder = activeThread ? threadHasMore : hasMoreGlobal;
    if (loadOlderWrap) {
      loadOlderWrap.classList.toggle('hide', !showOlder);
    }

    const baseMsgs = currentMessages();
    let msgs = baseMsgs;
    if (optimisticMsg && !activeThread) {
      const synced = baseMsgs.some(
        (m) =>
          m.role === 'user' &&
          ((m.request_id && m.request_id === optimisticMsg.request_id) ||
            ((m.id || 0) > optimisticBaseLastId && m.content === optimisticMsg.content)),
      );
      if (!synced) {
        msgs = [...baseMsgs, optimisticMsg];
      }
    }
    if (!msgs.length) {
      listEl.innerHTML = '';
      scrollAfterRender = true;
      return;
    }
    listEl.innerHTML = msgs
      .map(
        (m) => `
      <div class="msg msg-${esc(m.role)}${m.status === 'failed' ? ' msg-failed' : ''}">
        <div>${renderMarkdown(m.content)}</div>
        <div class="msg-meta">${esc(fmtTime(m.created_at))}${
          m.status === 'failed' ? ' · 未送达，可重新发送' : ''
        }</div>
      </div>`,
      )
      .join('');
    if (scrollAfterRender) {
      scrollerEl.scrollTop = scrollerEl.scrollHeight;
    }
    scrollAfterRender = true;
  }

  async function setThreadFilter(threadId, title) {
    if (!threadId) return;
    activeThread = { id: threadId, title: title || threadId };
    threadMessages = [];
    threadHasMore = false;
    try {
      const params = new URLSearchParams({ thread_id: threadId, limit: '40' });
      const res = await api(`/api/messages?${params.toString()}`);
      threadMessages = res?.messages || [];
      threadHasMore = Boolean(res?.has_more);
      scrollAfterRender = true;
      render();
    } catch (err) {
      showToast(`加载线程记录失败：${err.message}`);
    }
  }

  function setStage(stage) {
    if (stage) {
      stageEl.textContent = stage;
      stageEl.classList.remove('hide');
      scrollerEl.scrollTop = scrollerEl.scrollHeight;
    } else {
      stageEl.textContent = '';
      stageEl.classList.add('hide');
    }
  }

  function onDelta(delta) {
    if (!delta) return;
    if (delta.done || delta.reset) {
      clearStreaming();
      return;
    }
    if (typeof delta.text === 'string' && delta.text) {
      streamingText += delta.text;
      if (!streamRaf) {
        streamRaf = requestAnimationFrame(flushStreamRender);
      }
    }
  }

  function insertMention(username) {
    const prefix = `@${username} `;
    if (!inputEl.value.startsWith(prefix)) {
      inputEl.value = prefix + inputEl.value;
    }
    inputEl.focus();
  }

  return { render, setStage, onDelta, insertMention, setThreadFilter };
}
