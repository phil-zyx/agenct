/* Shared work-index presentation; no writes. */
const peopleNames = new Map();
function rememberPerson(id, displayName) {
  if (typeof id !== 'string' || !id) return false;
  const trimmed = typeof displayName === 'string' ? displayName.trim() : '';
  const previous = peopleNames.get(id) || '';
  if (!trimmed || trimmed === id) {
    if (!previous) return false;
    peopleNames.delete(id);
    return true;
  }
  if (previous === trimmed) return false;
  peopleNames.set(id, trimmed);
  return true;
}
function clearPeopleNames() { peopleNames.clear(); }
function memberLabel(id) {
  if (typeof id !== 'string' || !id) return '';
  const name = peopleNames.get(id);
  return name && name !== id ? `${name}（${id}）` : id;
}

// Server times are UTC "YYYY-MM-DD HH:MM:SS"; anything else is treated as unknown.
function parseUtc(value) {
  if (typeof value !== 'string' || !/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(value)) return null;
  const date = new Date(value.replace(' ', 'T') + 'Z');
  return Number.isNaN(date.getTime()) ? null : date;
}
const pad = n => String(n).padStart(2, '0');
function dayLabel(date) {
  const today = new Date();
  const start = d => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((start(today) - start(date)) / 86400000);
  if (days === 0) return '今天';
  if (days === 1) return '昨天';
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}
const clock = date => `${pad(date.getHours())}:${pad(date.getMinutes())}`;
const formatWhen = date => (dayLabel(date) === '今天' ? '' : dayLabel(date) + ' ') + clock(date);

function activityIcon(state) {
  const icon = el('span', 'activity-icon');
  const make = tag => document.createElementNS ? document.createElementNS('http://www.w3.org/2000/svg', tag) : document.createElement(tag);
  const svg = make('svg'); svg.setAttribute('viewBox', '0 0 24 24'); svg.setAttribute('aria-hidden', 'true');
  const circle = make('circle'); circle.setAttribute('cx', '12'); circle.setAttribute('cy', '12'); circle.setAttribute('r', '9'); svg.append(circle);
  const path = make('path');
  path.setAttribute('d', ['completed', 'confirmed'].includes(state) ? 'm7 12 3 3 7-7'
    : ['canceled', 'rejected'].includes(state) ? 'm8 8 8 8M16 8l-8 8'
    : state === 'working' ? 'M12 7v5l3 2'
    : state === 'ready_for_acceptance' ? 'M8 9h8M8 13h8M8 17h4' : 'M8 12h8');
  svg.append(path); icon.append(svg); return icon;
}
function activityLine(item, me) {
  const l = item.latest || {};
  const who = l.actor === me ? '你' : memberLabel(l.actor) + ' ';
  const counterpart = memberLabel(item.counterpart);
  switch (l.action) {
    case 'proposal_pending': return `等你确认 · 委派给 ${counterpart}`;
    case 'proposal_rejected': return '已拒绝，未共享';
    case 'created': return item.role === 'owner' ? `${counterpart} 委派给你，待接手` : `已委派给 ${counterpart}，等对方接手`;
    case 'accept': return `${who}已接手`;
    case 'deliver': return `${who}提交了交付内容`;
    case 'approve': return `${who}验收通过`;
    case 'return': return `${who}退回了交付内容`;
    case 'cancel': return `${who}取消了任务` + (l.reason ? `：${l.reason}` : '');
    case 'draft_complete': return '草稿已写好，等你审阅（仅你可见）';
    case 'draft_uncertain': return '暂时无法确认起草结果，请打开草稿查看';
    case 'draft_pending': return '正在为你起草…';
    case 'topic_turn': {
      const task = typeof visibleTasks !== 'undefined' ? visibleTasks.get(item.id) : null;
      const turns = Array.isArray(task?.recent_turns) ? task.recent_turns : [];
      const count = Number.isFinite(task?.turn_count) ? task.turn_count : turns.length;
      const last = turns.length ? turns[turns.length - 1] : null;
      const raw = String(last?.answer || last?.question || task?.result || '')
        .replace(/^🤵\s*管家（[^）]*）：\s*/u, '')
        .replace(/[#*`>-]+/g, ' ')
        .replace(/\s+/g, ' ')
        .trim();
      const preview = raw.length > 52 ? raw.slice(0, 52) + '…' : raw;
      if (preview) return preview;
      if (count > 0) return `共 ${count} 轮对话`;
      return '自动归档的上下文脉络';
    }
    default: return String(l.action || '');
  }
}
function flash(element) {
  if (!element) return;
  element.scrollIntoView?.({ block: 'center', behavior: 'smooth' });
  element.classList.remove('flash');
  void element.offsetWidth;
  element.classList.add('flash');
}
const collapsedActivityDays = new Set();
let activeMusePaneTab = 'activity';
let museCronsCollapsed = false;
let museShowAllCrons = false;
let lastMuseAutomation = null;

function cleanTopicTitle(rawTitle) {
  return String(rawTitle || '')
    .replace(/^\[归并:[^\]]+\]\s*/u, '')
    .trim();
}

function renderActivity(context, items) {
  const groups = [];
  for (const item of Array.isArray(items) ? items : []) {
    if (!item || typeof item.id !== 'string') continue;
    const at = parseUtc(item.latest?.at);
    const label = at ? dayLabel(at) : '更早';
    if (!groups.length || groups[groups.length - 1].label !== label) groups.push({ label, items: [] });
    groups[groups.length - 1].items.push({ item, at });
  }
  const nodes = [];
  for (const group of groups) {
    const dayHead = el('h3', 'day' + (collapsedActivityDays.has(group.label) ? ' collapsed' : ''), group.label);
    dayHead.onclick = () => {
      if (collapsedActivityDays.has(group.label)) collapsedActivityDays.delete(group.label);
      else collapsedActivityDays.add(group.label);
      renderActivity(context, items);
    };
    nodes.push(dayHead);
    if (collapsedActivityDays.has(group.label)) continue;
    for (const { item, at } of group.items) {
      const row = el('button', 'activity-item status-' + item.status
        + (typeof activeMuseTopicId !== 'undefined' && activeMuseTopicId === item.id ? ' topic-active' : ''));
      row.type = 'button';
      const icon = activityIcon(item.status);
      const body = el('span', 'activity-body');
      body.append(
        el('span', 'activity-title', cleanTopicTitle(item.title)),
        el('span', 'activity-line', activityLine(item, context.user)),
        el('span', 'activity-time', at ? clock(at) : '')
      );
      row.append(icon, body);
      row.onclick = () => {
        if (!current(context)) return;
        if (item.kind === 'task') {
          const task = typeof visibleTasks !== 'undefined' ? visibleTasks.get(item.id) : null;
          if (task?.is_topic && typeof selectMuseTopic === 'function') {
            selectMuseTopic(context, task);
            closeContacts(); closePeerConversation();
            returnToChatOnNarrowScreen();
          } else {
            setPaneTab('tasks');
            revealTask(item.id);
          }
        } else {
          closeContacts(); closePeerConversation();
          returnToChatOnNarrowScreen();
          const reveal = node => {
            if (node.proposalId === item.id) return node;
            for (const child of node.children) {
              const target = reveal(child);
              if (target) {
                if (node.tagName?.toLowerCase() === 'details') node.open = true;
                return target;
              }
            }
          };
          const target = reveal($('conversation'));
          if (target) { target.tabIndex = -1; target.focus?.(); flash(target); }
        }
      };
      nodes.push(row);
    }
  }
  $('activity').replaceChildren(...nodes);
}

function setPaneTab(tab) {
  activeMusePaneTab = ['activity', 'tasks', 'soul'].includes(tab) ? tab : 'activity';
  const isActivity = activeMusePaneTab === 'activity';
  const isTasks = activeMusePaneTab === 'tasks';
  const isSoul = activeMusePaneTab === 'soul';
  $('activity').classList[isActivity ? 'remove' : 'add']('hide');
  const schedBox = $('muse-scheduled-box');
  if (schedBox) {
    const hasCrons = Array.isArray(lastMuseAutomation?.crons) && lastMuseAutomation.crons.length > 0;
    schedBox.classList[isActivity && hasCrons ? 'remove' : 'add']('hide');
  }
  const auditBox = $('muse-audit-box');
  if (auditBox) auditBox.classList[isTasks ? 'remove' : 'add']('hide');
  $('tasks').classList[isTasks ? 'remove' : 'add']('hide');
  $('task-filters').classList[isTasks ? 'remove' : 'add']('hide');
  const soulPanel = $('muse-soul-panel');
  if (soulPanel) soulPanel.classList[isSoul ? 'remove' : 'add']('hide');

  $('tab-activity').classList[isActivity ? 'add' : 'remove']('on');
  $('tab-tasks').classList[isTasks ? 'add' : 'remove']('on');
  const tabSoul = $('tab-soul');
  if (tabSoul) {
    tabSoul.classList[isSoul ? 'add' : 'remove']('on');
    tabSoul.setAttribute('aria-selected', String(isSoul));
    tabSoul.tabIndex = isSoul ? 0 : -1;
  }
  $('tab-activity').setAttribute('aria-selected', String(isActivity));
  $('tab-tasks').setAttribute('aria-selected', String(isTasks));
  $('tab-activity').tabIndex = isActivity ? 0 : -1;
  $('tab-tasks').tabIndex = isTasks ? 0 : -1;
}

function formatCronShortTitle(instruction) {
  const s = String(instruction || '').trim();
  if (/gitlab/i.test(s) && /提交|汇总|总结/.test(s)) return 'GitLab 每日提交汇总';
  if (/早报/.test(s)) return '每日早报';
  if (/巡检|状态|heartbeat/i.test(s)) return 'Heartbeat 系统巡检';
  return s.length > 22 ? s.slice(0, 22) + '…' : (s || '定时任务');
}

function renderMuseDrawer(context, automation, proposals, tasks) {
  if (automation && typeof automation === 'object' && !Array.isArray(automation)) {
    lastMuseAutomation = automation;
  }
  const auto = lastMuseAutomation || { rules: [], crons: [], muse: null };
  const museName = (auto.muse?.name || 'Muse').trim() || 'Muse';
  const drawerName = $('muse-drawer-name');
  if (drawerName) drawerName.textContent = museName;
  const avatarName = $('muse-avatar-name');
  if (avatarName) avatarName.textContent = museName;

  const connText = $('muse-conn-text');
  const connStatus = $('muse-conn-status');
  if (connText && connStatus) {
    const onlineNode = Array.isArray(userNodes) ? userNodes.find(n => n.online) : null;
    connText.textContent = onlineNode ? '已连接' : '已连接';
    connStatus.title = onlineNode ? `已连接节点：${onlineNode.name || onlineNode.id}` : '云端 Muse 已连接';
  }

  // 1. Tab 1 top: Scheduled Tasks (预定任务)
  const schedBox = $('muse-scheduled-box');
  const crons = Array.isArray(auto.crons) ? auto.crons : [];
  if (schedBox) {
    schedBox.replaceChildren();
    if (!crons.length || activeMusePaneTab !== 'activity') {
      schedBox.classList.add('hide');
    } else {
      schedBox.classList.remove('hide');
      const headBtn = el('button', 'muse-sec-head');
      headBtn.type = 'button';
      headBtn.append(
        el('span', 'muse-sec-title', `${crons.length}项预定任务`),
        el('span', 'muse-sec-chevron', museCronsCollapsed ? '›' : '⌄')
      );
      headBtn.onclick = () => {
        museCronsCollapsed = !museCronsCollapsed;
        renderMuseDrawer(context, auto, proposals, tasks);
      };
      schedBox.append(headBtn);
      if (!museCronsCollapsed) {
        const list = el('div', 'muse-cron-list');
        const visibleCrons = museShowAllCrons ? crons : crons.slice(0, 3);
        for (const c of visibleCrons) {
          const item = el('div', 'muse-cron-item');
          const isDaily = Boolean(c.daily_time_cst);
          const icon = el('span', 'muse-cron-icon ' + (isDaily ? 'daily' : 'heartbeat'), isDaily ? '🗓️' : '♡');
          const body = el('div', 'muse-cron-body');
          body.append(
            el('div', 'muse-cron-title', formatCronShortTitle(c.instruction)),
            el('div', 'muse-cron-sub', c.daily_time_cst ? c.daily_time_cst : `每${c.interval_minutes}分钟`)
          );
          item.append(icon, body);
          item.title = c.instruction || '';
          item.onclick = () => {
            const input = $('text');
            if (input) {
              input.value = `修改定时任务（${formatCronShortTitle(c.instruction)}）：`;
              input.focus?.();
              returnToChatOnNarrowScreen();
            }
          };
          list.append(item);
        }
        if (crons.length > 3) {
          const moreBtn = el('button', 'muse-cron-more', museShowAllCrons ? '收起预定任务' : '查看所有预定任务');
          moreBtn.type = 'button';
          moreBtn.onclick = () => {
            museShowAllCrons = !museShowAllCrons;
            renderMuseDrawer(context, auto, proposals, tasks);
          };
          list.append(moreBtn);
        }
        schedBox.append(list);
      }
    }
  }

  // 2. Tab 2 top: Audit & Approval Records (审核记录)
  const auditBox = $('muse-audit-box');
  if (auditBox) {
    auditBox.replaceChildren();
    const rules = Array.isArray(auto.rules) ? auto.rules : [];
    const propList = Array.isArray(proposals) ? proposals : [];
    const crossTasks = (Array.isArray(tasks) ? tasks : []).filter(t => t.requester !== t.owner);
    const title = el('h3', 'muse-audit-head', `审核记录与安全边界（${propList.length + crossTasks.length + rules.length}）`);
    auditBox.append(title);
    for (const r of rules) {
      const row = el('div', 'muse-audit-item');
      const desc = r.require_human_approval
        ? `🚨 高危拦截规则：含「${r.keyword}」需本人拍板`
        : r.require_clarification
          ? `📎 必填材料核验：含「${r.keyword}」需提供 ${r.require_clarification}`
          : `💻 自动路由规则：含「${r.keyword}」派给 ${r.target_node_id}`;
      row.append(el('span', 'muse-audit-badge', '规则'), el('span', 'muse-audit-text', desc));
      auditBox.append(row);
    }
    for (const p of propList.slice(0, 6)) {
      const row = el('div', 'muse-audit-item');
      const st = p.status === 'pending' ? '待你确认' : p.status === 'confirmed' ? '已确认委派' : '已拒绝';
      row.append(
        el('span', 'muse-audit-badge ' + p.status, st),
        el('span', 'muse-audit-text', `委派给 ${memberLabel(p.owner)}：${String(p.summary || '').slice(0, 36)}`)
      );
      auditBox.append(row);
    }
  }

  // 3. Tab 3: Butler Name + SOUL.md + MEMORY.md
  const nameInput = $('muse-name-input');
  const soulInput = $('muse-soul-input');
  const memInput = $('muse-memory-input');
  const activeEl = document.activeElement;
  if (nameInput && activeEl !== nameInput && auto.muse?.name) {
    nameInput.value = auto.muse.name;
  }
  if (soulInput && activeEl !== soulInput && typeof auto.muse?.soul_md === 'string') {
    soulInput.value = auto.muse.soul_md;
  }
  if (memInput && activeEl !== memInput && typeof auto.muse?.memory_md === 'string') {
    memInput.value = auto.muse.memory_md;
  }
}


