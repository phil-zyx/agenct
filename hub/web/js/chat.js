// Chat stream, typing indicator, attachment uploads, and floating pill composer.
import { api, esc, fmtTime, renderMarkdown, showToast } from './api.js';
import { playDone, playSend, playTap, playToggle, playWarn } from './sound.js';
import { initDelegatedCopy, writeClipboard } from './ui.js';

function randomId() {
  if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
  const b = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

export function initChat({ state, onRefresh, onActivityChange }) {
  const scrollerEl = document.getElementById('scroller');
  const listEl = document.getElementById('messages');
  const formEl = document.getElementById('chat-form');
  const pillEl = formEl.querySelector('.composer-pill');
  const engineWrap = document.getElementById('composer-engine-wrap');
  const engineBtn = document.getElementById('composer-engine-btn');
  const engineLabel = document.getElementById('composer-engine-label');
  const engineMenu = document.getElementById('composer-engine-menu');
  const inputEl = document.getElementById('chat-input');
  const sendBtn = document.getElementById('send-btn');
  const fileEl = document.getElementById('file-input');
  const attsEl = document.getElementById('pending-atts');
  const stageEl = document.getElementById('stage-bar');
  const workStageEl = document.getElementById('work-stage');
  const elapsedEl = document.getElementById('work-elapsed');
  const workLogEl = document.getElementById('work-log');
  const logTailEl = document.getElementById('work-log-tail');
  const islandShellEl = document.getElementById('island-shell');
  const scrollBottomBtn = document.getElementById('scroll-bottom-btn');
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
  let submitting = false;
  let activeTurn = null;
  const turnEvents = new Map();
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
  let hasBootRendered = false;
  let lastSeenMsgId = 0;
  let switchingThread = false;
  let launchTimer = null;
  let brainWork = null;
  let taskWork = new Map();
  let visibleTaskId = null;
  let visibleWork = null;
  let elapsedTimer = null;
  let lastActivityKey = '';
  let lastTaskSnapshot = null;
  let currentStatusLabel = '';
  let donePulseTimer = null;
  const renderedContents = new Map();

  function distFromBottom() {
    return Math.max(0, scrollerEl.scrollHeight - scrollerEl.scrollTop - scrollerEl.clientHeight);
  }

  function syncScrollBottom() {
    if (!scrollBottomBtn) return;
    const dist = distFromBottom();
    if (dist <= 60) {
      scrollBottomBtn.classList.remove('is-visible', 'has-unread');
    } else if (dist > 160) {
      scrollBottomBtn.classList.add('is-visible');
    }
  }

  scrollerEl?.addEventListener('scroll', syncScrollBottom, { passive: true });
  scrollBottomBtn?.addEventListener('click', () => {
    playTap();
    scrollBottomBtn.classList.remove('has-unread');
    scrollerEl.scrollTo({ top: scrollerEl.scrollHeight, behavior: 'smooth' });
  });

  const stageLabels = new Map([
    ['connecting', '正在连接'],
    ['executing', '正在执行'],
    ['summarizing', '正在整理结果'],
    ['queued', '等待节点接单'],
    ['claimed by node', '节点已接单'],
    ['starting execution', '正在启动执行'],
  ]);

  function fmtDuration(secs) {
    const s = Math.max(0, Math.floor(secs));
    if (s < 60) return `${s} 秒`;
    if (s < 3600) return `${Math.floor(s / 60)} 分 ${s % 60} 秒`;
    return `${Math.floor(s / 3600)} 小时 ${Math.floor((s % 3600) / 60)} 分`;
  }

  function fmtCompactDuration(secs) {
    const s = Math.max(0, Math.floor(secs));
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.floor(s / 60)}m${String(s % 60).padStart(2, '0')}s`;
    return `${Math.floor(s / 3600)}h${Math.floor((s % 3600) / 60)}m`;
  }

  function humanizeStage(rawStage, status) {
    const st = String(rawStage || '').trim();
    const mapped = stageLabels.get(st.toLowerCase());
    if (mapped) return mapped;
    if (/^running\s+/i.test(st)) {
      const quiet = st.match(/（(已静默\s*[^）]+)）/);
      return quiet ? `正在执行 · ${quiet[1]}` : '正在执行';
    }
    return st || (status === 'queued' ? '等待节点接单' : '正在执行');
  }

  function emitActivity() {
    if (!onActivityChange) return;
    const payload = {
      brainStage: typeof brainWork?.stage === 'string' ? brainWork.stage : '',
      streaming: Boolean(streamingText),
      taskStatus: typeof lastTaskSnapshot?.status === 'string' ? lastTaskSnapshot.status : '',
      taskStage: typeof lastTaskSnapshot?.stage === 'string' ? lastTaskSnapshot.stage : '',
      statusLabel: currentStatusLabel,
    };
    const key = `${payload.brainStage}|${payload.streaming ? 1 : 0}|${payload.taskStatus}|${payload.taskStage}|${payload.statusLabel}`;
    if (key === lastActivityKey) return;
    lastActivityKey = key;
    onActivityChange(payload);
  }

  function renderElapsed() {
    const secs = visibleWork ? Math.floor((performance.now() - visibleWork.startedAt) / 1000) : 0;
    elapsedEl.textContent = visibleWork ? fmtDuration(secs) : '';
    if (visibleWork) {
      const stage = typeof visibleWork.stage === 'string' ? visibleWork.stage.trim() : '';
      const shortStage = humanizeStage(stage, lastTaskSnapshot?.status);
      const nextLabel = `${shortStage} · ${fmtCompactDuration(secs)}`;
      if (nextLabel !== currentStatusLabel) {
        currentStatusLabel = nextLabel;
        emitActivity();
      }
    } else if (currentStatusLabel) {
      currentStatusLabel = '';
      emitActivity();
    }
  }

  function renderWork() {
    const tasks = [...taskWork.values()].map(({ work }) => work).filter(Boolean);
    const task = tasks.find((work) => work.status === 'running') || tasks[0];
    lastTaskSnapshot = task || null;
    visibleWork = brainWork || task;
    const busy = Boolean(visibleWork);
    const stage = typeof visibleWork?.stage === 'string' ? visibleWork.stage.trim() : '';
    const label = humanizeStage(stage, task?.status);
    const text = busy ? `正在工作 · ${label}` : '';
    if (workStageEl.textContent !== text) workStageEl.textContent = text;
    stageEl.classList.toggle('hide', !busy);
    headEl?.classList.toggle('is-thinking', busy);
    heroEl?.classList.toggle('is-thinking', busy);

    const taskId = visibleWork?.taskId;
    const rawTail = String(visibleWork?.logTail || '').trim();
    const tailLines = [];
    if (rawTail) {
      const splitLines = rawTail.includes('\n') ? rawTail.split(/\r?\n/) : rawTail.split(/\s+\|\s+/);
      for (const rawLine of splitLines) {
        const line = rawLine.trim();
        if (line && tailLines[tailLines.length - 1] !== line) tailLines.push(line);
      }
    }
    const tail = tailLines.join('\n');
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

  function flashIslandDone() {
    if (!islandShellEl) return;
    islandShellEl.classList.remove('just-done');
    void islandShellEl.offsetWidth;
    islandShellEl.classList.add('just-done');
    clearTimeout(donePulseTimer);
    donePulseTimer = setTimeout(() => islandShellEl.classList.remove('just-done'), 1050);
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
    const nearBottom = distFromBottom() < 140;
    streamMsgEl.innerHTML = renderMarkdown(streamingText);
    streamWrapEl.classList.remove('hide');
    if (nearBottom) {
      scrollerEl.scrollTop = scrollerEl.scrollHeight;
    } else if (scrollBottomBtn) {
      scrollBottomBtn.classList.add('is-visible', 'has-unread');
    }
  }

  function syncComposerState() {
    if (!inputEl.value) {
      inputEl.style.height = '';
    } else {
      inputEl.style.height = 'auto';
      inputEl.style.height = `${Math.min(Math.max(inputEl.scrollHeight, 36), 180)}px`;
    }
    const active = inputEl.value.trim().length > 0 || pendingAtts.length > 0;
    pillEl?.classList.toggle('has-content', active);
  }

  function renderPendingAtts() {
    if (!pendingAtts.length) {
      attsEl.classList.add('hide');
      attsEl.innerHTML = '';
      syncComposerState();
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
    syncComposerState();
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

  inputEl.addEventListener('input', syncComposerState);

  if (pillEl) {
    pillEl.addEventListener('dragover', (ev) => {
      if (ev.dataTransfer?.types?.includes('Files')) {
        ev.preventDefault();
        pillEl.classList.add('is-dragover');
      }
    });
    pillEl.addEventListener('dragleave', (ev) => {
      if (!pillEl.contains(ev.relatedTarget)) pillEl.classList.remove('is-dragover');
    });
    pillEl.addEventListener('drop', async (ev) => {
      pillEl.classList.remove('is-dragover');
      const files = Array.from(ev.dataTransfer?.files || []);
      if (!files.length) return;
      ev.preventDefault();
      for (const file of files.slice(0, 4)) await uploadFile(file);
    });
  }

  inputEl.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter' && !ev.shiftKey && !ev.isComposing) {
      ev.preventDefault();
      formEl.requestSubmit();
    }
  });

  formEl.onsubmit = async (ev) => {
    ev.preventDefault();
    if (submitting || activeTurn) return;
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
    submitting = true;
    sendBtn.disabled = true;
    playSend();
    clearTimeout(launchTimer);
    sendBtn.classList.remove('is-launching');
    void sendBtn.offsetWidth;
    sendBtn.classList.add('is-launching');
    launchTimer = setTimeout(() => sendBtn.classList.remove('is-launching'), 300);
    // Retrying the exact same text after a failure reuses its request_id, so the server
    // revives the failed message instead of storing a duplicate.
    const requestId =
      lastFailed && lastFailed.text === text ? lastFailed.requestId : `req-${randomId()}`;
    optimisticBaseLastId = (state.messages || []).reduce((max, m) => Math.max(max, m.id || 0), 0);
    optimisticMsg = {
      role: 'user',
      content: text,
      thread_id: activeThread ? activeThread.id : null,
      request_id: requestId,
      status: 'pending',
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
      const chatBody = { message: text, request_id: requestId };
      if (activeThread) chatBody.thread_id = activeThread.id;
      const res = await api('/api/chat', {
        method: 'POST',
        body: chatBody,
      });
      submitting = false;
      if (res?.status === 'accepted') {
        activeTurn = { id: res.message_id || 0, requestId, text, work: turnWork };
        const syncedMsg = (state.messages || []).find(
          (m) =>
            m.role === 'user' &&
            ((res.message_id && m.id === res.message_id) ||
              (requestId && m.request_id === requestId)),
        );
        const terminalEv =
          (res.message_id && turnEvents.get(`id:${res.message_id}`)) ||
          (requestId && turnEvents.get(`req:${requestId}`));
        if (syncedMsg && syncedMsg.status !== 'pending') {
          syncTurnState();
        } else if (terminalEv) {
          await onRefresh().catch(() => syncTurnState());
        } else {
          syncTurnState();
        }
      } else {
        lastFailed = null;
        activeTurn = null;
        optimisticMsg = null;
        clearStreaming();
        sendBtn.disabled = false;
        typingEl.classList.add('hide');
        if (brainWork === turnWork) setStage('');
        await onRefresh();
        if (!document.activeElement || document.activeElement === document.body) {
          inputEl.focus();
        }
      }
    } catch (err) {
      submitting = false;
      optimisticMsg = null;
      clearStreaming();
      await onRefresh().catch(() => render());
      const serverMsg = (state.messages || []).find((m) => m.request_id === requestId);
      if (serverMsg?.status === 'complete') {
        lastFailed = null;
        activeTurn = null;
        sendBtn.disabled = false;
        typingEl.classList.add('hide');
        if (brainWork === turnWork) setStage('');
      } else if (
        serverMsg?.status === 'pending' ||
        (err.status === 409 && /already being processed/i.test(String(err.message || '')))
      ) {
        lastFailed = null;
        activeTurn = { id: serverMsg?.id || 0, requestId, text, work: turnWork };
        syncTurnState();
        showToast('连接波动，消息仍在后台处理，完成后会自动显示');
      } else {
        playWarn();
        activeTurn = null;
        sendBtn.disabled = false;
        typingEl.classList.add('hide');
        if (brainWork === turnWork) setStage('');
        lastFailed = { text, requestId };
        inputEl.value = text;
        syncComposerState();
        showToast(`发送失败：${err.message}`);
        inputEl.focus();
      }
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
      switchingThread = true;
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
    if (activeThread) {
      const matched = (state.threads || []).find((t) => t.id === activeThread.id);
      if (matched) activeThread.title = matched.title;
    }
    if (threadFilterBar && threadFilterTitle) {
      threadFilterBar.classList.toggle('hide', !activeThread);
      threadFilterTitle.textContent = activeThread ? `当前话题：${activeThread.title}` : '';
    }
    listEl.dataset.filtered = activeThread ? 'true' : 'false';

    const showOlder = activeThread ? threadHasMore : hasMoreGlobal;
    if (loadOlderWrap) {
      loadOlderWrap.classList.toggle('hide', !showOlder);
    }

    const baseMsgs = currentMessages();
    let msgs = baseMsgs;
    if (optimisticMsg && (!activeThread || optimisticMsg.thread_id === activeThread.id)) {
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
      renderedContents.clear();
      listEl.innerHTML = '';
      scrollAfterRender = true;
      switchingThread = false;
      syncScrollBottom();
      syncTurnState();
      return;
    }
    const stickBottom = !hasBootRendered || switchingThread || Boolean(optimisticMsg) || distFromBottom() <= 160;
    const maxRenderedId = msgs.reduce((mx, m) => Math.max(mx, m.id || 0), 0);
    const animateNew = hasBootRendered && !switchingThread;
    let newAssistantArrived = false;
    renderedContents.clear();
    listEl.innerHTML = msgs
      .map((m, idx) => {
        const key = String(idx);
        renderedContents.set(key, String(m.content || ''));
        const isNew = animateNew && (!m.id || m.id > lastSeenMsgId);
        if (isNew && !activeThread && m.role === 'assistant' && m.status !== 'failed') {
          newAssistantArrived = true;
        }
        const actionsHtml =
          m.role === 'assistant' && m.status !== 'failed'
            ? `<div class="msg-actions" aria-label="消息操作">
          <button type="button" class="msg-act-btn msg-act-copy" data-msg-idx="${key}" aria-label="复制全文" title="复制全文">
            <svg class="ic ic-copy" viewBox="0 0 24 24" aria-hidden="true"><use href="#ic-copy"/></svg>
            <svg class="ic ic-check" viewBox="0 0 24 24" aria-hidden="true"><use href="#ic-check"/></svg>
          </button>
          <button type="button" class="msg-act-btn msg-act-quote" data-msg-idx="${key}" aria-label="引用追问" title="引用追问">
            <svg class="ic" viewBox="0 0 24 24" aria-hidden="true"><use href="#ic-reply"/></svg>
          </button>
        </div>`
            : '';
        return `
      <div class="msg msg-${esc(m.role)}${m.status === 'failed' ? ' msg-failed' : ''}${isNew ? ' msg-enter' : ''}">
        ${actionsHtml}
        <div>${renderMarkdown(m.content)}</div>
        <div class="msg-meta">${esc(fmtTime(m.created_at))}${
          m.status === 'failed'
            ? ' · 未送达，可重新发送'
            : m.role === 'user' && m.status === 'pending'
              ? ' · 处理中…'
              : ''
        }</div>
      </div>`;
      })
      .join('');
    if (!activeThread) {
      lastSeenMsgId = Math.max(lastSeenMsgId, maxRenderedId);
      hasBootRendered = true;
    }
    if (newAssistantArrived) playDone();
    switchingThread = false;
    if (scrollAfterRender && stickBottom) {
      scrollerEl.scrollTop = scrollerEl.scrollHeight;
      scrollBottomBtn?.classList.remove('has-unread');
    } else if (newAssistantArrived && !stickBottom && scrollBottomBtn) {
      scrollBottomBtn.classList.add('is-visible', 'has-unread');
    }
    syncScrollBottom();
    scrollAfterRender = true;
    syncTurnState();
  }

  function recordTurnEvent(key, value) {
    if (!key) return;
    if (turnEvents.has(key)) turnEvents.delete(key);
    turnEvents.set(key, value);
    while (turnEvents.size > 32) {
      const oldest = turnEvents.keys().next().value;
      turnEvents.delete(oldest);
    }
  }

  function onMessageEvent(ev) {
    if (!ev || (!ev.done && !ev.failed)) return;
    const entry = {
      status: ev.failed ? 'failed' : 'complete',
      error: typeof ev.error === 'string' ? ev.error : '',
    };
    if (ev.message_id) recordTurnEvent(`id:${ev.message_id}`, entry);
    if (ev.request_id) recordTurnEvent(`req:${ev.request_id}`, entry);
  }

  function syncTurnState() {
    const msgsAll = state.messages || [];
    const pendingMsg = [...msgsAll]
      .reverse()
      .find((m) => m.role === 'user' && m.status === 'pending');

    if (pendingMsg) {
      const sameTurn =
        activeTurn &&
        ((pendingMsg.id && activeTurn.id === pendingMsg.id) ||
          (pendingMsg.request_id && activeTurn.requestId === pendingMsg.request_id));
      const work = sameTurn
        ? activeTurn.work
        : brainWork || { stage: 'connecting', startedAt: performance.now() };
      activeTurn = {
        id: pendingMsg.id || 0,
        requestId: pendingMsg.request_id || '',
        text: pendingMsg.content || '',
        work,
      };
      if (!brainWork) {
        brainWork = work;
        renderWork();
      }
      sendBtn.disabled = true;
      if (!streamingText) typingEl.classList.remove('hide');
      return;
    }

    if (submitting) return;

    if (activeTurn) {
      const finished = activeTurn;
      const maxMsgId = msgsAll.reduce((mx, m) => Math.max(mx, m.id || 0), 0);
      const finishedMsg = [...msgsAll]
        .reverse()
        .find(
          (m) =>
            m.role === 'user' &&
            ((finished.id && m.id === finished.id) ||
              (finished.requestId && m.request_id === finished.requestId)),
        );
      const terminalEv =
        (finished.id && turnEvents.get(`id:${finished.id}`)) ||
        (finished.requestId && turnEvents.get(`req:${finished.requestId}`));
      const settled =
        Boolean(finishedMsg && finishedMsg.status !== 'pending') ||
        Boolean(terminalEv) ||
        (finished.id && maxMsgId >= finished.id);
      if (!settled) {
        sendBtn.disabled = true;
        if (!streamingText) typingEl.classList.remove('hide');
        return;
      }

      activeTurn = null;
      optimisticMsg = null;
      clearStreaming();
      typingEl.classList.add('hide');
      sendBtn.disabled = false;
      if (brainWork) setStage('');

      if (finishedMsg?.status === 'failed' || terminalEv?.status === 'failed') {
        const errText = terminalEv?.error || '';
        const textToRetry = finishedMsg?.content || finished.text || '';
        const retryReqId = finishedMsg?.request_id || finished.requestId || '';
        playWarn();
        if (textToRetry && retryReqId) {
          lastFailed = { text: textToRetry, requestId: retryReqId };
          if (!inputEl.value.trim()) {
            inputEl.value = textToRetry;
            syncComposerState();
          }
        }
        showToast(errText ? `发送失败：${errText}` : '消息处理失败，可重新发送');
      } else {
        lastFailed = null;
        const activeTag = document.activeElement?.tagName;
        if (!activeTag || activeTag === 'BODY') {
          inputEl.focus();
        }
      }
      return;
    }

    sendBtn.disabled = false;
    typingEl.classList.add('hide');
  }

  async function setThreadFilter(threadId, title) {
    if (!threadId) return;
    activeThread = { id: threadId, title: title || threadId };
    threadMessages = [];
    threadHasMore = false;
    switchingThread = true;
    try {
      const params = new URLSearchParams({ thread_id: threadId, limit: '40' });
      const res = await api(`/api/messages?${params.toString()}`);
      threadMessages = res?.messages || [];
      threadHasMore = Boolean(res?.has_more);
      scrollAfterRender = true;
      render();
    } catch (err) {
      switchingThread = false;
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
    syncComposerState();
    inputEl.focus();
  }

  function closeEngineMenu() {
    if (!engineMenu || !engineBtn) return;
    engineMenu.classList.add('hide');
    engineBtn.setAttribute('aria-expanded', 'false');
  }

  function primaryNode() {
    const nodes = state.nodes || [];
    return (
      nodes.find((n) => n.online && n.kind === 'pc') ||
      nodes.find((n) => n.online) ||
      nodes[0] ||
      null
    );
  }

  async function switchEngine(node, ag) {
    if (!node || !ag || ag === node.default_agent) return;
    try {
      await api(`/api/nodes/${encodeURIComponent(node.id)}`, {
        method: 'PUT',
        body: { default_agent: ag },
      });
      playToggle(true);
      showToast(`已将 ${node.name} 默认引擎切换为 ${ag}`);
      await onRefresh();
    } catch (err) {
      playWarn();
      showToast(err.message);
    }
  }

  function parseNodeCaps(raw) {
    const info = {};
    for (const part of String(raw || '').split('|')) {
      const idx = part.indexOf(':');
      if (idx <= 0) continue;
      info[part.slice(0, idx).trim().toLowerCase()] = part.slice(idx + 1).trim().toLowerCase();
    }
    return info;
  }

  function nodeIsolationSummary(caps) {
    const sb = caps.sandbox || 'workspace-write';
    const iso = caps.isolation || '';
    if (sb === 'unsafe-full' || iso === 'direct') return '全宿主机权限';
    if (iso === 'bwrap' || iso === 'seatbelt') {
      return sb === 'readonly' ? `OS只读隔离 (${iso})` : `OS工作区隔离 (${iso})`;
    }
    if (sb === 'readonly') return '仅只读';
    return '';
  }

  function engineScopeLabel(ag, caps) {
    if (nodeIsolationSummary(caps)) return '';
    if (ag === 'codex') return caps.codex_net === '1' ? 'OS沙箱·可联网' : 'OS沙箱·断网';
    if (ag === 'agy') return 'CLI终端沙箱';
    if (ag === 'claude') return 'CLI工具白名单';
    if (ag === 'opencode') return 'CLI目录限制';
    return '工作区沙箱';
  }

  function renderEnginePill() {
    if (!engineWrap || !engineBtn || !engineLabel || !engineMenu) return;
    const node = primaryNode();
    if (!node) {
      engineWrap.classList.add('hide');
      closeEngineMenu();
      return;
    }
    engineWrap.classList.remove('hide');
    engineWrap.classList.toggle('is-online', Boolean(node.online));
    const caps = parseNodeCaps(node.capabilities);
    const curDefault = node.effective_default_agent || node.default_agent || 'auto';
    engineLabel.innerHTML = `<span class="engine-node-name">${esc(node.name)} · </span><span>${esc(curDefault)}</span>`;

    const candidates = Array.from(
      new Set([
        ...(node.effective_agents || []),
        ...(node.detected_agents || []),
        'agy',
        'opencode',
        'codex',
        'claude',
      ]),
    ).filter(Boolean);
    const isoSummary = nodeIsolationSummary(caps);

    engineMenu.innerHTML = `
      <div class="engine-menu-head">
        <span>${esc(node.name)}</span>
        <span class="badge ${node.online ? 'ok' : ''}">${node.online ? '在线' : '离线'}</span>
      </div>
      ${isoSummary ? `<div class="engine-menu-sub">${esc(isoSummary)}</div>` : ''}
      ${candidates
        .map((ag) => {
          const active = ag === (node.default_agent || curDefault);
          const scope = engineScopeLabel(ag, caps);
          const scopeHtml = scope ? `<span class="engine-menu-scope">${esc(scope)}</span>` : '';
          return `<button type="button" class="engine-menu-opt${active ? ' on' : ''}" role="menuitemradio" aria-checked="${active ? 'true' : 'false'}" data-pick-engine="${esc(ag)}"><span>${esc(ag)}</span><span class="engine-menu-opt-meta">${scopeHtml}<span class="engine-menu-check">${active ? '✓' : ''}</span></span></button>`;
        })
        .join('')}`;

    engineMenu.querySelectorAll('[data-pick-engine]').forEach((btn) => {
      btn.onclick = async (ev) => {
        ev.stopPropagation();
        const ag = btn.dataset.pickEngine;
        closeEngineMenu();
        await switchEngine(node, ag);
      };
    });
  }

  engineBtn?.addEventListener('click', (ev) => {
    ev.stopPropagation();
    if (!engineMenu) return;
    const open = engineMenu.classList.contains('hide');
    engineMenu.classList.toggle('hide', !open);
    engineBtn.setAttribute('aria-expanded', String(open));
    playToggle(open);
  });
  document.addEventListener('click', (ev) => {
    if (engineWrap && !engineWrap.contains(ev.target)) closeEngineMenu();
  });
  document.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape') closeEngineMenu();
  });

  listEl?.addEventListener('click', async (ev) => {
    const copyBtn = ev.target?.closest?.('.msg-act-copy');
    if (copyBtn) {
      ev.preventDefault();
      ev.stopPropagation();
      const raw = renderedContents.get(copyBtn.dataset.msgIdx || '') || '';
      if (!raw) return;
      const ok = await writeClipboard(raw);
      if (!ok) {
        playWarn();
        showToast('复制失败，请手动选择复制');
        return;
      }
      playTap();
      copyBtn.classList.add('is-copied');
      clearTimeout(copyBtn._copyTimer);
      copyBtn._copyTimer = setTimeout(() => copyBtn.classList.remove('is-copied'), 1600);
      return;
    }
    const quoteBtn = ev.target?.closest?.('.msg-act-quote');
    if (quoteBtn) {
      ev.preventDefault();
      ev.stopPropagation();
      const raw = renderedContents.get(quoteBtn.dataset.msgIdx || '') || '';
      const snippet = raw
        .split(/\r?\n/)
        .map((l) => l.replace(/^#+\s*/, '').trim())
        .filter(Boolean)
        .slice(0, 2)
        .join(' ')
        .slice(0, 110);
      if (!snippet) return;
      playTap();
      const prefix = `> ${snippet}\n\n`;
      inputEl.value = inputEl.value.trim() ? `${prefix}${inputEl.value}` : prefix;
      syncComposerState();
      inputEl.focus();
      inputEl.setSelectionRange(inputEl.value.length, inputEl.value.length);
    }
  });

  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && activeTurn) {
      onRefresh().catch(() => {});
    }
  });

  initDelegatedCopy(listEl);
  syncComposerState();
  renderEnginePill();
  return { render, setStage, onDelta, onTask, onMessageEvent, syncTasks, insertMention, setThreadFilter, renderEnginePill, flashIslandDone, switchEngine };
}
