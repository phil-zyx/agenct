// Chat stream, typing indicator, attachment uploads, and floating pill composer.
import { api, esc, fmtTime, renderMarkdown, showToast } from './api.js';

function randomId() {
  if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
  const b = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

export function initChat({ state, onRefresh, onActivityChange }) {
  const scrollerEl = document.getElementById('scroller');
  const listEl = document.getElementById('messages');
  const formEl = document.getElementById('chat-form');
  const inputEl = document.getElementById('chat-input');
  const sendBtn = document.getElementById('send-btn');
  const fileEl = document.getElementById('file-input');
  const attsEl = document.getElementById('pending-atts');
  const stageEl = document.getElementById('stage-bar');
  const workStageEl = document.getElementById('work-stage');
  const elapsedEl = document.getElementById('work-elapsed');
  const workLogEl = document.getElementById('work-log');
  const logTailEl = document.getElementById('work-log-tail');
  const headEl = document.getElementById('muse-head-toggle');
  const heroEl = document.getElementById('hero-avatar-wrap');
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
  let brainWork = null;
  let taskWork = new Map();
  let visibleTaskId = null;
  let visibleWork = null;
  let elapsedTimer = null;
  let lastActivityKey = '';
  let lastTaskSnapshot = null;

  const stageLabels = new Map([
    ['connecting', '正在连接'],
    ['executing', '正在执行'],
    ['summarizing', '正在整理结果'],
    ['queued', '等待节点接单'],
    ['claimed by node', '节点已接单'],
    ['starting execution', '正在启动执行'],
  ]);

  function emitActivity() {
    if (!onActivityChange) return;
    const payload = {
      brainStage: typeof brainWork?.stage === 'string' ? brainWork.stage : '',
      streaming: Boolean(streamingText),
      taskStatus: typeof lastTaskSnapshot?.status === 'string' ? lastTaskSnapshot.status : '',
      taskStage: typeof lastTaskSnapshot?.stage === 'string' ? lastTaskSnapshot.stage : '',
    };
    const key = `${payload.brainStage}|${payload.streaming ? 1 : 0}|${payload.taskStatus}|${payload.taskStage}`;
    if (key === lastActivityKey) return;
    lastActivityKey = key;
    onActivityChange(payload);
  }

  function renderElapsed() {
    elapsedEl.textContent = visibleWork
      ? `${Math.floor((performance.now() - visibleWork.startedAt) / 1000)} 秒`
      : '';
  }

  function renderWork() {
    const tasks = [...taskWork.values()].map(({ work }) => work).filter(Boolean);
    const task = tasks.find((work) => work.status === 'running') || tasks[0];
    lastTaskSnapshot = task || null;
    visibleWork = brainWork || task;
    const busy = Boolean(visibleWork);
    const stage = typeof visibleWork?.stage === 'string' ? visibleWork.stage.trim() : '';
    const label = stageLabels.get(stage?.toLowerCase()) || stage
      || (task?.status === 'queued' ? '等待节点接单' : '正在执行');
    const text = busy ? `正在工作 · ${label}` : '';
    if (workStageEl.textContent !== text) workStageEl.textContent = text;
    stageEl.classList.toggle('hide', !busy);
    headEl?.classList.toggle('is-thinking', busy);
    heroEl?.classList.toggle('is-thinking', busy);

    const taskId = visibleWork?.taskId;
    const tail = visibleWork?.logTail || '';
    if (taskId !== visibleTaskId || !tail) workLogEl.open = false;
    visibleTaskId = taskId;
    if (logTailEl.textContent !== tail) logTailEl.textContent = tail;
    workLogEl.classList.toggle('hide', !tail);

    renderElapsed();
    if (busy && elapsedTimer === null) elapsedTimer = setInterval(renderElapsed, 1000);
    if (!busy && elapsedTimer !== null) {
      clearInterval(elapsedTimer);
      elapsedTimer = null;
    }
    emitActivity();
  }

  function onTask(event, version) {
    const { task_id: taskId, status, stage, elapsed_secs: elapsed, log_tail: logTail } = event;
    if (typeof taskId !== 'string' || typeof status !== 'string') return;
    const previous = taskWork.get(taskId)?.work;
    const now = performance.now();
    const phase = status === 'running' && stage === undefined && event.node_id ? 'queued' : status;
    const active = phase === 'running' || phase === 'queued';
    const startedAt = Number.isFinite(elapsed) && elapsed >= 0
      ? now - elapsed * 1000
      : previous?.status === phase && stage !== 'Claimed by node' ? previous.startedAt : now;
    taskWork.set(taskId, {
      version,
      work: active ? { taskId, status: phase, startedAt, stage, logTail } : null,
    });
    renderWork();
  }

  function syncTasks(tasks, version) {
    const next = new Map();
    for (const task of tasks) {
      if (task.status !== 'running') continue;
      const status = task.job_status || task.status;
      if (status !== 'running' && status !== 'queued') continue;
      const previous = taskWork.get(task.id)?.work;
      const sameRun = previous?.status === status
        && (previous.revision === undefined || previous.revision === task.revision);
      next.set(task.id, {
        version,
        work: {
          taskId: task.id, status, revision: task.revision,
          startedAt: sameRun ? previous.startedAt : performance.now(),
          stage: task.stage, logTail: status === 'running' ? task.log_tail : '',
        },
      });
    }
    // An event received during this request is newer than the snapshot may be.
    for (const [id, entry] of taskWork) {
      if (entry.version > version) next.set(id, entry);
    }
    taskWork = next;
    renderWork();
  }

  function clearStreaming() {
    if (streamRaf) {
      cancelAnimationFrame(streamRaf);
      streamRaf = 0;
    }
    const hadText = Boolean(streamingText);
    streamingText = '';
    if (streamWrapEl) streamWrapEl.classList.add('hide');
    if (streamMsgEl) streamMsgEl.innerHTML = '';
    if (hadText) emitActivity();
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
    const turnWork = { stage: 'connecting', startedAt: performance.now() };
    brainWork = turnWork;
    renderWork();
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
      if (brainWork === turnWork) setStage('');
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
      brainWork ??= { startedAt: performance.now() };
      brainWork.stage = stage;
    } else {
      brainWork = null;
    }
    renderWork();
  }

  function onDelta(delta) {
    if (!delta) return;
    if (delta.done || delta.reset) {
      clearStreaming();
      return;
    }
    if (typeof delta.text === 'string' && delta.text) {
      const wasEmpty = !streamingText;
      streamingText += delta.text;
      if (wasEmpty) emitActivity();
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

  return { render, setStage, onDelta, onTask, syncTasks, insertMention, setThreadFilter };
}
