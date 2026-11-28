// Raycast-style frosted glass command palette: live FTS5 search + quick workspace actions.
import { api, esc, fmtTime } from './api.js';
import { playTap, playToggle } from './sound.js';

export function initCommandPalette({
  state,
  panelEl,
  inputEl,
  resultsEl,
  closeBtn,
  triggerBtn,
  actions = {},
}) {
  if (!panelEl || !inputEl || !resultsEl) {
    return { open() {}, close() {}, toggle() {}, isOpen: () => false };
  }

  let searchTimer = null;
  let reqSeq = 0;
  let selectedIdx = 0;
  let actionRunners = [];

  function primaryNode() {
    const nodes = state.nodes || [];
    return (
      nodes.find((n) => n.online && n.kind === 'pc') ||
      nodes.find((n) => n.online) ||
      nodes[0] ||
      null
    );
  }

  function buildQuickActions() {
    const list = [];
    const node = primaryNode();
    if (node) {
      const curDefault = node.effective_default_agent || node.default_agent || 'auto';
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
      for (const ag of candidates) {
        list.push({
          title: `切换默认引擎为 ${ag}`,
          sub: `${node.name} · ${node.online ? '在线' : '离线'}`,
          badge: ag === (node.default_agent || curDefault) ? '当前' : node.name,
          icon: '#ic-bolt',
          keywords: `engine switch 切换引擎 ${ag} ${node.name}`,
          run: () => actions.onSwitchEngine?.(node, ag),
        });
      }
    }

    list.push({
      title: '切换交互音效',
      sub: `当前：${actions.getSoundLabel?.() || '柔和'}`,
      badge: '音效',
      icon: '#ic-sound',
      keywords: 'sound audio mute 音效 声音',
      run: () => actions.onCycleSound?.(),
    });
    list.push({
      title: '切换管家形象',
      sub: '打开形象选择器（动态管家 / 樱粉 / 和风 / 猫耳）',
      badge: '形象',
      icon: '#ic-avatar',
      keywords: 'avatar 头像 形象 buddy 管家',
      run: () => actions.onOpenAvatarPicker?.(),
    });
    list.push({
      title: '修改管家名字',
      sub: `当前名字：${state.profile?.name || 'Agenct'}`,
      badge: '命名',
      icon: '#ic-edit',
      keywords: 'rename name 名字 改名 命名 昵称',
      run: () => actions.onRenameAgent?.(),
    });
    list.push({
      title: actions.isTasksOpen?.() ? '收起管家工作台' : '打开管家工作台',
      sub: '查看运行中任务、工作线程与长期记忆',
      badge: '工作台',
      icon: '#ic-tasks',
      keywords: 'tasks drawer workspace 工作台 任务 线程',
      run: () => actions.onToggleTasks?.(),
    });
    list.push({
      title: actions.isContactsOpen?.() ? '收起联系人侧栏' : '打开联系人侧栏',
      sub: '与团队同事发起 @ 协作',
      badge: '协作',
      icon: '#ic-contacts',
      keywords: 'contacts 联系人 协作 同事',
      run: () => actions.onToggleContacts?.(),
    });
    list.push({
      title: '打开账号与设置',
      sub: '管理模型接口、节点、自动化规则与定时任务',
      badge: '设置',
      icon: '#ic-settings',
      keywords: 'settings config 模型 节点 gitlab 定时 cron 设置',
      run: () => actions.onOpenSettings?.(),
    });
    return list;
  }

  function renderCmdItem(idx, { icon, title, sub, badge }) {
    return `
      <button type="button" class="cmd-item${idx === selectedIdx ? ' is-selected' : ''}" data-cmd-idx="${idx}" role="option" aria-selected="${idx === selectedIdx ? 'true' : 'false'}">
        <span class="cmd-item-ic"><svg class="ic" viewBox="0 0 24 24" aria-hidden="true"><use href="${esc(icon)}"/></svg></span>
        <span class="cmd-item-body">
          <span class="cmd-item-title">${esc(title)}</span>
          ${sub ? `<span class="cmd-item-sub">${esc(sub)}</span>` : ''}
        </span>
        ${badge ? `<span class="badge">${esc(badge)}</span>` : ''}
      </button>`;
  }

  function syncSelection(scroll = false) {
    const items = Array.from(resultsEl.querySelectorAll('.cmd-item'));
    if (!items.length) {
      selectedIdx = -1;
      return;
    }
    if (selectedIdx < 0 || selectedIdx >= items.length) selectedIdx = 0;
    items.forEach((el, i) => {
      const on = i === selectedIdx;
      el.classList.toggle('is-selected', on);
      el.setAttribute('aria-selected', String(on));
      if (on && scroll) el.scrollIntoView({ block: 'nearest' });
    });
  }

  function renderEntries(groups) {
    actionRunners = [];
    const parts = [];
    for (const g of groups) {
      if (!g.items.length) continue;
      parts.push(`<div class="cmd-group-title">${esc(g.label)}</div>`);
      for (const item of g.items) {
        const idx = actionRunners.length;
        actionRunners.push(item.run || null);
        parts.push(renderCmdItem(idx, item));
      }
    }
    selectedIdx = actionRunners.length ? 0 : -1;
    resultsEl.innerHTML = parts.length ? parts.join('') : '<p class="cmd-empty note">未找到匹配指令或记录</p>';
    syncSelection(false);
  }

  function filterLocalGroups(rawQuery) {
    const trimmed = rawQuery.trim();
    const actionOnly = trimmed.startsWith('>');
    const q = (actionOnly ? trimmed.slice(1) : trimmed).trim().toLowerCase();
    const allActions = buildQuickActions();
    const matchedActions = q
      ? allActions.filter(
          (a) =>
            a.title.toLowerCase().includes(q) ||
            a.sub.toLowerCase().includes(q) ||
            a.keywords.toLowerCase().includes(q),
        )
      : allActions;

    const threads = (state.threads || [])
      .filter((t) => !q || (t.title || '').toLowerCase().includes(q))
      .slice(0, q ? 5 : 4)
      .map((t) => ({
        title: t.title || t.id,
        sub: '工作线程 · 筛选对话上下文',
        badge: t.status === 'active' ? '进行中' : t.status || '线程',
        icon: '#ic-chat',
        run: () => {
          close(true);
          actions.onOpenThread?.(t.id, t.title || t.id);
        },
      }));

    const groups = [
      { label: '快捷指令', items: matchedActions.slice(0, q ? 8 : 6) },
      { label: q ? '匹配工作线程' : '最近工作线程', items: threads },
    ];
    return { groups, trimmed, actionOnly };
  }

  async function runQuery(rawQuery) {
    const seq = ++reqSeq;
    const { groups, trimmed, actionOnly } = filterLocalGroups(rawQuery);
    renderEntries(groups);
    if (!trimmed || actionOnly) return;

    const searchQ = trimmed;
    try {
      const res = await api(`/api/search?q=${encodeURIComponent(searchQ)}`);
      if (seq !== reqSeq || isOpen() === false) return;

      const remoteGroups = [];
      const msgs = res.messages || [];
      if (msgs.length) {
        remoteGroups.push({
          label: '对话记录',
          items: msgs.map((m) => {
            const thrTitle =
              (state.threads || []).find((t) => t.id === m.thread_id)?.title || m.thread_id || '';
            return {
              title: m.snippet || '',
              sub: `${m.role === 'user' ? '我' : 'Agenct'} · ${fmtTime(m.created_at)}`,
              badge: thrTitle,
              icon: '#ic-chat',
              run: m.thread_id
                ? () => {
                    close(true);
                    actions.onOpenThread?.(m.thread_id, thrTitle);
                  }
                : () => close(true),
            };
          }),
        });
      }
      const mems = res.memory_matches || [];
      if (mems.length) {
        remoteGroups.push({
          label: '长期记忆',
          items: mems.map((line) => ({
            title: line,
            sub: 'MEMORY.md',
            badge: '记忆',
            icon: '#ic-avatar',
            run: () => {
              close(true);
              actions.onOpenAvatarPicker?.();
            },
          })),
        });
      }
      renderEntries([...groups, ...remoteGroups]);
    } catch (err) {
      if (seq !== reqSeq) return;
      renderEntries(groups);
      resultsEl.insertAdjacentHTML('beforeend', `<p class="cmd-empty note">搜索失败：${esc(err.message)}</p>`);
    }
  }

  function isOpen() {
    return !panelEl.classList.contains('hide');
  }

  function open() {
    const wasOpen = isOpen();
    panelEl.classList.remove('hide');
    triggerBtn?.classList.add('active');
    triggerBtn?.setAttribute('aria-expanded', 'true');
    actions.onBeforeOpen?.();
    if (!wasOpen) playToggle(true);
    actions.onSyncRail?.();
    runQuery(inputEl.value);
    requestAnimationFrame(() => {
      inputEl.focus();
      inputEl.select();
    });
  }

  function close(silent = false) {
    const wasOpen = isOpen();
    clearTimeout(searchTimer);
    reqSeq++;
    panelEl.classList.add('hide');
    triggerBtn?.classList.remove('active');
    triggerBtn?.setAttribute('aria-expanded', 'false');
    if (wasOpen && !silent) playToggle(false);
    actions.onSyncRail?.();
  }

  function toggle() {
    if (isOpen()) close();
    else open();
  }

  resultsEl.addEventListener('click', (ev) => {
    const btn = ev.target?.closest?.('.cmd-item');
    if (!btn) return;
    const idx = Number(btn.dataset.cmdIdx);
    const runner = actionRunners[idx];
    if (runner) {
      playTap();
      runner();
    }
  });

  resultsEl.addEventListener('pointermove', (ev) => {
    const btn = ev.target?.closest?.('.cmd-item');
    if (!btn) return;
    const idx = Number(btn.dataset.cmdIdx);
    if (Number.isFinite(idx) && idx !== selectedIdx) {
      selectedIdx = idx;
      syncSelection(false);
    }
  });

  panelEl.addEventListener('click', (ev) => {
    if (ev.target === panelEl) close();
  });

  closeBtn?.addEventListener('click', () => close());
  triggerBtn?.addEventListener('click', () => toggle());

  inputEl.addEventListener('input', () => {
    clearTimeout(searchTimer);
    const { groups, trimmed, actionOnly } = filterLocalGroups(inputEl.value);
    renderEntries(groups);
    if (!trimmed || actionOnly) return;
    searchTimer = setTimeout(() => runQuery(inputEl.value), 180);
  });

  inputEl.addEventListener('keydown', (ev) => {
    if (ev.key === 'ArrowDown') {
      ev.preventDefault();
      const count = actionRunners.length;
      if (count) {
        selectedIdx = (selectedIdx + 1) % count;
        syncSelection(true);
      }
    } else if (ev.key === 'ArrowUp') {
      ev.preventDefault();
      const count = actionRunners.length;
      if (count) {
        selectedIdx = (selectedIdx - 1 + count) % count;
        syncSelection(true);
      }
    } else if (ev.key === 'Enter' && !ev.isComposing) {
      const activeEl = resultsEl.querySelector(`.cmd-item[data-cmd-idx="${selectedIdx}"]`);
      if (activeEl) {
        ev.preventDefault();
        activeEl.click();
      }
    }
  });

  return { open, close, toggle, isOpen };
}
