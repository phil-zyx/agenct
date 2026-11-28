// Chat stream, typing indicator, attachment uploads, and floating pill composer.
import { api, esc, fmtTime, renderMarkdown, showToast } from './api.js';
import { playDone, playSend, playTap, playToggle, playWarn } from './sound.js';
import { renderStepTrailHtml } from './triage.js';
import { bindHoldAction, closeContextMenu, fmtBytes, initDelegatedCopy, openContextMenu, writeClipboard } from './ui.js';

function randomId() {
  if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
  const b = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

export function initChat({ state, onRefresh, onActivityChange, onOpenSoulMeter }) {
  const scrollerEl = document.getElementById('scroller');
  const startersEl = document.getElementById('chat-starters');
  const listEl = document.getElementById('messages');
  const actionDeckEl = document.getElementById('chat-action-deck');
  const formEl = document.getElementById('chat-form');
  const pillEl = formEl.querySelector('.composer-pill');
  const orbitEl = document.getElementById('composer-orbit');
  const suggestEl = document.getElementById('composer-suggest');
  const statusThreadWrap = document.getElementById('status-thread-wrap');
  const statusThreadBtn = document.getElementById('status-thread-btn');
  const threadMenuEl = document.getElementById('composer-thread-menu');
  const statusSandboxChip = document.getElementById('status-sandbox-chip');
  const statusCtxBtn = document.getElementById('status-ctx-btn');
  const statusCtxFill = document.getElementById('status-ctx-fill');
  const statusCtxLabel = document.getElementById('status-ctx-label');
  const engineWrap = document.getElementById('composer-engine-wrap');
  const engineBtn = document.getElementById('composer-engine-btn');
  const engineLabel = document.getElementById('composer-engine-label');
  const engineMenu = document.getElementById('composer-engine-menu');
  const inputEl = document.getElementById('chat-input');
  const sendBtn = document.getElementById('send-btn');
  const fileEl = document.getElementById('file-input');
  const attsEl = document.getElementById('pending-atts');
  const stageEl = document.getElementById('stage-bar');
  const workIndicatorEl = document.getElementById('work-indicator');
  const workStageEl = document.getElementById('work-stage');
  const elapsedEl = document.getElementById('work-elapsed');
  const workStopBtn = document.getElementById('work-stop-btn');
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
  let activeCancellableTaskId = null;
  let lastRenderedTail = '';
  let isComposingInput = false;
  let suggestOpen = false;
  let suggestItems = [];
  let suggestIdx = 0;
  let suggestRange = null;
  const renderedContents = new Map();
  const renderedMsgs = new Map();

  function syncOrbitGeometry() {
    if (!pillEl || !orbitEl) return;
    const w = pillEl.clientWidth;
    const h = pillEl.clientHeight;
    if (!w || !h) return;
    const rawRx = parseFloat(getComputedStyle(pillEl).borderTopLeftRadius) || 26;
    const rx = Math.max(4, Math.min(rawRx - 0.75, Math.min(w, h) / 2 - 0.75));
    orbitEl.setAttribute('viewBox', `0 0 ${w} ${h}`);
    const rw = Math.max(0, w - 1.5);
    const rh = Math.max(0, h - 1.5);
    orbitEl.querySelectorAll('rect').forEach((rect) => {
      rect.setAttribute('x', '0.75');
      rect.setAttribute('y', '0.75');
      rect.setAttribute('width', String(rw));
      rect.setAttribute('height', String(rh));
      rect.setAttribute('rx', String(rx));
      rect.setAttribute('ry', String(rx));
    });
  }

  if (pillEl && typeof ResizeObserver === 'function') {
    const ro = new ResizeObserver(() => syncOrbitGeometry());
    ro.observe(pillEl);
  }

  if (workStopBtn) {
    bindHoldAction(
      workStopBtn,
      async () => {
        const tid = activeCancellableTaskId;
        if (!tid) return;
        try {
          await api(`/api/tasks/${encodeURIComponent(tid)}/action`, {
            method: 'POST',
            body: { action: 'cancel' },
          });
          showToast('任务已中止');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      },
      { duration: 650, shortHint: '请长按「停止」确认中止任务' },
    );
  }

  const STARTER_CARDS = [
    {
      icon: '#ic-bolt',
      title: '巡检本机仓库',
      sub: '让在线节点查看工作区 git 状态与近期提交',
      prompt: '帮我检查在线本机节点的工作区仓库状态和最近几次提交。',
    },
    {
      icon: '#ic-shield',
      title: '配置我的军团角色',
      sub: '设定只读规划 planner 与可写执行 executor',
      prompt: '帮我配置两个军团角色：planner（只读规划）和 executor（可写执行）。',
    },
    {
      icon: '#ic-tasks',
      title: '创建每日定时汇报',
      sub: '每天固定时间自动汇总进展并推送到 IM',
      prompt: '帮我创建一个每天 18:30 的定时任务：汇总今天的代码提交与未完待办。',
    },
    {
      icon: '#ic-spark',
      title: '记住我的工作偏好',
      sub: '把习惯、环境事实或输出风格写入长期记忆',
      prompt: '记住：默认用中文简短汇报结论，涉及改动文件时先给计划再动手。',
    },
  ];

  function renderStartersOnce() {
    if (!startersEl || startersEl.dataset.ready) return;
    startersEl.dataset.ready = '1';
    startersEl.innerHTML = `
      <div class="starters-head">
        <span class="starters-kicker">快速开始</span>
        <p class="starters-lead">直接输入需求，或从下面选一个常用起点</p>
      </div>
      <div class="starters-grid">
        ${STARTER_CARDS.map(
          (c, i) => `
          <button type="button" class="starter-card" data-starter-idx="${i}">
            <span class="starter-ic"><svg class="ic" viewBox="0 0 24 24" aria-hidden="true"><use href="${esc(c.icon)}"/></svg></span>
            <span class="starter-body">
              <span class="starter-title">${esc(c.title)}</span>
              <span class="starter-sub">${esc(c.sub)}</span>
            </span>
          </button>`,
        ).join('')}
      </div>`;
    startersEl.querySelectorAll('[data-starter-idx]').forEach((btn) => {
      btn.onclick = () => {
        const card = STARTER_CARDS[Number(btn.dataset.starterIdx)];
        if (!card) return;
        playTap();
        inputEl.value = card.prompt;
        closeSuggest();
        syncComposerState();
        inputEl.focus();
        inputEl.setSelectionRange(inputEl.value.length, inputEl.value.length);
      };
    });
  }
  renderStartersOnce();

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
    const stageLower = stage.toLowerCase();
    let variant = 'stars';
    if (task?.status === 'queued') variant = 'spin';
    else if (task?.status === 'running') {
      variant = stageLower.includes('plan') || stageLower.includes('readonly') || stageLower.includes('只读')
        ? 'stars'
        : 'wave';
    } else if (streamingText || stageLower === 'summarizing') {
      variant = 'pulse';
    }
    if (workIndicatorEl) workIndicatorEl.dataset.thinkVariant = variant;

    const label = humanizeStage(stage, task?.status);
    const text = busy ? `正在工作 · ${label}` : '';
    if (workStageEl.textContent !== text) workStageEl.textContent = text;
    stageEl.classList.toggle('hide', !busy);
    headEl?.classList.toggle('is-thinking', busy);
    heroEl?.classList.toggle('is-thinking', busy);

    activeCancellableTaskId =
      task && (task.status === 'running' || task.status === 'queued') && task.taskId
        ? task.taskId
        : null;
    workStopBtn?.classList.toggle('hide', !activeCancellableTaskId);

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
    if (tail !== lastRenderedTail) {
      lastRenderedTail = tail;
      logTailEl.innerHTML = tail ? renderStepTrailHtml(tailLines) : '';
    }
    workLogEl.dataset.steps = String(tailLines.length);
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
    const myUid = state.user?.id;
    for (const task of tasks) {
      if (myUid && task.owner_id !== myUid) continue;
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
    syncOrbitGeometry();
  }

  function attachmentBadgeLabel(att) {
    const mime = String(att.mime_type || '').toLowerCase();
    const name = String(att.name || '').toLowerCase();
    if (mime.startsWith('image/')) return 'IMG';
    if (mime === 'application/pdf' || name.endsWith('.pdf')) return 'PDF';
    if (mime.includes('zip') || name.endsWith('.zip')) return 'ZIP';
    if (mime === 'application/json' || name.endsWith('.json')) return 'JSON';
    if (mime === 'text/markdown' || name.endsWith('.md')) return 'MD';
    if (mime === 'text/csv' || name.endsWith('.csv')) return 'CSV';
    if (mime.startsWith('text/') || name.endsWith('.txt')) return 'TXT';
    return 'FILE';
  }

  function revokeAttachmentPreview(att) {
    if (att?.previewUrl) {
      try {
        URL.revokeObjectURL(att.previewUrl);
      } catch {
        // ignore
      }
    }
  }

  function renderPendingAtts() {
    if (!pendingAtts.length) {
      attsEl.classList.add('hide');
      attsEl.innerHTML = '';
      syncComposerState();
      return;
    }
    attsEl.classList.remove('hide');
    const ringCirc = 2 * Math.PI * 15;
    attsEl.innerHTML = pendingAtts
      .map((a, i) => {
        const badge = attachmentBadgeLabel(a);
        const visualHtml = a.previewUrl
          ? `<img class="att-thumb" src="${esc(a.previewUrl)}" alt=""><span class="att-badge">${esc(badge)}</span>`
          : `<span class="att-badge">${esc(badge)}</span>`;
        const pct = Math.max(0, Math.min(100, Number(a.progress) || 0));
        const metaText = a.uploading ? '上传中…' : fmtBytes(a.size_bytes);
        return `
          <div class="att-tile${a.uploading ? ' is-uploading' : ''}${a.previewUrl ? ' has-thumb' : ''}" data-att-idx="${i}">
            <span class="att-visual">
              ${visualHtml}
              <svg class="att-ring" viewBox="0 0 36 36" aria-hidden="true">
                <circle class="att-ring-track" cx="18" cy="18" r="15"/>
                <circle class="att-ring-fill" cx="18" cy="18" r="15" data-att-pct="${pct}"/>
              </svg>
            </span>
            <span class="att-info">
              <span class="att-name">${esc(a.name)}</span>
              <span class="att-meta">${esc(metaText)}</span>
            </span>
            <button type="button" class="att-rm-btn" data-rm="${i}" aria-label="移除附件">×</button>
          </div>`;
      })
      .join('');
    attsEl.querySelectorAll('.att-ring-fill').forEach((circle) => {
      const pct = Number(circle.dataset.attPct || 100);
      circle.style.setProperty('--att-circ', String(ringCirc));
      circle.style.setProperty('--att-dashoffset', String(ringCirc * (1 - pct / 100)));
    });
    attsEl.querySelectorAll('.att-thumb').forEach((img) => {
      img.addEventListener('error', () => {
        img.closest('.att-tile')?.classList.add('is-thumb-fallback');
      });
    });
    attsEl.querySelectorAll('[data-rm]').forEach((btn) => {
      btn.onclick = () => {
        const idx = Number(btn.dataset.rm);
        const [removed] = pendingAtts.splice(idx, 1);
        revokeAttachmentPreview(removed);
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
    const localId = randomId();
    const mime = file.type || 'text/plain';
    const previewUrl = mime.startsWith('image/') && typeof URL?.createObjectURL === 'function'
      ? URL.createObjectURL(file)
      : '';
    const entry = {
      localId,
      id: '',
      name,
      mime_type: mime,
      size_bytes: file.size || 0,
      previewUrl,
      uploading: true,
      progress: 28,
    };
    pendingAtts.push(entry);
    renderPendingAtts();

    try {
      const buf = await file.arrayBuffer();
      const stillQueued = pendingAtts.find((a) => a.localId === localId);
      if (!stillQueued) {
        revokeAttachmentPreview(entry);
        return;
      }
      stillQueued.progress = 68;
      renderPendingAtts();
      let binary = '';
      const bytes = new Uint8Array(buf);
      for (let i = 0; i < bytes.length; i += 8192) {
        binary += String.fromCharCode(...bytes.subarray(i, i + 8192));
      }
      const meta = await api('/api/attachments', {
        method: 'POST',
        body: {
          name,
          mime_type: mime,
          data_base64: btoa(binary),
        },
      });
      const target = pendingAtts.find((a) => a.localId === localId);
      if (!target) {
        revokeAttachmentPreview(entry);
        return;
      }
      target.id = meta.id;
      target.name = meta.name || name;
      target.mime_type = meta.mime_type || mime;
      target.size_bytes = meta.size_bytes || file.size || 0;
      target.progress = 100;
      target.uploading = false;
      renderPendingAtts();
      showToast(`已上传：${target.name}`);
    } catch (err) {
      const idx = pendingAtts.findIndex((a) => a.localId === localId);
      if (idx >= 0) {
        const [removed] = pendingAtts.splice(idx, 1);
        revokeAttachmentPreview(removed);
        renderPendingAtts();
      } else {
        revokeAttachmentPreview(entry);
      }
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

  function closeSuggest() {
    if (!suggestOpen && suggestEl?.classList.contains('hide')) return;
    suggestOpen = false;
    suggestItems = [];
    suggestRange = null;
    suggestEl?.classList.add('hide');
    if (suggestEl) suggestEl.innerHTML = '';
  }

  function syncSuggestSelection() {
    if (!suggestEl) return;
    const opts = Array.from(suggestEl.querySelectorAll('.suggest-item'));
    if (!opts.length) return;
    if (suggestIdx < 0 || suggestIdx >= opts.length) suggestIdx = 0;
    opts.forEach((el, i) => {
      const on = i === suggestIdx;
      el.classList.toggle('is-selected', on);
      el.setAttribute('aria-selected', String(on));
      if (on) el.scrollIntoView({ block: 'nearest' });
    });
  }

  function applySuggest(idx) {
    const item = suggestItems[idx];
    if (!item || !suggestRange) return;
    const val = inputEl.value;
    const next = val.slice(0, suggestRange.start) + item.insert + val.slice(suggestRange.end);
    const nextCaret = suggestRange.start + item.insert.length;
    inputEl.value = next;
    closeSuggest();
    syncComposerState();
    inputEl.focus();
    inputEl.setSelectionRange(nextCaret, nextCaret);
    playTap();
  }

  function buildSuggestCandidates(kind, query) {
    const q = query.toLowerCase();
    const groups = [];
    if (kind === '/') {
      const cmds = [];
      const myUid = state.user?.id;
      const hasElev = (state.tasks || []).some(
        (t) => (!myUid || t.owner_id === myUid) && t.status === 'pending' && t.sandbox === 'elevated',
      );
      if (hasElev) {
        cmds.push({
          title: '/approve',
          sub: '输入 6 位审批码单次放行挂起任务',
          badge: '待放行',
          icon: '#ic-shield',
          insert: '/approve ',
          search: 'approve 放行 审批',
        });
      }
      cmds.push({
        title: '/node',
        sub: '直接派发任务给在线执行节点',
        badge: '节点',
        icon: '#ic-bolt',
        insert: '/node ',
        search: 'node 节点 执行',
      });
      for (const r of state.roles || []) {
        cmds.push({
          title: `/node --role ${r.name}`,
          sub: r.description || `按 ${r.name} 角色规范派发（${r.agent || '默认'}）`,
          badge: r.sandbox === 'readonly' ? '只读角色' : '角色',
          icon: '#ic-spark',
          insert: `/node --role ${r.name} `,
          search: `node role ${r.name} ${r.description || ''}`,
        });
      }
      cmds.push(
        {
          title: '/role',
          sub: '通过对话新建或调整军团角色分工',
          badge: '军团',
          icon: '#ic-tuning',
          insert: '/role ',
          search: 'role 角色 军团',
        },
        {
          title: '/cron',
          sub: '插入每日定时汇报指令模板',
          badge: '定时',
          icon: '#ic-tasks',
          insert: '每天 18:30 帮我汇总今日进展并汇报：',
          search: 'cron 定时 汇报',
        },
        {
          title: '/memory',
          sub: '让管家整理并精简长期记忆',
          badge: '记忆',
          icon: '#ic-avatar',
          insert: '帮我整理并精简长期记忆，剔除已过时条目。',
          search: 'memory 记忆',
        },
      );
      const matched = q
        ? cmds.filter((c) => c.title.toLowerCase().includes(q) || c.search.toLowerCase().includes(q))
        : cmds;
      if (matched.length) groups.push({ label: '快捷指令', items: matched.slice(0, 7) });
      return groups;
    }

    const contacts = (state.contacts || [])
      .filter((c) => !q || `${c.username} ${c.display_name}`.toLowerCase().includes(q))
      .slice(0, 5)
      .map((c) => ({
        title: `@${c.username}`,
        sub: `${c.display_name} · 发起团队协作`,
        badge: '同事',
        icon: '#ic-contacts',
        insert: `@${c.username} `,
      }));
    if (contacts.length) groups.push({ label: '同事协作', items: contacts });

    const roles = (state.roles || [])
      .filter((r) => !q || `${r.name} ${r.description || ''}`.toLowerCase().includes(q))
      .slice(0, 5)
      .map((r) => ({
        title: r.name,
        sub: r.description || `${r.agent || '节点默认'}${r.sandbox === 'readonly' ? ' · 只读' : ''}`,
        badge: '角色',
        icon: '#ic-spark',
        insert: `请用 ${r.name} 角色处理：`,
      }));
    if (roles.length) groups.push({ label: '军团角色', items: roles });

    const nodes = (state.nodes || [])
      .filter((n) => !q || `${n.name} ${n.id}`.toLowerCase().includes(q))
      .slice(0, 4)
      .map((n) => ({
        title: n.name,
        sub: `${n.online ? '在线' : '离线'} · ${n.effective_default_agent || n.default_agent || 'auto'}`,
        badge: n.online ? '在线节点' : '离线节点',
        icon: '#ic-bolt',
        insert: `在节点 ${n.name} 上：`,
      }));
    if (nodes.length) groups.push({ label: '执行节点', items: nodes });
    return groups;
  }

  function updateSuggest() {
    if (!suggestEl || isComposingInput) return;
    const caret = inputEl.selectionStart ?? inputEl.value.length;
    if ((inputEl.selectionEnd ?? caret) !== caret) {
      closeSuggest();
      return;
    }
    const before = inputEl.value.slice(0, caret);
    let kind = '';
    let query = '';
    let start = 0;

    const slashMatch = before.match(/^\/([a-zA-Z0-9_-]*)$/);
    if (slashMatch) {
      kind = '/';
      query = slashMatch[1];
      start = 0;
    } else {
      const atMatch = before.match(/(?:^|[\s(（])@([^\s@]*)$/);
      if (atMatch) {
        kind = '@';
        query = atMatch[1];
        start = before.length - query.length - 1;
      }
    }

    if (!kind) {
      closeSuggest();
      return;
    }
    const groups = buildSuggestCandidates(kind, query);
    const flat = [];
    const htmlParts = [];
    for (const g of groups) {
      if (!g.items.length) continue;
      htmlParts.push(`<div class="suggest-group-title">${esc(g.label)}</div>`);
      for (const item of g.items) {
        const idx = flat.length;
        flat.push(item);
        htmlParts.push(`
          <button type="button" class="suggest-item${idx === 0 ? ' is-selected' : ''}" data-suggest-idx="${idx}" role="option" aria-selected="${idx === 0 ? 'true' : 'false'}">
            <span class="suggest-item-ic"><svg class="ic" viewBox="0 0 24 24" aria-hidden="true"><use href="${esc(item.icon)}"/></svg></span>
            <span class="suggest-item-body">
              <span class="suggest-item-title">${esc(item.title)}</span>
              ${item.sub ? `<span class="suggest-item-sub">${esc(item.sub)}</span>` : ''}
            </span>
            ${item.badge ? `<span class="badge">${esc(item.badge)}</span>` : ''}
          </button>`);
      }
    }
    if (!flat.length) {
      closeSuggest();
      return;
    }
    suggestOpen = true;
    suggestItems = flat;
    suggestIdx = 0;
    suggestRange = { start, end: caret };
    suggestEl.innerHTML = htmlParts.join('');
    suggestEl.classList.remove('hide');
  }

  suggestEl?.addEventListener('mousedown', (ev) => {
    ev.preventDefault();
  });
  suggestEl?.addEventListener('click', (ev) => {
    const btn = ev.target?.closest?.('.suggest-item');
    if (!btn) return;
    applySuggest(Number(btn.dataset.suggestIdx));
  });
  suggestEl?.addEventListener('pointermove', (ev) => {
    const btn = ev.target?.closest?.('.suggest-item');
    if (!btn) return;
    const idx = Number(btn.dataset.suggestIdx);
    if (Number.isFinite(idx) && idx !== suggestIdx) {
      suggestIdx = idx;
      syncSuggestSelection();
    }
  });

  inputEl.addEventListener('compositionstart', () => {
    isComposingInput = true;
  });
  inputEl.addEventListener('compositionend', () => {
    isComposingInput = false;
    updateSuggest();
  });
  inputEl.addEventListener('input', () => {
    syncComposerState();
    updateSuggest();
  });
  inputEl.addEventListener('click', updateSuggest);

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
    if (suggestOpen) {
      if (ev.key === 'ArrowDown') {
        ev.preventDefault();
        suggestIdx = (suggestIdx + 1) % suggestItems.length;
        syncSuggestSelection();
        return;
      }
      if (ev.key === 'ArrowUp') {
        ev.preventDefault();
        suggestIdx = (suggestIdx - 1 + suggestItems.length) % suggestItems.length;
        syncSuggestSelection();
        return;
      }
      if ((ev.key === 'Tab' || (ev.key === 'Enter' && !ev.shiftKey)) && !ev.isComposing) {
        ev.preventDefault();
        applySuggest(suggestIdx);
        return;
      }
      if (ev.key === 'Escape') {
        ev.preventDefault();
        ev.stopPropagation();
        closeSuggest();
        return;
      }
    }
    if (ev.key === 'Enter' && !ev.shiftKey && !ev.isComposing) {
      ev.preventDefault();
      formEl.requestSubmit();
    }
  });

  formEl.onsubmit = async (ev) => {
    ev.preventDefault();
    if (submitting || activeTurn) return;
    if (pendingAtts.some((a) => a.uploading)) {
      showToast('附件仍在上传，请稍候');
      return;
    }
    closeSuggest();
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
    pendingAtts.forEach(revokeAttachmentPreview);
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
    clearThreadFilterBtn.onclick = (ev) => {
      ev.stopPropagation();
      closeThreadMenu();
      activeThread = null;
      threadMessages = [];
      threadHasMore = false;
      switchingThread = true;
      scrollAfterRender = true;
      render();
    };
  }

  function closeThreadMenu() {
    if (!threadMenuEl || !statusThreadBtn) return;
    threadMenuEl.classList.add('hide');
    statusThreadBtn.setAttribute('aria-expanded', 'false');
  }

  function clearThreadIfActive(threadId) {
    if (!activeThread || activeThread.id !== threadId) return;
    activeThread = null;
    threadMessages = [];
    threadHasMore = false;
    switchingThread = true;
    scrollAfterRender = true;
    render();
  }

  function renderStatusTab() {
    const activeThreads = (state.threads || []).filter((t) => t.status !== 'archived');
    if (threadFilterTitle) {
      if (activeThread) {
        threadFilterTitle.textContent = `话题：${activeThread.title}`;
      } else {
        threadFilterTitle.textContent = activeThreads.length
          ? `全部对话 · ${activeThreads.length} 话题`
          : '全部对话';
      }
    }
    clearThreadFilterBtn?.classList.toggle('hide', !activeThread);
    statusThreadWrap?.classList.toggle('is-filtered', Boolean(activeThread));

    if (threadMenuEl) {
      const opts = [
        `<button type="button" class="engine-menu-opt${!activeThread ? ' on' : ''}" role="menuitemradio" aria-checked="${!activeThread ? 'true' : 'false'}" data-pick-thread=""><span>全部对话（全局流）</span><span class="engine-menu-check">${!activeThread ? '✓' : ''}</span></button>`,
        ...activeThreads.slice(0, 8).map((t) => {
          const on = activeThread?.id === t.id;
          return `<button type="button" class="engine-menu-opt${on ? ' on' : ''}" role="menuitemradio" aria-checked="${on ? 'true' : 'false'}" data-pick-thread="${esc(t.id)}" data-thread-title="${esc(t.title)}"><span>${esc(t.title)}</span><span class="engine-menu-opt-meta"><span class="engine-menu-scope">${t.turn_count || 0}轮</span><span class="engine-menu-check">${on ? '✓' : ''}</span></span></button>`;
        }),
      ];
      threadMenuEl.innerHTML = `<div class="engine-menu-head"><span>切换对话范围</span></div>${opts.join('')}`;
      threadMenuEl.querySelectorAll('[data-pick-thread]').forEach((btn) => {
        btn.onclick = (ev) => {
          ev.stopPropagation();
          const tid = btn.dataset.pickThread;
          const title = btn.dataset.threadTitle || tid;
          closeThreadMenu();
          playTap();
          if (!tid) {
            activeThread = null;
            threadMessages = [];
            threadHasMore = false;
            switchingThread = true;
            scrollAfterRender = true;
            render();
          } else {
            setThreadFilter(tid, title);
          }
        };
      });
    }

    if (statusSandboxChip) {
      const node = primaryNode();
      if (!node) {
        statusSandboxChip.classList.add('hide');
        statusSandboxChip.textContent = '';
      } else {
        const caps = parseNodeCaps(node.capabilities);
        const iso = nodeIsolationSummary(caps) || '工作区沙箱';
        statusSandboxChip.textContent = iso;
        statusSandboxChip.classList.toggle('is-warn', iso === '全宿主机权限');
        statusSandboxChip.classList.remove('hide');
      }
    }

    if (statusCtxLabel && statusCtxFill) {
      const cs = state.context_stats || {};
      const uncompacted = Number.isFinite(cs.uncompacted_messages)
        ? cs.uncompacted_messages
        : (state.messages || []).length;
      const trigger = Number.isFinite(cs.compact_trigger_messages) ? cs.compact_trigger_messages : 18;
      const pct = Math.min(100, Math.round((uncompacted / Math.max(1, trigger)) * 100));
      statusCtxLabel.textContent = `${uncompacted}/${trigger}`;
      statusCtxFill.style.strokeDashoffset = String(100 - pct);
    }
  }

  statusThreadBtn?.addEventListener('click', (ev) => {
    ev.stopPropagation();
    if (!threadMenuEl) return;
    const open = threadMenuEl.classList.contains('hide');
    threadMenuEl.classList.toggle('hide', !open);
    statusThreadBtn.setAttribute('aria-expanded', String(open));
    playToggle(open);
  });
  statusCtxBtn?.addEventListener('click', () => {
    playTap();
    onOpenSoulMeter?.();
  });

  function renderActionDeck() {
    if (!actionDeckEl) return;
    const uid = state.user?.id;
    if (!uid) {
      actionDeckEl.classList.add('hide');
      actionDeckEl.innerHTML = '';
      return;
    }
    const nodes = state.nodes || [];
    const elevatedNodeIds = new Set(
      nodes.filter((n) => parseNodeCaps(n.capabilities).elevated === '1').map((n) => n.id),
    );
    const anyElevated = elevatedNodeIds.size > 0;
    const tasks = (state.tasks || []).filter(
      (t) => t.owner_id === uid && (!activeThread || t.thread_id === activeThread.id),
    );

    const actionable = [];
    for (const t of tasks) {
      const isElevPending = t.requester_id === uid && t.status === 'pending' && t.sandbox === 'elevated';
      const isBlockedFail = t.requester_id === uid && t.status === 'failed' && Boolean(t.blocked_reason);
      const isCollab = t.requester_id !== uid && (t.status === 'pending' || t.status === 'running');
      if (isElevPending || isBlockedFail) {
        actionable.push({ kind: 'elev', task: t });
      } else if (isCollab) {
        actionable.push({ kind: 'collab', task: t });
      }
      if (actionable.length >= 3) break;
    }

    if (!actionable.length) {
      actionDeckEl.classList.add('hide');
      actionDeckEl.innerHTML = '';
      return;
    }

    actionDeckEl.innerHTML = actionable
      .map(({ kind, task: t }) => {
        if (kind === 'elev') {
          const canElev = t.node_id ? elevatedNodeIds.has(t.node_id) : anyElevated;
          const badgeText = t.status === 'pending' ? '待单次放行' : '沙箱受阻';
          const reason = t.blocked_reason || t.instruction || '';
          const elevLabel = t.status === 'pending' ? '单次放行执行' : '单次放行重试';
          return `
            <div class="decision-card is-warn" data-decision-id="${esc(t.id)}">
              <div class="decision-head">
                <span class="decision-title"><svg class="ic" viewBox="0 0 24 24" aria-hidden="true"><use href="#ic-shield"/></svg><span>${esc(t.title)}</span></span>
                <span class="badge warn">${esc(badgeText)}</span>
              </div>
              ${reason ? `<div class="decision-sub">${esc(reason.slice(0, 140))}</div>` : ''}
              <div class="row">
                ${canElev ? `<button type="button" class="sm warn" data-deck-elevate="${esc(t.id)}">${esc(elevLabel)}</button>` : ''}
                ${t.status === 'failed' ? `<button type="button" class="sm primary" data-deck-assign="${esc(t.id)}">普通重试</button>` : ''}
                <button type="button" class="sm danger" data-deck-cancel="${esc(t.id)}">取消</button>
              </div>
            </div>`;
        }
        return `
          <div class="decision-card is-info" data-decision-id="${esc(t.id)}">
            <div class="decision-head">
              <span class="decision-title"><svg class="ic" viewBox="0 0 24 24" aria-hidden="true"><use href="#ic-contacts"/></svg><span>${esc(t.title)}</span></span>
              <span class="badge">${t.status === 'running' ? '执行中' : '同事协作'}</span>
            </div>
            ${t.instruction ? `<div class="decision-sub">${esc(t.instruction.slice(0, 140))}</div>` : ''}
            <div class="decision-deliver-row">
              <input type="text" class="decision-deliver-input" data-deck-deliver-input="${esc(t.id)}" placeholder="输入交付结论或处理结果…">
              <button type="button" class="sm primary" data-deck-deliver="${esc(t.id)}">交付结果</button>
            </div>
            <div class="row">
              ${t.status === 'pending' ? `<button type="button" class="sm" data-deck-assign="${esc(t.id)}">派给节点</button>` : ''}
              <button type="button" class="sm danger" data-deck-cancel="${esc(t.id)}">取消</button>
            </div>
          </div>`;
      })
      .join('');
    actionDeckEl.classList.remove('hide');

    actionDeckEl.querySelectorAll('[data-deck-elevate]').forEach((b) => {
      bindHoldAction(
        b,
        async () => {
          try {
            await api(`/api/tasks/${encodeURIComponent(b.dataset.deckElevate)}/action`, {
              method: 'POST',
              body: { action: 'retry_elevated' },
            });
            showToast('已批准单次放行');
            await onRefresh();
          } catch (err) {
            showToast(err.message);
          }
        },
        { shortHint: '请长按确认仅本次放行' },
      );
    });
    actionDeckEl.querySelectorAll('[data-deck-cancel]').forEach((b) => {
      bindHoldAction(
        b,
        async () => {
          try {
            await api(`/api/tasks/${encodeURIComponent(b.dataset.deckCancel)}/action`, {
              method: 'POST',
              body: { action: 'cancel' },
            });
            showToast('任务已取消');
            await onRefresh();
          } catch (err) {
            showToast(err.message);
          }
        },
        { shortHint: '请长按「取消」确认中止任务' },
      );
    });
    actionDeckEl.querySelectorAll('[data-deck-assign]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/tasks/${encodeURIComponent(b.dataset.deckAssign)}/action`, {
            method: 'POST',
            body: { action: 'assign_node' },
          });
          showToast('已派发给节点');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    actionDeckEl.querySelectorAll('[data-deck-deliver]').forEach((b) => {
      b.onclick = async () => {
        const tid = b.dataset.deckDeliver;
        const input = actionDeckEl.querySelector(`[data-deck-deliver-input="${CSS.escape(tid)}"]`);
        const result = String(input?.value || '').trim();
        if (!result) {
          showToast('请先填写交付结果内容', { tone: 'warn' });
          input?.focus();
          return;
        }
        try {
          await api(`/api/tasks/${encodeURIComponent(tid)}/action`, {
            method: 'POST',
            body: { action: 'deliver', result },
          });
          showToast('协作结果已交付');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
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
    renderStatusTab();
    renderActionDeck();
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
    if (startersEl) {
      const showStarters = !activeThread && msgs.length === 0 && !streamingText;
      startersEl.classList.toggle('hide', !showStarters);
    }
    if (!msgs.length) {
      renderedContents.clear();
      renderedMsgs.clear();
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
    renderedMsgs.clear();
    listEl.innerHTML = msgs
      .map((m, idx) => {
        const key = String(idx);
        renderedContents.set(key, String(m.content || ''));
        renderedMsgs.set(key, m);
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
      <div class="msg msg-${esc(m.role)}${m.status === 'failed' ? ' msg-failed' : ''}${isNew ? ' msg-enter' : ''}" data-msg-idx="${key}">
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
      if (wasEmpty) {
        startersEl?.classList.add('hide');
        emitActivity();
        renderWork();
      }
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
    if (statusThreadWrap && !statusThreadWrap.contains(ev.target)) closeThreadMenu();
    if (formEl && !formEl.contains(ev.target)) closeSuggest();
  });
  document.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape') {
      closeEngineMenu();
      closeThreadMenu();
      closeSuggest();
      closeContextMenu();
    }
  });

  listEl?.addEventListener('contextmenu', (ev) => {
    if (window.getSelection()?.toString()?.trim()) return;
    const msgEl = ev.target?.closest?.('.msg[data-msg-idx]');
    if (!msgEl) return;
    const idx = msgEl.dataset.msgIdx || '';
    const raw = renderedContents.get(idx) || '';
    const meta = renderedMsgs.get(idx);
    if (!raw) return;
    ev.preventDefault();
    const codeMatch = raw.match(/```[^\n]*\n([\s\S]*?)```/);
    const items = [
      {
        label: '复制全文',
        icon: '#ic-copy',
        onSelect: async () => {
          const ok = await writeClipboard(raw);
          showToast(ok ? '已复制消息全文' : '复制失败，请手动选择');
        },
      },
      {
        label: '引用追问',
        icon: '#ic-reply',
        onSelect: () => {
          const snippet = raw
            .split(/\r?\n/)
            .map((l) => l.replace(/^#+\s*/, '').trim())
            .filter(Boolean)
            .slice(0, 2)
            .join(' ')
            .slice(0, 110);
          if (!snippet) return;
          const prefix = `> ${snippet}\n\n`;
          inputEl.value = inputEl.value.trim() ? `${prefix}${inputEl.value}` : prefix;
          syncComposerState();
          inputEl.focus();
          inputEl.setSelectionRange(inputEl.value.length, inputEl.value.length);
        },
      },
    ];
    if (codeMatch?.[1]) {
      items.push({
        label: '复制代码块',
        icon: '#ic-file',
        onSelect: async () => {
          const ok = await writeClipboard(codeMatch[1].trim());
          showToast(ok ? '已复制代码块' : '复制失败');
        },
      });
    }
    if (meta?.thread_id && !activeThread) {
      const thrTitle =
        (state.threads || []).find((t) => t.id === meta.thread_id)?.title || meta.thread_id;
      items.push({ separator: true });
      items.push({
        label: `只看话题「${thrTitle.slice(0, 14)}」`,
        icon: '#ic-tasks',
        onSelect: () => setThreadFilter(meta.thread_id, thrTitle),
      });
    }
    openContextMenu({ x: ev.clientX, y: ev.clientY, items });
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
  renderStatusTab();
  return { render, setStage, onDelta, onTask, onMessageEvent, syncTasks, insertMention, setThreadFilter, clearThreadIfActive, renderEnginePill, flashIslandDone, switchEngine };
}
