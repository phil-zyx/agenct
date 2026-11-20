// Right Muse Drawer (Tasks/Nodes, Ledgers/Crons/Audit, SOUL/MEMORY) + Settings Pane + Contacts.
import { AVATAR_CATALOG, api, esc, fmtTime, renderMarkdown, showToast } from './api.js';

export function initDrawer({ state, onRefresh, onMention, onPromptChat }) {
  const bodyEl = document.getElementById('drawer-body');
  const settingsBody = document.getElementById('settings-body');
  const contactsList = document.getElementById('contacts-list');
  const tabBtns = Array.from(document.querySelectorAll('.muse-seg .tab'));

  let activeTab = 'tasks';
  let lastInstallCmd = '';
  let integrations = { gitlab: null, im: null, models: null };

  tabBtns.forEach((btn) => {
    btn.onclick = () => {
      activeTab = btn.dataset.tab;
      tabBtns.forEach((b) => {
        const on = b === btn;
        b.classList.toggle('on', on);
        b.setAttribute('aria-selected', String(on));
      });
      renderDrawer();
    };
  });

  async function loadIntegrations() {
    try {
      const [gitlab, im, models] = await Promise.all([
        api('/api/integrations/gitlab'),
        api('/api/integrations/im'),
        api('/api/settings/models'),
      ]);
      integrations = { gitlab, im, models };
    } catch {
      // ignore transient errors
    }
  }

  const TASK_STATUS_LABEL = {
    pending: '待处理',
    running: '执行中',
    done: '已完成',
    failed: '失败',
    canceled: '已取消',
  };
  const LEDGER_STATUS_LABEL = { active: '跟进中', muted: '已静默', done: '已完成' };
  const TARGET_LABEL = { cloud: '云端', 'cloud-node': '云端节点', auto: '自动节点' };
  const AUDIT_ACTION_LABEL = {
    node_create: '接入节点',
    node_submit: '节点完成',
    dispatch_node: '派发节点',
    cron_run: '定时执行',
    update_memory: '更新记忆',
    update_ledger: '更新事项',
    context_compact: '压缩上下文',
    permissions: '修改授权',
    delegate: '发起协作',
    task_deliver: '交付结果',
    gitlab_config: 'GitLab 配置',
    gitlab_mention: 'GitLab 提及',
    member_status: '成员状态',
  };

  function stripNoise(raw) {
    return String(raw || '')
      .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '')
      .replace(/\u25af\[\d+(?:;\d+)*m/g, '')
      .replace(/\r\n/g, '\n')
      .trim();
  }

  function cleanPreviewLine(raw, limit = 64) {
    const clean = stripNoise(raw);
    if (!clean) return '';
    const lines = clean
      .split('\n')
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith('```') && !/^[-*_]{3,}$/.test(l));
    const line = (lines[0] || clean.split('\n')[0] || '')
      .replace(/^#+\s*/, '')
      .replace(/^>\s*/, '')
      .replace(/\*\*/g, '')
      .trim();
    if (!line) return '';
    return line.length > limit ? `${line.slice(0, limit)}…` : line;
  }

  function formatStage(stage) {
    const s = String(stage || '').trim();
    if (!s || s === 'done' || s === 'failed') return '';
    if (s === 'Claimed by node') return '节点已接单';
    if (s === 'Starting execution') return '准备执行';
    if (s.startsWith('Running ')) return `${s.slice(8)} 执行中`;
    return s;
  }

  function compactTextHtml(text, limit = 60, moreLabel = '查看详情') {
    const clean = stripNoise(text);
    if (!clean) return '';
    const preview = cleanPreviewLine(clean, limit);
    if (clean.length <= limit && !clean.includes('\n')) {
      return `<div class="item-sub">${esc(preview || clean)}</div>`;
    }
    return `
      ${preview ? `<div class="item-sub">${esc(preview)}</div>` : ''}
      <details class="node-repos"><summary>${esc(moreLabel)}</summary><div class="item-sub detail-body">${esc(clean)}</div></details>`;
  }

  function parseCapabilities(raw) {
    const info = {};
    for (const part of String(raw || '').split('|')) {
      const idx = part.indexOf(':');
      if (idx <= 0) continue;
      const k = part.slice(0, idx).trim().toLowerCase();
      const v = part.slice(idx + 1).trim();
      if (k && v) info[k] = v;
    }
    const osMap = { linux: 'Linux', darwin: 'macOS', windows: 'Windows' };
    const summary = [];
    if (info.os) summary.push(osMap[info.os] || info.os);
    if (info.agent) summary.push(`引擎 ${info.agent}`);
    const repos = info.repos ? info.repos.split(',').map((s) => s.trim()).filter(Boolean) : [];
    if (repos.length) summary.push(`${repos.length} 个仓库`);
    return {
      summary: summary.join(' · ') || (raw && !raw.includes(':') ? raw : ''),
      repos,
    };
  }

  function nodesCardHtml(nodes) {
    const nodesHtml = nodes.length
      ? nodes
          .map((n) => {
            const kindLabel = n.kind === 'cloud' ? '云端' : '本机';
            const parsed = parseCapabilities(n.capabilities);
            const reposHtml = parsed.repos.length
              ? `<details class="node-repos"><summary>已发现仓库 (${parsed.repos.length})</summary><div class="repo-tags">${parsed.repos.map((r) => `<span class="repo-tag">${esc(r)}</span>`).join('')}</div></details>`
              : '';
            return `
          <div class="item">
            <div class="item-head">
              <span class="node-title">${esc(n.name)} <span class="badge">${kindLabel}</span></span>
              <span class="badge ${n.online ? 'ok' : ''}">${n.online ? '在线' : '离线'}</span>
            </div>
            ${parsed.summary ? `<div class="item-sub">${esc(parsed.summary)}</div>` : ''}
            ${reposHtml}
            <div class="row">
              <button type="button" class="sm" data-rotate="${esc(n.id)}">重置令牌</button>
              <button type="button" class="sm danger" data-rm-node="${esc(n.id)}">移除</button>
            </div>
          </div>`;
          })
          .join('')
      : '<p class="note">尚未接入执行节点</p>';

    return `
      <section class="panel-card">
        <h3>我的节点 (${nodes.length})</h3>
        ${nodesHtml}
        <details class="node-repos mt-10" ${nodes.length === 0 ? 'open' : ''}>
          <summary>+ 添加新节点</summary>
          <form class="add-node-form mt-6">
            <div class="row">
              <input type="text" name="name" placeholder="输入节点名称" required class="flex-1">
              <select name="kind" class="w-kind">
                <option value="pc">本机</option>
                <option value="cloud">云端</option>
              </select>
              <button type="submit" class="primary sm">添加</button>
            </div>
          </form>
        </details>
        ${lastInstallCmd ? `<div class="install-box"><strong>一次性接入命令：</strong><br><code>${esc(lastInstallCmd)}</code></div>` : ''}
      </section>`;
  }

  function bindNodeHandlers(root) {
    root.querySelectorAll('[data-rotate]').forEach((b) => {
      b.onclick = async () => {
        try {
          const r = await api(`/api/nodes/${b.dataset.rotate}/token`, { method: 'POST' });
          lastInstallCmd = `AGENCT_NODE_TOKEN=${r.token}`;
          showToast('节点令牌已重置');
          renderAll();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    root.querySelectorAll('[data-rm-node]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/nodes/${b.dataset.rmNode}`, { method: 'DELETE' });
          showToast('已移除节点');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    root.querySelectorAll('.add-node-form').forEach((form) => {
      form.onsubmit = async (ev) => {
        ev.preventDefault();
        const fd = new FormData(form);
        try {
          const r = await api('/api/nodes', {
            method: 'POST',
            body: { name: fd.get('name'), kind: fd.get('kind') },
          });
          lastInstallCmd = r.install_unix;
          showToast('节点已创建，请复制下方接入命令');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
  }

  function renderTasksTab() {
    const threads = state.threads || [];
    const tasks = state.tasks || [];
    const crons = state.crons || [];
    const ledgers = state.ledgers || [];
    const nodes = state.nodes || [];
    const uid = state.user?.id;

    // Group tasks by implicit thread_id; legacy unthreaded self-tasks collapse by title.
    const tasksByThread = new Map();
    const legacyGroups = new Map();
    const standaloneCollab = [];

    for (const t of tasks) {
      if (t.thread_id) {
        if (!tasksByThread.has(t.thread_id)) tasksByThread.set(t.thread_id, []);
        tasksByThread.get(t.thread_id).push(t);
      } else if (t.requester_id !== t.owner_id) {
        standaloneCollab.push(t);
      } else {
        const key = (t.title || t.id).trim();
        if (!legacyGroups.has(key)) legacyGroups.set(key, []);
        legacyGroups.get(key).push(t);
      }
    }

    function renderRunsDetails(runs) {
      if (!runs.length) return '';
      if (runs.length === 1) {
        const clean = stripNoise(runs[0].result);
        return clean
          ? `<details class="node-repos"><summary>查看结果</summary><div class="item-sub detail-body">${esc(clean)}</div></details>`
          : '';
      }
      const rows = runs
        .map((r) => {
          const st = TASK_STATUS_LABEL[r.status] || r.status;
          const res = stripNoise(r.result);
          const snippet = res ? `<div class="item-sub detail-body">${esc(res)}</div>` : '';
          return `
            <div class="run-entry">
              <div class="run-head">
                <span class="badge ${r.status === 'done' ? 'ok' : r.status === 'running' ? 'warn' : ''}">${esc(st)}</span>
                <span>${esc(fmtTime(r.updated_at))}</span>
              </div>
              ${snippet}
            </div>`;
        })
        .join('');
      return `<details class="node-repos"><summary>执行记录 (${runs.length})</summary>${rows}</details>`;
    }

    function taskActionsHtml(latest) {
      if (!latest) return [];
      const isOwner = latest.owner_id === uid;
      const isCrossUser = latest.requester_id !== latest.owner_id;
      const actions = [];
      if (isOwner && (latest.status === 'pending' || latest.status === 'failed')) {
        actions.push(
          `<button type="button" class="sm primary" data-assign="${esc(latest.id)}">${latest.status === 'failed' ? '重试' : '派给节点'}</button>`,
        );
      }
      if (isOwner && isCrossUser && latest.status !== 'done' && latest.status !== 'canceled') {
        actions.push(`<button type="button" class="sm" data-deliver="${esc(latest.id)}">交付结果</button>`);
      }
      if (latest.status === 'pending' || latest.status === 'running') {
        actions.push(`<button type="button" class="sm danger" data-cancel="${esc(latest.id)}">取消</button>`);
      }
      return actions;
    }

    function renderThreadCard(thr) {
      const runs = tasksByThread.get(thr.id) || [];
      const latest = runs[0] || null;
      const isRunning = runs.some((r) => r.status === 'running');
      const isPending = runs.some((r) => r.status === 'pending');

      let badgeText = thr.status === 'archived' ? '已归档' : '进行中';
      let badgeCls = thr.status === 'active' ? 'ok' : '';
      if (isRunning) {
        badgeText = '执行中';
        badgeCls = 'warn';
      } else if (isPending) {
        badgeText = '待处理';
        badgeCls = 'warn';
      } else if (latest?.status === 'failed' && thr.status === 'active') {
        badgeText = '失败';
        badgeCls = '';
      } else if (latest?.status === 'done' && thr.status === 'active') {
        badgeText = '已完成';
        badgeCls = 'ok';
      }

      const metaParts = [];
      if (thr.node_id) {
        metaParts.push(thr.has_session ? `节点 ${thr.node_id} · 可续接` : `节点 ${thr.node_id}`);
      }
      if (runs.length > 1) metaParts.push(`${runs.length} 次执行`);
      if (thr.updated_at) metaParts.push(fmtTime(thr.updated_at));
      const metaHtml = metaParts.length ? `<div class="thread-meta">${esc(metaParts.join(' · '))}</div>` : '';

      const stageText = isRunning && latest ? formatStage(latest.stage) : '';
      const progressHtml =
        isRunning && (stageText || latest?.log_tail)
          ? `<div class="item-sub">${esc(stageText || '执行中')}${latest?.log_tail ? `\n${esc(stripNoise(latest.log_tail))}` : ''}</div>`
          : '';

      const preview = cleanPreviewLine(latest?.result || thr.summary, 72);
      const previewHtml = !isRunning && preview ? `<div class="item-sub">${esc(preview)}</div>` : '';
      const detailsHtml = renderRunsDetails(runs);

      const actions = taskActionsHtml(latest);
      if (thr.status === 'active') {
        actions.push(`<button type="button" class="sm" data-archive-thread="${esc(thr.id)}">归档</button>`);
      } else {
        actions.push(`<button type="button" class="sm" data-restore-thread="${esc(thr.id)}">恢复</button>`);
        actions.push(`<button type="button" class="sm danger" data-del-thread="${esc(thr.id)}">删除</button>`);
      }

      return `
        <div class="item">
          <div class="item-head">
            <span>${esc(thr.title)}</span>
            <span class="badge ${badgeCls}">${esc(badgeText)}</span>
          </div>
          ${metaHtml}
          ${progressHtml}
          ${previewHtml}
          ${detailsHtml}
          ${actions.length ? `<div class="row">${actions.join('')}</div>` : ''}
        </div>`;
    }

    const activeThreads = threads.filter((t) => t.status === 'active');
    const archivedThreads = threads.filter((t) => t.status !== 'active');

    const activeCards = [];
    for (const thr of activeThreads) {
      activeCards.push(renderThreadCard(thr));
    }

    // Cross-user delegated tasks (not tied to a thread).
    for (const t of standaloneCollab) {
      const stLabel = TASK_STATUS_LABEL[t.status] || t.status;
      const stageText = t.status === 'running' ? formatStage(t.stage) : '';
      const progress =
        t.status === 'running' && (stageText || t.log_tail)
          ? `<div class="item-sub">${esc(stageText || '执行中')}${t.log_tail ? `\n${esc(stripNoise(t.log_tail))}` : ''}</div>`
          : '';
      const preview = cleanPreviewLine(t.result || t.instruction, 72);
      const actions = taskActionsHtml(t);
      activeCards.push(`
        <div class="item">
          <div class="item-head">
            <span>${esc(t.title)}</span>
            <span class="badge ${t.status === 'done' ? 'ok' : t.status === 'running' || t.status === 'pending' ? 'warn' : ''}">${esc(stLabel)}</span>
          </div>
          <div class="thread-meta">同事协作 · ${esc(fmtTime(t.updated_at))}</div>
          ${progress}
          ${preview ? `<div class="item-sub">${esc(preview)}</div>` : ''}
          ${renderRunsDetails([t])}
          ${actions.length ? `<div class="row">${actions.join('')}</div>` : ''}
        </div>`);
    }

    // Active ledgers unified into the same thread list.
    for (const l of ledgers) {
      activeCards.push(`
        <div class="item">
          <div class="item-head">
            <span>${esc(l.title)}</span>
            <span class="badge ${l.status === 'active' ? 'ok' : ''}">${esc(LEDGER_STATUS_LABEL[l.status] || l.status)}</span>
          </div>
          ${compactTextHtml(l.notes, 64, '查看全文')}
          <div class="row"><button type="button" class="sm danger" data-del-ledger="${esc(l.id)}">删除</button></div>
        </div>`);
    }

    // Legacy unthreaded self-tasks grouped cleanly by title (eliminates duplicate cards).
    for (const [title, runs] of legacyGroups.entries()) {
      const latest = runs[0];
      const isRunning = latest.status === 'running';
      const stLabel = TASK_STATUS_LABEL[latest.status] || latest.status;
      const metaParts = [];
      if (latest.node_id) metaParts.push(`节点 ${latest.node_id}`);
      if (runs.length > 1) metaParts.push(`${runs.length} 次执行`);
      if (latest.updated_at) metaParts.push(fmtTime(latest.updated_at));
      const stageText = isRunning ? formatStage(latest.stage) : '';
      const progress =
        isRunning && (stageText || latest.log_tail)
          ? `<div class="item-sub">${esc(stageText || '执行中')}${latest.log_tail ? `\n${esc(stripNoise(latest.log_tail))}` : ''}</div>`
          : '';
      const preview = !isRunning ? cleanPreviewLine(latest.result, 72) : '';
      const actions = taskActionsHtml(latest);
      activeCards.push(`
        <div class="item">
          <div class="item-head">
            <span>${esc(title)}</span>
            <span class="badge ${latest.status === 'done' ? 'ok' : isRunning ? 'warn' : ''}">${esc(stLabel)}</span>
          </div>
          ${metaParts.length ? `<div class="thread-meta">${esc(metaParts.join(' · '))}</div>` : ''}
          ${progress}
          ${preview ? `<div class="item-sub">${esc(preview)}</div>` : ''}
          ${renderRunsDetails(runs)}
          ${actions.length ? `<div class="row">${actions.join('')}</div>` : ''}
        </div>`);
    }

    const threadsHtml = activeCards.length
      ? activeCards.join('')
      : '<p class="note">暂无活跃工作线程，在对话中发起任务或跟进事项会自动生成</p>';

    const archivedHtml = archivedThreads.length
      ? `<details class="node-repos mt-10"><summary>已归档线程 (${archivedThreads.length})</summary><div class="mt-6">${archivedThreads.map(renderThreadCard).join('')}</div></details>`
      : '';

    const cronsHtml = crons.length
      ? crons
          .map((c) => {
            const when = c.daily_time_cst ? `每天 ${c.daily_time_cst}` : `每 ${c.interval_minutes} 分钟`;
            const target = TARGET_LABEL[c.target] || c.target;
            let runInfo = '';
            if (c.last_status === 'running') {
              runInfo = '<p class="note">执行中…</p>';
            } else if (c.last_status === 'ok') {
              runInfo = `<p class="note">上次执行成功 · ${esc(fmtTime(c.last_run_at))}</p>`;
            } else if (c.last_status === 'failed') {
              const retry = c.next_retry_at
                ? `，将于 ${esc(fmtTime(c.next_retry_at))} 重试`
                : '，已停止重试，等待下个周期';
              runInfo = `<p class="note cron-failed">连续失败 ${esc(String(c.fail_count))} 次${retry}：${esc(c.last_error)}</p>`;
            }
            return `
              <div class="item">
                <div class="item-head">
                  <span>${esc(when)}</span>
                  <span class="badge">${esc(target)}</span>
                </div>
                ${compactTextHtml(c.instruction, 46, '查看完整指令')}
                ${runInfo}
                <div class="row"><button type="button" class="sm danger" data-del-cron="${esc(c.id)}">删除</button></div>
              </div>`;
          })
          .join('')
      : '<p class="note">暂无定时任务，直接对话即可创建</p>';

    bodyEl.innerHTML = `
      <section class="panel-card">
        <h3>工作线程 (${activeCards.length})</h3>
        ${threadsHtml}
        ${archivedHtml}
      </section>

      <section class="panel-card">
        <h3>定时任务 (${crons.length})</h3>
        ${cronsHtml}
        <details class="node-repos mt-10">
          <summary>+ 手动添加定时</summary>
          <form id="add-cron-form" class="mt-6">
            <input type="text" name="instruction" placeholder="定时内容（如：汇总今日 GitLab 提交）" required>
            <div class="row">
              <input type="text" name="daily_time_cst" placeholder="每天 HH:MM" class="flex-1">
              <select name="target" class="w-kind">
                <option value="cloud">云端</option>
                <option value="cloud-node">云端节点</option>
                <option value="auto">自动</option>
              </select>
              <button type="submit" class="primary sm">添加</button>
            </div>
          </form>
        </details>
      </section>

      ${nodesCardHtml(nodes)}`;

    bodyEl.querySelectorAll('[data-assign]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/tasks/${b.dataset.assign}/action`, { method: 'POST', body: { action: 'assign_node' } });
          showToast('已派发给节点');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-deliver]').forEach((b) => {
      b.onclick = async () => {
        const result = window.prompt('输入交付结果内容：');
        if (!result?.trim()) return;
        try {
          await api(`/api/tasks/${b.dataset.deliver}/action`, {
            method: 'POST',
            body: { action: 'deliver', result: result.trim() },
          });
          showToast('已交付');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-cancel]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/tasks/${b.dataset.cancel}/action`, { method: 'POST', body: { action: 'cancel' } });
          showToast('任务已取消');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-archive-thread]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/threads/${b.dataset.archiveThread}`, { method: 'PUT', body: { status: 'archived' } });
          showToast('线程已归档');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-restore-thread]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/threads/${b.dataset.restoreThread}`, { method: 'PUT', body: { status: 'active' } });
          showToast('线程已恢复');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-del-thread]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/threads/${b.dataset.delThread}`, { method: 'DELETE' });
          showToast('线程已删除');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-del-ledger]').forEach((b) => {
      b.onclick = async () => {
        await api(`/api/ledgers/${b.dataset.delLedger}`, { method: 'DELETE' });
        await onRefresh();
      };
    });
    bodyEl.querySelectorAll('[data-del-cron]').forEach((b) => {
      b.onclick = async () => {
        await api(`/api/automation/crons/${b.dataset.delCron}`, { method: 'DELETE' });
        await onRefresh();
      };
    });
    document.getElementById('add-cron-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const fd = new FormData(ev.target);
      const daily = String(fd.get('daily_time_cst') || '').trim();
      await api('/api/automation/crons', {
        method: 'POST',
        body: {
          instruction: fd.get('instruction'),
          daily_time_cst: daily || null,
          interval_minutes: 60,
          target: fd.get('target'),
        },
      });
      showToast('定时任务已创建');
      await onRefresh();
    };
    bindNodeHandlers(bodyEl);
  }

  function stripMdHeading(raw) {
    return String(raw || '')
      .replace(/^#\s*(SOUL\.md|MEMORY\.md)[^\n]*\n?/i, '')
      .trim();
  }

  function renderSoulTab() {
    const p = state.profile || { name: 'Agenct', soul_md: '', memory_md: '', avatar_id: 'agenct', updated_at: 0 };
    const currentAvatar = p.avatar_id || 'agenct';
    const audits = state.audit_logs || [];
    const soulText = stripMdHeading(p.soul_md);
    const memText = stripMdHeading(p.memory_md);
    const updatedHint = p.updated_at ? `更新于 ${fmtTime(p.updated_at)}` : '系统自动维护';

    const avatarChoicesHtml = AVATAR_CATALOG.map((av) => {
      const selected = av.id === currentAvatar;
      return `
        <button type="button" class="avatar-choice ${selected ? 'on' : ''}" data-pick-avatar="${esc(av.id)}" aria-pressed="${selected ? 'true' : 'false'}">
          <span class="avatar-choice-ring">
            <img class="avatar-choice-img ${av.animated ? 'is-portrait' : ''}" src="${esc(av.src)}" alt="${esc(av.name)}" width="54" height="54" loading="lazy">
          </span>
          <span class="avatar-choice-name">${esc(av.name)}</span>
          ${av.animated ? '<span class="avatar-choice-tag">动态</span>' : '<span class="avatar-choice-tag muted">静态</span>'}
        </button>`;
    }).join('');

    bodyEl.innerHTML = `
      <section class="panel-card">
        <h3><span>形象与动态头像</span><span class="note">点选即刻生效</span></h3>
        <div class="avatar-grid">${avatarChoicesHtml}</div>
      </section>

      <section class="panel-card">
        <h3><span>行事风格</span><span class="note">${esc(updatedHint)}</span></h3>
        <div class="memory-prose">${renderMarkdown(soulText || '默认干练直接风格')}</div>
      </section>

      <section class="panel-card">
        <h3>长期记忆</h3>
        <div class="memory-prose">${renderMarkdown(memText || '暂无长期记忆，聊天中告诉我即可自动记住。')}</div>
        <p class="note mt-10">无需手填：在对话中直接说「以后叫你…」「记住…」即可由系统自动更新。</p>
        <div class="row">
          <button type="button" id="chat-tune-memory" class="sm primary">去对话调整</button>
        </div>
        <details class="node-repos mt-10">
          <summary>手动修正（高级）</summary>
          <form id="brain-form" class="mt-6">
            <label class="field-label">管家命名</label>
            <input type="text" name="name" value="${esc(p.name)}" maxlength="48" required>
            <label class="field-label">SOUL</label>
            <textarea name="soul_md" rows="4">${esc(p.soul_md)}</textarea>
            <label class="field-label">MEMORY</label>
            <textarea name="memory_md" rows="5">${esc(p.memory_md)}</textarea>
            <div class="row"><button type="submit" class="primary sm">保存修正</button></div>
          </form>
        </details>
      </section>

      <section class="panel-card">
        <h3>最近动态</h3>
        ${
          audits.length
            ? audits
                .slice(0, 10)
                .map((a) => {
                  const act = AUDIT_ACTION_LABEL[a.action] || a.action;
                  return `
              <div class="item">
                <div class="item-head"><span>${esc(a.title)}</span><span class="note">${esc(fmtTime(a.created_at))}</span></div>
                <div class="item-sub">${esc(act)}${a.detail ? ` · ${esc(a.detail.slice(0, 60))}` : ''}</div>
              </div>`;
                })
                .join('')
            : '<p class="note">暂无记录</p>'
        }
      </section>`;

    bodyEl.querySelectorAll('[data-pick-avatar]').forEach((btn) => {
      btn.onclick = async () => {
        const id = btn.dataset.pickAvatar;
        if (!id || id === currentAvatar) return;
        try {
          await api('/api/brain', { method: 'PUT', body: { avatar_id: id } });
          const picked = AVATAR_CATALOG.find((a) => a.id === id);
          showToast(`已切换头像：${picked?.name || id}`);
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });

    document.getElementById('chat-tune-memory').onclick = () => {
      if (onPromptChat) onPromptChat('记住：');
    };

    document.getElementById('brain-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const fd = new FormData(ev.target);
      await api('/api/brain', {
        method: 'PUT',
        body: { name: fd.get('name'), soul_md: fd.get('soul_md'), memory_md: fd.get('memory_md') },
      });
      showToast('已修正人设与记忆');
      await onRefresh();
    };
  }

  function renderSettingsPane() {
    const perms = state.permissions || {};
    const gl = integrations.gitlab || {};
    const im = integrations.im || {};
    const models = integrations.models || {};
    const def = models.server_default || {};
    const nodes = state.nodes || [];
    const modelFields = (slot, ep, placeholderModel) => `
          <label class="field-label">接口地址（OpenAI 兼容，以 /chat/completions 结尾）</label>
          <input type="url" name="${slot}_url" value="${esc(ep?.url || '')}" placeholder="https://api.deepseek.com/v1/chat/completions">
          <label class="field-label">模型名</label>
          <input type="text" name="${slot}_model" value="${esc(ep?.model || '')}" placeholder="${esc(placeholderModel)}">
          <label class="field-label">API Key ${ep?.key_preview ? `(已保存 ${esc(ep.key_preview)})` : '(未配置)'}</label>
          <input type="password" name="${slot}_key" placeholder="${ep ? '留空保持不变' : 'sk-...'}" autocomplete="off">`;

    settingsBody.innerHTML = `
      <section class="panel-card">
        <h3>大脑模型</h3>
        <p class="note">不配置则使用服务器默认：${esc(def.brain_model || '-')}${def.brain_supports_images ? '（支持图片）' : '（仅文字）'}${def.vision_model ? `，识图：${esc(def.vision_model)}` : ''}</p>
        <form id="models-form">
          ${modelFields('brain', models.brain, 'deepseek-chat / gpt-4o / qwen-vl-max')}
          <label class="toggle"><span>该模型支持图片输入（直接看图）</span><input type="checkbox" name="brain_images" ${models.brain?.supports_images ? 'checked' : ''}></label>
          <details class="mt-6" ${models.vision ? 'open' : ''}>
            <summary>识图模型（可选：大脑不能看图时，先把图片转成文字）</summary>
            ${modelFields('vision', models.vision, 'qwen-vl-plus / glm-4v-flash / gpt-4o-mini')}
          </details>
          <div class="row">
            <button type="submit" class="primary sm">保存</button>
            <button type="button" id="models-test-btn" class="sm">测试连通</button>
            ${models.brain ? '<button type="button" id="models-clear-brain" class="sm danger">恢复默认大脑</button>' : ''}
            ${models.vision ? '<button type="button" id="models-clear-vision" class="sm danger">移除识图模型</button>' : ''}
          </div>
        </form>
      </section>

      <section class="panel-card">
        <h3>自动化授权（默认全部关闭）</h3>
        <form id="perms-form">
          <label class="toggle"><span>自动接手同事协作并派发节点</span><input type="checkbox" name="auto_accept" ${perms.auto_accept ? 'checked' : ''}></label>
          <label class="toggle"><span>节点完成后自动交付给同事</span><input type="checkbox" name="auto_deliver" ${perms.auto_deliver ? 'checked' : ''}></label>
          <label class="toggle"><span>GitLab @提及后自动派发节点执行</span><input type="checkbox" name="gitlab_execute" ${perms.gitlab_execute ? 'checked' : ''}></label>
          <label class="toggle"><span>GitLab 执行结果自动评论回写</span><input type="checkbox" name="gitlab_reply" ${perms.gitlab_reply ? 'checked' : ''}></label>
          <div class="row"><button type="submit" class="primary sm">保存授权</button></div>
        </form>
      </section>

      ${nodesCardHtml(nodes)}

      <section class="panel-card">
        <h3>GitLab 接入</h3>
        <form id="gl-form">
          <label class="field-label">GitLab 实例（受服务器白名单限制）</label>
          <input type="url" name="base_url" value="${esc(gl.base_url || '')}">
          <label class="field-label">Personal Access Token ${gl.configured ? `(已保存 ${esc(gl.token_preview)})` : '(未配置)'}</label>
          <input type="password" name="token" placeholder="留空保持不变">
          <label class="toggle"><span>定时轮询 GitLab Todo</span><input type="checkbox" name="auto_poll_todos" ${gl.auto_poll_todos ? 'checked' : ''}></label>
          <div class="row"><button type="submit" class="primary sm">保存 GitLab 配置</button></div>
        </form>
      </section>

      <section class="panel-card">
        <h3>IM 通知通道（飞书 / 企微 / 钉钉）</h3>
        <form id="im-form">
          <label class="field-label">平台</label>
          <select name="provider">
            ${['feishu', 'wecom', 'dingtalk'].map((k) => `<option value="${k}" ${im.provider === k ? 'selected' : ''}>${k}</option>`).join('')}
          </select>
          <label class="field-label">Webhook URL ${im.configured ? '(已配置)' : '(未配置)'}</label>
          <input type="url" name="webhook_url" placeholder="https://open.feishu.cn/open-apis/bot/v2/hook/...">
          <div class="row">
            <button type="submit" class="primary sm">保存</button>
            <button type="button" id="im-test-btn" class="sm">发送测试</button>
          </div>
        </form>
      </section>`;

    bindNodeHandlers(settingsBody);
    const saveModels = async (body, okMsg) => {
      try {
        integrations.models = await api('/api/settings/models', { method: 'PUT', body });
        showToast(okMsg);
        renderSettingsPane();
      } catch (err) {
        showToast(err.message);
      }
    };
    document.getElementById('models-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const f = ev.target;
      const slot = (name, extra = {}) => {
        const url = f[`${name}_url`].value.trim();
        const model = f[`${name}_model`].value.trim();
        if (!url && !model) return undefined;
        return { url, model, api_key: f[`${name}_key`].value.trim() || null, ...extra };
      };
      const body = {};
      const brain = slot('brain', { supports_images: f.brain_images.checked });
      const vision = slot('vision');
      if (brain) body.brain = brain;
      if (vision) body.vision = vision;
      if (!brain && !vision) {
        showToast('请填写接口地址和模型名');
        return;
      }
      await saveModels(body, '模型配置已保存');
    };
    document.getElementById('models-clear-brain')?.addEventListener('click', () =>
      saveModels({ clear_brain: true }, '已恢复默认大脑模型'),
    );
    document.getElementById('models-clear-vision')?.addEventListener('click', () =>
      saveModels({ clear_vision: true }, '已移除识图模型'),
    );
    document.getElementById('models-test-btn').onclick = async () => {
      try {
        const r = await api('/api/settings/models/test', { method: 'POST' });
        const fmt = (label, x) =>
          x ? `${label} ${x.model}：${x.ok ? '连通 ✓' : `失败（${x.error}）`}` : '';
        showToast([fmt('大脑', r.brain), fmt('识图', r.vision)].filter(Boolean).join('；'));
      } catch (err) {
        showToast(err.message);
      }
    };
    document.getElementById('perms-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const f = ev.target;
      await api('/api/automation/permissions', {
        method: 'PUT',
        body: {
          auto_accept: f.auto_accept.checked,
          auto_deliver: f.auto_deliver.checked,
          gitlab_execute: f.gitlab_execute.checked,
          gitlab_reply: f.gitlab_reply.checked,
        },
      });
      showToast('自动化授权已更新');
      await onRefresh();
    };
    document.getElementById('gl-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const f = ev.target;
      try {
        await api('/api/integrations/gitlab', {
          method: 'PUT',
          body: {
            base_url: f.base_url.value.trim(),
            token: f.token.value.trim() || null,
            auto_poll_todos: f.auto_poll_todos.checked,
          },
        });
        showToast('GitLab 配置已保存');
        await loadIntegrations();
        renderSettingsPane();
      } catch (err) {
        showToast(err.message);
      }
    };
    document.getElementById('im-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const f = ev.target;
      try {
        const res = await api('/api/integrations/im', {
          method: 'PUT',
          body: {
            provider: f.provider.value,
            webhook_url: f.webhook_url.value.trim() || null,
          },
        });
        showToast(res.inbound_token ? `已保存（入站令牌：${res.inbound_token}）` : 'IM 配置已保存');
        await loadIntegrations();
        renderSettingsPane();
      } catch (err) {
        showToast(err.message);
      }
    };
    document.getElementById('im-test-btn').onclick = async () => {
      try {
        await api('/api/integrations/im/test', { method: 'POST' });
        showToast('测试消息已发送');
      } catch (err) {
        showToast(err.message);
      }
    };
  }

  function renderContacts() {
    const contacts = state.contacts || [];
    contactsList.innerHTML = contacts.length
      ? contacts
          .map(
            (c) => `
          <button type="button" class="contact-item" data-peer="${esc(c.username)}">
            <strong>${esc(c.display_name)}</strong>
            <span class="note">@${esc(c.username)} · 点击发起协作</span>
          </button>`,
          )
          .join('')
      : '<p class="note">暂无其他同事登录</p>';

    contactsList.querySelectorAll('[data-peer]').forEach((btn) => {
      btn.onclick = () => onMention(btn.dataset.peer);
    });
  }

  function renderDrawer() {
    if (activeTab === 'tasks') renderTasksTab();
    else renderSoulTab();
  }

  function renderAll() {
    renderDrawer();
    renderSettingsPane();
    renderContacts();
  }

  return { render: renderAll, loadIntegrations, renderSettingsPane };
}
