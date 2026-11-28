// Right Muse Drawer (Tasks/Nodes, Crons/Audit, SOUL/MEMORY) + Settings Pane + Contacts.
import { AVATAR_CATALOG, api, esc, fmtTime, renderMarkdown, showToast } from './api.js';
import { renderBuddySvg } from './avatar.js';
import { playTap, playToggle } from './sound.js';
import { stripNoise, describeTask, cleanPreviewLine } from './triage.js';
import { COPY_BTN_HTML, bindHoldAction, initDelegatedCopy } from './ui.js';

export function initDrawer({ state, onRefresh, onMention, onPromptChat, onFilterThread }) {
  const bodyEl = document.getElementById('drawer-body');
  const settingsBody = document.getElementById('settings-body');
  const contactsList = document.getElementById('contacts-list');
  const tabBtns = Array.from(document.querySelectorAll('.muse-seg .tab'));
  initDelegatedCopy(bodyEl);
  initDelegatedCopy(settingsBody);

  let activeTab = 'tasks';
  let lastInstallCmd = '';
  let lastImBindCode = '';
  let integrations = { gitlab: null, im: null, models: null };
  const taskTimers = new Map();
  let elapsedTimer = null;

  tabBtns.forEach((btn, idx) => {
    btn.onclick = () => {
      if (activeTab === btn.dataset.tab) return;
      activeTab = btn.dataset.tab;
      if (btn.parentElement) btn.parentElement.dataset.activeIdx = String(idx);
      tabBtns.forEach((b) => {
        const on = b === btn;
        b.classList.toggle('on', on);
        b.setAttribute('aria-selected', String(on));
      });
      playToggle(true);
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
  const TARGET_LABEL = { cloud: '云端', 'cloud-node': '云端节点', auto: '自动节点' };
  const AUDIT_ACTION_LABEL = {
    node_create: '接入节点',
    node_update: '更新节点',
    node_submit: '节点完成',
    dispatch_node: '派发节点',
    task_elevate: '单次放行',
    task_callback: '自动跟进',
    task_digest: '提炼结论',
    cron_run: '定时执行',
    update_memory: '更新记忆',
    update_ledger: '更新事项',
    manage_role: '军团角色',
    context_compact: '压缩上下文',
    permissions: '修改授权',
    delegate: '发起协作',
    task_deliver: '交付结果',
    gitlab_config: 'GitLab 配置',
    gitlab_mention: 'GitLab 提及',
    im_config: 'IM 配置',
    im_bind: 'IM 绑定',
    member_status: '成员状态',
  };

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

  const NODE_ENGINES = ['claude', 'codex', 'agy', 'opencode'];

  function engineSelectOptions(selected = '') {
    return [
      `<option value=""${selected === '' ? ' selected' : ''}>默认：跟随节点</option>`,
      ...NODE_ENGINES.map(
        (ag) => `<option value="${ag}"${selected === ag ? ' selected' : ''}>${ag}</option>`,
      ),
    ].join('');
  }

  function engineCheckboxesHtml(selectedList = [], detectedList = []) {
    return NODE_ENGINES.map((ag) => {
      const checked = selectedList.includes(ag) ? ' checked' : '';
      const installed = detectedList.includes(ag) ? ' <span class="engine-installed">已装</span>' : '';
      return `<label class="engine-check"><input type="checkbox" name="engines" value="${ag}"${checked}><span>${ag}${installed}</span></label>`;
    }).join('');
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
    const repos = info.repos ? info.repos.split(',').map((s) => s.trim()).filter(Boolean) : [];
    if (repos.length) summary.push(`${repos.length} 个仓库`);
    const sb = (info.sandbox || '').toLowerCase();
    return {
      summary: summary.join(' · ') || (raw && !raw.includes(':') ? raw : ''),
      repos,
      sandbox: sb,
      isolation: (info.isolation || '').toLowerCase(),
      elevated: info.elevated === '1',
      codexNet: info.codex_net === '1',
    };
  }

  function rolesCardHtml(roles, { foldable = false, open = false } = {}) {
    const SANDBOX_LABEL = { readonly: '只读', 'workspace-write': '可写' };
    const rolesHtml = roles.length
      ? roles
          .map((r) => {
            const sbLabel = SANDBOX_LABEL[r.sandbox] || '默认沙箱';
            const sbCls = r.sandbox === 'readonly' ? 'ok' : '';
            const tags = [];
            tags.push(`<span class="engine-tag">${esc(r.agent || '节点默认')}</span>`);
            if (r.model) tags.push(`<span class="engine-tag is-default">${esc(r.model)}</span>`);
            if (r.effort) tags.push(`<span class="engine-tag">推理:${esc(r.effort)}</span>`);
            if (r.node_id) tags.push(`<span class="engine-tag">节点:${esc(r.node_id)}</span>`);
            const promptHtml = r.prompt
              ? `<details class="node-repos"><summary>查看角色规范</summary><div class="item-sub detail-body">${esc(r.prompt)}</div></details>`
              : '';
            return `
          <div class="item" data-role-name="${esc(r.name)}">
            <div class="item-head">
              <span>${esc(r.name)}</span>
              <span class="badge ${sbCls}">${esc(sbLabel)}</span>
            </div>
            ${r.description ? `<div class="item-sub">${esc(r.description)}</div>` : ''}
            <div class="engine-tags">${tags.join('')}</div>
            ${promptHtml}
            <div class="row">
              <button type="button" class="sm danger" data-del-role="${esc(r.name)}">删除</button>
            </div>
          </div>`;
          })
          .join('')
      : '<p class="note">尚未配置军团角色。在对话中直接说，例如：<br>「配置 planner 用 agy / claude-opus-5-5-high（只读规划），executor 用 codex / gpt-6.2-sol（可写执行）」</p>';

    const body = `
        ${rolesHtml}
        <div class="row">
          <button type="button" id="chat-tune-roles" class="sm primary">去对话调整</button>
        </div>`;

    if (foldable) {
      return `
      <details class="panel-card drawer-fold" id="roles-card" data-drawer-fold="roles"${open ? ' open' : ''}>
        <summary><span>我的军团 (${roles.length})</span><span class="drawer-fold-meta">对话配置</span></summary>
        ${body}
      </details>`;
    }

    return `
      <section class="panel-card" id="roles-card">
        <h3><span>我的军团 (${roles.length})</span><span class="note">对话配置</span></h3>
        ${body}
      </section>`;
  }

  function nodesCardHtml(nodes, { foldable = false, open = false } = {}) {
    const onlineCount = nodes.filter((n) => n.online).length;
    const nodesHtml = nodes.length
      ? nodes
          .map((n) => {
            const kindLabel = n.kind === 'cloud' ? '云端' : '本机';
            const parsed = parseCapabilities(n.capabilities);
            const engines =
              (n.effective_agents && n.effective_agents.length ? n.effective_agents : n.detected_agents) || [];
            const defEngine = n.effective_default_agent || n.default_agent || '';
            const enginesHtml = engines.length
              ? `<div class="engine-tags">${engines
                  .map(
                    (ag) =>
                      `<span class="engine-tag${ag === defEngine ? ' is-default' : ''}">${esc(ag)}${ag === defEngine ? ' · 默认' : ''}</span>`,
                  )
                  .join('')}</div>`
              : '';
            const iso = parsed.isolation;
            const sbTag =
              parsed.sandbox === 'unsafe-full' || iso === 'direct'
                ? '<span class="engine-tag is-warn">全宿主机权限</span>'
                : iso === 'bwrap' || iso === 'seatbelt'
                  ? `<span class="engine-tag">${parsed.sandbox === 'readonly' ? 'OS只读隔离' : 'OS工作区隔离'} (${esc(iso)})</span>`
                  : parsed.sandbox === 'readonly'
                    ? '<span class="engine-tag">仅只读</span>'
                    : parsed.sandbox
                      ? '<span class="engine-tag">CLI内置限制</span>'
                      : '';
            const elevTag = parsed.elevated ? '<span class="engine-tag">支持单次放行</span>' : '';
            const sandboxTagsHtml = sbTag || elevTag ? `<div class="engine-tags">${sbTag}${elevTag}</div>` : '';
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
            ${enginesHtml}
            ${sandboxTagsHtml}
            ${reposHtml}
            <details class="node-repos">
              <summary>配置引擎</summary>
              <form class="edit-node-form mt-6" data-node-id="${esc(n.id)}">
                <div class="engine-checks">${engineCheckboxesHtml(n.allowed_agents || [], n.detected_agents || [])}</div>
                <div class="note mt-6">不勾选则自动开放该节点上报的全部引擎</div>
                <div class="row engine-row">
                  <select name="default_agent" class="flex-1">${engineSelectOptions(n.default_agent || '')}</select>
                  <button type="submit" class="primary sm">保存配置</button>
                </div>
              </form>
            </details>
            <div class="row">
              <button type="button" class="sm" data-rotate="${esc(n.id)}">重置令牌</button>
              <button type="button" class="sm danger" data-rm-node="${esc(n.id)}">移除</button>
            </div>
          </div>`;
          })
          .join('')
      : '<p class="note">尚未接入执行节点</p>';

    const body = `
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
            <div class="engine-checks mt-6">${engineCheckboxesHtml([], [])}</div>
            <div class="row engine-row">
              <select name="default_agent" class="flex-1">${engineSelectOptions('')}</select>
            </div>
          </form>
        </details>
        ${lastInstallCmd ? `<div class="install-box">${COPY_BTN_HTML}<strong>一次性接入命令：</strong><br><code>${esc(lastInstallCmd)}</code></div>` : ''}`;

    if (foldable) {
      return `
      <details class="panel-card drawer-fold" data-drawer-fold="nodes"${open ? ' open' : ''}>
        <summary><span>我的节点 (${nodes.length})</span><span class="drawer-fold-meta">${onlineCount} 在线</span></summary>
        ${body}
      </details>`;
    }

    return `
      <section class="panel-card">
        <h3>我的节点 (${nodes.length})</h3>
        ${body}
      </section>`;
  }

  function bindNodeHandlers(root) {
    root.querySelectorAll('[data-rotate]').forEach((b) => {
      bindHoldAction(
        b,
        async () => {
          try {
            const r = await api(`/api/nodes/${b.dataset.rotate}/token`, { method: 'POST' });
            lastInstallCmd = `AGENCT_NODE_TOKEN=${r.token}`;
            showToast('节点令牌已重置');
            renderAll();
          } catch (err) {
            showToast(err.message);
          }
        },
        { shortHint: '请长按「重置令牌」确认操作' },
      );
    });
    root.querySelectorAll('[data-rm-node]').forEach((b) => {
      bindHoldAction(
        b,
        async () => {
          try {
            await api(`/api/nodes/${b.dataset.rmNode}`, { method: 'DELETE' });
            showToast('已移除节点');
            await onRefresh();
          } catch (err) {
            showToast(err.message);
          }
        },
        { shortHint: '请长按「移除」确认删除节点' },
      );
    });
    root.querySelectorAll('.edit-node-form').forEach((form) => {
      form.onsubmit = async (ev) => {
        ev.preventDefault();
        const fd = new FormData(form);
        const nodeId = form.dataset.nodeId;
        const allowed = fd.getAll('engines').map(String).filter(Boolean);
        const def = String(fd.get('default_agent') || '').trim();
        try {
          await api(`/api/nodes/${nodeId}`, {
            method: 'PUT',
            body: { allowed_agents: allowed, default_agent: def },
          });
          showToast('节点引擎配置已更新');
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
        const allowed = fd.getAll('engines').map(String).filter(Boolean);
        const def = String(fd.get('default_agent') || '').trim();
        try {
          const r = await api('/api/nodes', {
            method: 'POST',
            body: {
              name: fd.get('name'),
              kind: fd.get('kind'),
              allowed_agents: allowed,
              default_agent: def,
            },
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

  function updateTaskElapsed() {
    const now = performance.now();
    for (const el of bodyEl.querySelectorAll('[data-triage-elapsed]')) {
      const { elapsed, since } = taskTimers.get(el.dataset.triageElapsed);
      const seconds = Math.floor((elapsed ?? 0) + (now - since) / 1000);
      el.textContent = `${elapsed === null ? '已观察' : '已用时'} ${seconds} 秒`;
    }
  }

  function renderTasksTab() {
    const roles = state.roles || [];
    const threads = state.threads || [];
    const tasks = state.tasks || [];
    const crons = state.crons || [];
    const nodes = state.nodes || [];
    const elevatedNodeIds = new Set(nodes.filter((n) => parseCapabilities(n.capabilities).elevated).map((n) => n.id));
    const anyElevatedNode = elevatedNodeIds.size > 0;
    const uid = state.user?.id;
    const taskViews = new Map(tasks.map((task) => [task, describeTask(task)]));
    const openLogs = new Set(
      Array.from(bodyEl.querySelectorAll('[data-triage-log][open]'), (el) => el.dataset.triageLog),
    );
    const openFolds = new Set(
      Array.from(bodyEl.querySelectorAll('[data-drawer-fold][open]'), (el) => el.dataset.drawerFold),
    );
    const runningIds = new Set();

    function renderTaskProgress(task) {
      const view = taskViews.get(task)?.progress;
      if (!view) return '';
      const isRunning = task.status === 'running';
      const stage = formatStage(view.stage) || (isRunning
        ? (task.job_status === 'queued' ? '等待节点接单' : '执行中')
        : '');
      if (isRunning) {
        const timer = taskTimers.get(task.id);
        if (!timer || timer.revision !== task.revision || timer.jobStatus !== task.job_status || timer.elapsed !== view.elapsed) {
          taskTimers.set(task.id, {
            revision: task.revision, jobStatus: task.job_status, elapsed: view.elapsed, since: performance.now(),
          });
        }
        runningIds.add(task.id);
      }
      const elapsed = isRunning ? `<span data-triage-elapsed="${esc(task.id)}"></span>`
        : view.elapsed === null ? '' : `已用时 ${Math.floor(view.elapsed)} 秒`;
      const progress = [
        stage ? esc(stage) : '',
        elapsed,
      ].filter(Boolean).join(' · ');
      const logKey = `${task.id}:${task.revision ?? ''}`;
      const formattedLog = view.logTail
        ? (view.logTail.includes('\n') ? view.logTail.split(/\r?\n/) : view.logTail.split(/\s+\|\s+/))
            .map((s) => s.trim())
            .filter(Boolean)
            .filter((s, idx, arr) => idx === 0 || s !== arr[idx - 1])
            .join('\n')
        : '';
      const log = formattedLog
        ? `<details class="node-repos" data-triage-log="${esc(logKey)}"${openLogs.has(logKey) ? ' open' : ''}><summary>执行动态</summary><div class="item-sub detail-body">${esc(formattedLog)}</div></details>`
        : '';
      const links = view.links.map((url) => `<div class="item-sub"><a href="${esc(url)}" target="_blank" rel="noopener noreferrer">${esc(url)}</a></div>`).join('');
      return `${progress ? `<div class="item-sub">${progress}</div>` : ''}${log}${links}`;
    }

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

    function renderBlockedHtml(t) {
      if (!t) return '';
      if (t.status === 'pending' && t.sandbox === 'elevated') {
        return '<div class="sandbox-block-note">待确认单次放行：回复聊天中的 /approve 审批码或长按下方按钮后执行</div>';
      }
      if (t.status !== 'failed' || !t.blocked_reason) return '';
      return `<div class="sandbox-block-note">沙箱受阻：${esc(t.blocked_reason)}</div>`;
    }

    function taskActionsHtml(latest) {
      if (!latest) return [];
      const isOwner = latest.owner_id === uid;
      const isCrossUser = latest.requester_id !== latest.owner_id;
      const isPendingElev = latest.status === 'pending' && latest.sandbox === 'elevated';
      const actions = [];
      if (isOwner && !isPendingElev && (latest.status === 'pending' || latest.status === 'failed')) {
        actions.push(
          `<button type="button" class="sm primary" data-assign="${esc(latest.id)}">${latest.status === 'failed' ? '重试' : '派给节点'}</button>`,
        );
      }
      if (isOwner && !isCrossUser) {
        const canElevateNode = latest.node_id ? elevatedNodeIds.has(latest.node_id) : anyElevatedNode;
        if (isPendingElev && canElevateNode) {
          actions.push(`<button type="button" class="sm warn" data-elevate="${esc(latest.id)}">单次放行执行</button>`);
        } else if (latest.status === 'failed' && latest.blocked_reason && canElevateNode) {
          actions.push(`<button type="button" class="sm warn" data-elevate="${esc(latest.id)}">单次放行重试</button>`);
        }
      }
      if (isOwner && isCrossUser && latest.status !== 'done' && latest.status !== 'canceled') {
        actions.push(`<button type="button" class="sm" data-deliver="${esc(latest.id)}">交付结果</button>`);
      }
      if (latest.status === 'pending' || latest.status === 'running') {
        actions.push(`<button type="button" class="sm danger" data-cancel="${esc(latest.id)}">取消</button>`);
      }
      return actions;
    }

    function renderThreadCard(thr, { attention = false } = {}) {
      const runs = tasksByThread.get(thr.id) || [];
      const latest = runs[0] || null;
      const preview = taskViews.get(latest)?.preview || cleanPreviewLine(thr.summary, 72);
      const previewHtml = preview ? `<div class="item-sub">${esc(preview)}</div>` : '';
      const updatedAt = latest?.updated_at || thr.updated_at;

      if (!attention) {
        return `
        <div class="item thread-card" role="button" tabindex="0" data-filter-thread="${esc(thr.id)}" data-thread-title="${esc(thr.title)}">
          <div class="item-head">
            <span>${esc(thr.title)}</span>
            ${updatedAt ? `<span class="thread-time">${esc(fmtTime(updatedAt))}</span>` : ''}
          </div>
          ${previewHtml}
        </div>`;
      }

      const isRunning = runs.some((r) => r.status === 'running');
      const isPending = runs.some((r) => r.status === 'pending');
      const isPendingElev = latest?.status === 'pending' && latest?.sandbox === 'elevated';
      let badgeText = '失败';
      let badgeCls = '';
      if (isRunning) {
        badgeText = '执行中';
        badgeCls = 'warn';
      } else if (isPendingElev) {
        badgeText = '待放行';
        badgeCls = 'warn';
      } else if (isPending) {
        badgeText = '待处理';
        badgeCls = 'warn';
      }
      const actions = taskActionsHtml(latest);

      return `
        <div class="item thread-card" role="button" tabindex="0" data-filter-thread="${esc(thr.id)}" data-thread-title="${esc(thr.title)}">
          <div class="item-head">
            <span>${esc(thr.title)}</span>
            <span class="badge ${badgeCls}">${esc(badgeText)}</span>
          </div>
          ${renderBlockedHtml(latest)}
          ${previewHtml}
          ${renderTaskProgress(latest)}
          ${actions.length ? `<div class="row">${actions.join('')}</div>` : ''}
        </div>`;
    }

    const isAttentionTask = (t) => !!t && (t.status === 'running' || t.status === 'pending' || t.status === 'failed');

    const attentionCards = [];
    const attentionThreadIds = new Set();
    for (const thr of threads) {
      const latest = tasksByThread.get(thr.id)?.[0];
      if (isAttentionTask(latest)) {
        attentionThreadIds.add(thr.id);
        attentionCards.push(renderThreadCard(thr, { attention: true }));
      }
    }
    const idleThreads = threads.filter((thr) => !attentionThreadIds.has(thr.id));

    const renderCollabCard = (t) => {
      const stLabel = TASK_STATUS_LABEL[t.status] || t.status;
      const { preview } = taskViews.get(t) || { preview: '' };
      const actions = taskActionsHtml(t);
      return `
        <div class="item">
          <div class="item-head">
            <span>${esc(t.title)}</span>
            <span class="badge ${t.status === 'done' ? 'ok' : t.status === 'running' || t.status === 'pending' ? 'warn' : ''}">${esc(stLabel)}</span>
          </div>
          <div class="thread-meta">同事协作 · ${esc(fmtTime(t.updated_at))}</div>
          ${preview ? `<div class="item-sub">${esc(preview)}</div>` : ''}
          ${renderTaskProgress(t)}
          ${renderRunsDetails([t])}
          ${actions.length ? `<div class="row">${actions.join('')}</div>` : ''}
        </div>`;
    };

    const doneCollabCards = [];
    for (const t of standaloneCollab) {
      const card = renderCollabCard(t);
      if (isAttentionTask(t)) attentionCards.push(card);
      else doneCollabCards.push(card);
    }

    // Legacy unthreaded self-tasks grouped cleanly by title (eliminates duplicate cards).
    const doneLegacyCards = [];
    for (const [title, runs] of legacyGroups.entries()) {
      const latest = runs[0];
      const isRunning = latest.status === 'running';
      const stLabel = TASK_STATUS_LABEL[latest.status] || latest.status;
      const metaParts = [];
      if (latest.node_id) metaParts.push(`节点 ${latest.node_id}`);
      if (runs.length > 1) metaParts.push(`${runs.length} 次执行`);
      if (latest.updated_at) metaParts.push(fmtTime(latest.updated_at));
      const { preview } = taskViews.get(latest) || { preview: '' };
      const actions = taskActionsHtml(latest);
      const card = `
        <div class="item">
          <div class="item-head">
            <span>${esc(title)}</span>
            <span class="badge ${latest.status === 'done' ? 'ok' : isRunning ? 'warn' : ''}">${esc(stLabel)}</span>
          </div>
          ${metaParts.length ? `<div class="thread-meta">${esc(metaParts.join(' · '))}</div>` : ''}
          ${renderBlockedHtml(latest)}
          ${preview ? `<div class="item-sub">${esc(preview)}</div>` : ''}
          ${renderTaskProgress(latest)}
          ${renderRunsDetails(runs)}
          ${actions.length ? `<div class="row">${actions.join('')}</div>` : ''}
        </div>`;
      if (isAttentionTask(latest)) attentionCards.push(card);
      else doneLegacyCards.push(card);
    }

    const attentionHtml = attentionCards.length
      ? `<section class="panel-card workbench-live"><h3><span>待处理 / 执行中 (${attentionCards.length})</span><span class="note">实时状态</span></h3>${attentionCards.join('')}</section>`
      : '';

    const visibleThreads = idleThreads.slice(0, 5);
    const moreThreads = idleThreads.slice(5);
    const threadBodyParts = [];
    if (visibleThreads.length) {
      threadBodyParts.push(visibleThreads.map((thr) => renderThreadCard(thr)).join(''));
    } else if (!attentionCards.length) {
      threadBodyParts.push('<p class="note">暂无历史话题，在对话中派发任务或讨论会自动生成</p>');
    }
    if (moreThreads.length) {
      threadBodyParts.push(
        `<details class="node-repos mt-10"><summary>更早话题 (${moreThreads.length})</summary><div class="mt-6">${moreThreads.map((thr) => renderThreadCard(thr)).join('')}</div></details>`,
      );
    }
    const extraDoneCards = [...doneCollabCards, ...doneLegacyCards];
    if (extraDoneCards.length) {
      threadBodyParts.push(
        `<details class="node-repos mt-10"><summary>近期单次任务 (${extraDoneCards.length})</summary><div class="mt-6">${extraDoneCards.join('')}</div></details>`,
      );
    }
    const threadsSectionHtml = `
      <section class="panel-card">
        <h3><span>最近话题 (${threads.length})</span><span class="note">点击继续</span></h3>
        ${threadBodyParts.join('')}
      </section>`;

    const hasActiveOrFailedCron = crons.some((c) => c.last_status === 'failed' || c.last_status === 'running');
    const cronsOpen = openFolds.has('crons') || hasActiveOrFailedCron;
    const rolesOpen = openFolds.has('roles');
    const nodesOpen = openFolds.has('nodes') || nodes.length === 0 || Boolean(lastInstallCmd);

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
      ${attentionHtml}
      ${threadsSectionHtml}

      <details class="panel-card drawer-fold" data-drawer-fold="crons"${cronsOpen ? ' open' : ''}>
        <summary><span>定时任务 (${crons.length})</span><span class="drawer-fold-meta">${hasActiveOrFailedCron ? '需关注' : '自动运行'}</span></summary>
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
      </details>

      ${rolesCardHtml(roles, { foldable: true, open: rolesOpen })}

      ${nodesCardHtml(nodes, { foldable: true, open: nodesOpen })}`;

    for (const id of taskTimers.keys()) {
      if (!runningIds.has(id)) taskTimers.delete(id);
    }
    updateTaskElapsed();
    if (runningIds.size && elapsedTimer === null) elapsedTimer = setInterval(updateTaskElapsed, 1000);
    if (!runningIds.size) stopTaskElapsed();

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
    bodyEl.querySelectorAll('[data-elevate]').forEach((b) => {
      bindHoldAction(
        b,
        async () => {
          try {
            await api(`/api/tasks/${b.dataset.elevate}/action`, { method: 'POST', body: { action: 'retry_elevated' } });
            showToast('已批准单次放行');
            await onRefresh();
          } catch (err) {
            showToast(err.message);
          }
        },
        { shortHint: '请长按确认仅本次放行' },
      );
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
      bindHoldAction(
        b,
        async () => {
          try {
            await api(`/api/tasks/${b.dataset.cancel}/action`, { method: 'POST', body: { action: 'cancel' } });
            showToast('任务已取消');
            await onRefresh();
          } catch (err) {
            showToast(err.message);
          }
        },
        { shortHint: '请长按「取消」确认中止任务' },
      );
    });
    bodyEl.querySelectorAll('[data-filter-thread]').forEach((el) => {
      el.onclick = (ev) => {
        if (ev.target.closest('button, a, details, summary, input, select')) return;
        onFilterThread?.(el.dataset.filterThread, el.dataset.threadTitle);
      };
      el.onkeydown = (ev) => {
        if ((ev.key === 'Enter' || ev.key === ' ') && ev.target === el) {
          ev.preventDefault();
          onFilterThread?.(el.dataset.filterThread, el.dataset.threadTitle);
        }
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
    document.getElementById('chat-tune-roles')?.addEventListener('click', () => {
      onPromptChat?.('帮我配置军团角色：');
    });
    bodyEl.querySelectorAll('[data-del-role]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/roles/${encodeURIComponent(b.dataset.delRole)}`, { method: 'DELETE' });
          showToast('军团角色已删除');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
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
      const previewHtml = av.liveSvg
        ? `<span class="avatar-choice-buddy" data-agent-state="idle">${renderBuddySvg()}</span>`
        : `<img class="avatar-choice-img ${av.animated ? 'is-portrait' : ''}" src="${esc(av.src)}" alt="${esc(av.name)}" width="54" height="54" loading="lazy">`;
      const tagHtml = av.liveSvg
        ? '<span class="avatar-choice-tag">状态联动</span>'
        : av.animated
          ? '<span class="avatar-choice-tag">动态</span>'
          : '<span class="avatar-choice-tag muted">静态</span>';
      return `
        <button type="button" class="avatar-choice ${selected ? 'on' : ''}" data-pick-avatar="${esc(av.id)}" aria-pressed="${selected ? 'true' : 'false'}">
          <span class="avatar-choice-ring">${previewHtml}</span>
          <span class="avatar-choice-name">${esc(av.name)}</span>
          ${tagHtml}
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
        playTap();
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
      ${
        models.secrets_encrypted === false
          ? '<div class="sandbox-block-note mb-10">安全提示：服务器尚未配置 AGENCT_MASTER_KEY，用户自定义 API Key、GitLab PAT 与 IM Webhook 当前以明文存储在 SQLite 中。生产部署建议设置 AGENCT_MASTER_KEY 并运行 agenct-hub reencrypt-secrets。</div>'
          : ''
      }
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
        <h3>IM 随身双向接入（免公网端口）</h3>
        <p class="note">支持团队共享同一企微/钉钉机器人：每位成员用自己账号生成口令并给机器人发送 <code>/bind 口令</code>，发言时按成员身份自动路由到各自的 Agenct 空间、记忆与节点。</p>
        ${(() => {
          const wecom = im.wecom || {};
          const dd = im.dingtalk || {};
          const sharedWecom = Boolean(im.shared_wecom_online);
          const sharedDd = Boolean(im.shared_dingtalk_online);
          const hasOwnCreds = Boolean(wecom.configured || dd.configured);
          const showCredsOpen = hasOwnCreds || Boolean(state.user?.is_admin) || (!sharedWecom && !sharedDd);
          const statusBadge = (st, enabled, shared) => {
            if (enabled) {
              const s = st?.state || 'connecting';
              if (s === 'connected') return '<span class="badge ok">长连接在线</span>';
              if (s === 'error') return '<span class="badge">连接异常</span>';
              return '<span class="badge warn">连接中</span>';
            }
            return shared ? '<span class="badge ok">团队机器人在线</span>' : '<span class="badge">未接入</span>';
          };
          const bindNotice = lastImBindCode
            ? `<div class="sandbox-block-note mb-10">
                 <div>请用你的企业微信或钉钉（单聊或群聊 <code>@机器人</code>）发送下方配对口令（15 分钟内有效）：</div>
                 <div class="install-box mt-6"><code>/bind ${esc(lastImBindCode)}</code>${COPY_BTN_HTML}</div>
               </div>`
            : im.bind_pending
              ? '<div class="sandbox-block-note mb-10">已有生效中的配对口令待验证；若忘记可点击下方「重新生成口令」。</div>'
              : '';
          const credsInner = `
              <div class="item">
                <div class="item-head">
                  <strong>企业微信 · 智能机器人（API 长连接）</strong>
                  <div class="row mt-0">${statusBadge(wecom.status, wecom.enabled, sharedWecom)}</div>
                </div>
                ${wecom.status?.state === 'error' && wecom.status?.last_error ? `<p class="note cron-failed">${esc(wecom.status.last_error)}</p>` : ''}
                <label class="toggle"><span>启用企业微信长连接</span><input type="checkbox" name="wecom_enabled" ${wecom.enabled ? 'checked' : ''}></label>
                <label class="field-label">Bot ID</label>
                <input type="text" name="wecom_bot_id" value="${esc(wecom.bot_id || '')}" placeholder="工作台 → 智能机器人 → API 长连接 → Bot ID">
                <label class="field-label">Secret ${wecom.configured ? `(已保存 ${esc(wecom.secret_preview)})` : '(未配置)'}</label>
                <input type="password" name="wecom_secret" placeholder="${wecom.configured ? '留空保持不变' : '机器人 Secret'}" autocomplete="off">
              </div>

              <div class="item">
                <div class="item-head">
                  <strong>钉钉 · 企业内部机器人（Stream 模式）</strong>
                  <div class="row mt-0">${statusBadge(dd.status, dd.enabled, sharedDd)}</div>
                </div>
                ${dd.status?.state === 'error' && dd.status?.last_error ? `<p class="note cron-failed">${esc(dd.status.last_error)}</p>` : ''}
                <label class="toggle"><span>启用钉钉 Stream 长连接</span><input type="checkbox" name="dingtalk_enabled" ${dd.enabled ? 'checked' : ''}></label>
                <label class="field-label">Client ID（AppKey）</label>
                <input type="text" name="dingtalk_client_id" value="${esc(dd.client_id || '')}" placeholder="钉钉开放平台 → 企业内部应用 → AppKey">
                <label class="field-label">Client Secret（AppSecret）${dd.configured ? `(已保存 ${esc(dd.secret_preview)})` : '(未配置)'}</label>
                <input type="password" name="dingtalk_client_secret" placeholder="${dd.configured ? '留空保持不变' : 'AppSecret'}" autocomplete="off">
              </div>`;
          return `
            <div class="item mb-10">
              <div class="item-head">
                <strong>我的账号配对状态</strong>
                <div class="row mt-0">
                  ${wecom.bound_user_id ? `<span class="badge ok">企微 ${esc(wecom.bound_user_id)}</span>` : `<span class="badge ${sharedWecom ? 'warn' : ''}">${sharedWecom ? '企微待配对' : '企微未绑定'}</span>`}
                  ${dd.bound_staff_id ? `<span class="badge ok">钉钉 ${esc(dd.bound_staff_id)}</span>` : `<span class="badge ${sharedDd ? 'warn' : ''}">${sharedDd ? '钉钉待配对' : '钉钉未绑定'}</span>`}
                </div>
              </div>
              <div class="row">
                <button type="button" id="im-bind-btn" class="primary sm">${im.bind_pending ? '重新生成口令' : '生成配对口令'}</button>
                <button type="button" id="im-test-btn" class="sm">发送测试</button>
                ${wecom.bound_user_id ? '<button type="button" id="im-unbind-wecom" class="sm danger">解绑企微</button>' : ''}
                ${dd.bound_staff_id ? '<button type="button" id="im-unbind-dd" class="sm danger">解绑钉钉</button>' : ''}
              </div>
            </div>
            ${bindNotice}
            <form id="im-form">
              ${
                showCredsOpen
                  ? credsInner
                  : `<details class="mt-6"><summary>机器人连接凭证（管理员已接入团队机器人，成员无需填写，直接上方生成口令配对即可）</summary>${credsInner}</details>`
              }

              <details class="mt-6" ${im.configured ? 'open' : ''}>
                <summary>可选：群自定义机器人单向通知 Webhook</summary>
                <label class="field-label">平台</label>
                <select name="provider">
                  ${['wecom', 'dingtalk', 'feishu'].map((k) => `<option value="${k}" ${(im.provider || 'wecom') === k ? 'selected' : ''}>${k}</option>`).join('')}
                </select>
                <label class="field-label">Webhook URL ${im.configured ? '(已配置)' : '(未配置)'}</label>
                <input type="url" name="webhook_url" placeholder="${im.configured ? '留空保持不变' : 'https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=...'}">
                <label class="field-label">加签密钥（可选，钉钉/飞书群机器人）${im.webhook_secret_configured ? '(已保存)' : ''}</label>
                <input type="password" name="webhook_secret" placeholder="${im.webhook_secret_configured ? '留空保持不变' : 'SEC...'}" autocomplete="off">
                ${im.configured ? '<div class="row"><button type="button" id="im-clear-webhook" class="sm danger">清除群 Webhook</button></div>' : ''}
              </details>

              <label class="toggle mt-6"><span>收到同事协作请求时通知</span><input type="checkbox" name="notify_delegation" ${im.notify_delegation !== false ? 'checked' : ''}></label>
              <label class="toggle"><span>任务完成、单次放行或定时汇报时通知</span><input type="checkbox" name="notify_delivery" ${im.notify_delivery !== false ? 'checked' : ''}></label>

              <div class="row">
                <button type="submit" class="primary sm">保存配置</button>
              </div>
            </form>`;
        })()}
      </section>`;

    bindNodeHandlers(settingsBody);
    settingsBody.querySelectorAll('label.toggle input[type="checkbox"]').forEach((cb) => {
      cb.addEventListener('change', () => playToggle(cb.checked));
    });
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
    const clearBrainBtn = document.getElementById('models-clear-brain');
    bindHoldAction(clearBrainBtn, () => saveModels({ clear_brain: true }, '已恢复默认大脑模型'), {
      shortHint: '请长按「恢复默认大脑」确认清除',
    });
    const clearVisionBtn = document.getElementById('models-clear-vision');
    bindHoldAction(clearVisionBtn, () => saveModels({ clear_vision: true }, '已移除识图模型'), {
      shortHint: '请长按「移除识图模型」确认删除',
    });
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
    const saveImPayload = async (body, okMsg) => {
      try {
        const res = await api('/api/integrations/im', { method: 'PUT', body });
        if (res.bind_code) {
          lastImBindCode = res.bind_code;
          showToast(`已生成配对口令：/bind ${res.bind_code}`);
        } else {
          showToast(okMsg);
        }
        await loadIntegrations();
        renderSettingsPane();
      } catch (err) {
        showToast(err.message);
      }
    };
    document.getElementById('im-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const f = ev.target;
      const body = {
        wecom: {
          enabled: f.wecom_enabled.checked,
          bot_id: f.wecom_bot_id.value.trim(),
          secret: f.wecom_secret.value.trim() || null,
        },
        dingtalk: {
          enabled: f.dingtalk_enabled.checked,
          client_id: f.dingtalk_client_id.value.trim(),
          client_secret: f.dingtalk_client_secret.value.trim() || null,
        },
        provider: f.provider.value,
        webhook_url: f.webhook_url.value.trim() || null,
        webhook_secret: f.webhook_secret.value.trim() || null,
        notify_delegation: f.notify_delegation.checked,
        notify_delivery: f.notify_delivery.checked,
      };
      await saveImPayload(body, 'IM 配置已保存');
    };
    document.getElementById('im-bind-btn').onclick = async () => {
      await saveImPayload({ issue_bind_code: true }, '已生成配对口令');
    };
    document.getElementById('im-unbind-wecom')?.addEventListener('click', async () => {
      await saveImPayload({ wecom: { clear_bind: true }, issue_bind_code: true }, '已解绑企微账号');
    });
    document.getElementById('im-unbind-dd')?.addEventListener('click', async () => {
      await saveImPayload({ dingtalk: { clear_bind: true }, issue_bind_code: true }, '已解绑钉钉账号');
    });
    document.getElementById('im-clear-webhook')?.addEventListener('click', async () => {
      await saveImPayload({ clear_webhook: true, clear_webhook_secret: true }, '已清除群 Webhook');
    });
    document.getElementById('im-test-btn').onclick = async () => {
      try {
        const res = await api('/api/integrations/im/test', { method: 'POST' });
        const channels = (res.delivered || []).join('、');
        showToast(channels ? `测试消息已发送（${channels}）` : '测试消息已发送');
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
    else {
      stopTaskElapsed();
      renderSoulTab();
    }
  }

  function stopTaskElapsed() {
    if (elapsedTimer === null) return;
    clearInterval(elapsedTimer);
    elapsedTimer = null;
  }

  function renderAll() {
    renderDrawer();
    renderSettingsPane();
    renderContacts();
  }

  function openTab(tabName) {
    const idx = tabBtns.findIndex((b) => b.dataset.tab === tabName);
    if (idx < 0) return;
    activeTab = tabName;
    const seg = tabBtns[idx].parentElement;
    if (seg) seg.dataset.activeIdx = String(idx);
    tabBtns.forEach((b, i) => {
      const on = i === idx;
      b.classList.toggle('on', on);
      b.setAttribute('aria-selected', String(on));
    });
    renderDrawer();
  }

  return { render: renderAll, loadIntegrations, renderSettingsPane, openTab };
}
