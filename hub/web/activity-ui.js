/* Shared work-index presentation; no writes. */
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
  const who = l.actor === me ? '你' : l.actor + ' ';
  switch (l.action) {
    case 'proposal_pending': return `等你确认 · 委派给 ${item.counterpart}`;
    case 'proposal_rejected': return '已拒绝，未共享';
    case 'created': return item.role === 'owner' ? `${item.counterpart} 委派给你，待接手` : `已委派给 ${item.counterpart}，等对方接手`;
    case 'accept': return `${who}已接手`;
    case 'deliver': return `${who}提交了交付内容`;
    case 'approve': return `${who}验收通过`;
    case 'return': return `${who}退回了交付内容`;
    case 'cancel': return `${who}取消了任务` + (l.reason ? `：${l.reason}` : '');
    case 'draft_complete': return '草稿已写好，等你审阅（仅你可见）';
    case 'draft_uncertain': return '暂时无法确认起草结果，请打开草稿查看';
    case 'draft_pending': return '正在为你起草…';
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
    nodes.push(el('h3', 'day', group.label));
    for (const { item, at } of group.items) {
      const row = el('button', 'activity-item status-' + item.status);
      row.type = 'button';
      const icon = activityIcon(item.status);
      const body = el('span', 'activity-body');
      body.append(el('span', 'activity-title', item.title), el('span', 'activity-line', activityLine(item, context.user)));
      row.append(icon, body, el('span', 'activity-time', at ? clock(at) : ''));
      row.onclick = () => {
        if (!current(context)) return;
        if (item.kind === 'task') {
          setPaneTab('tasks');
          panelTask = item.id;
          flash([...$('tasks').children].find(card => card.taskId === item.id));
        } else {
          closeContacts(); closePeerConversation();
          returnToChatOnNarrowScreen();
          flash([...$('conversation').children].find(card => card.proposalId === item.id));
        }
      };
      nodes.push(row);
    }
  }
  $('activity').replaceChildren(...nodes);
}
function setPaneTab(tab) {
  const tasks = tab === 'tasks';
  $('tasks').classList[tasks ? 'remove' : 'add']('hide');
  $('task-filters').classList[tasks ? 'remove' : 'add']('hide');
  $('activity').classList[tasks ? 'add' : 'remove']('hide');
  $('tab-tasks').classList[tasks ? 'add' : 'remove']('on');
  $('tab-activity').classList[tasks ? 'remove' : 'add']('on');
  $('tab-tasks').setAttribute('aria-selected', String(tasks));
  $('tab-activity').setAttribute('aria-selected', String(!tasks));
  $('tab-activity').tabIndex = tasks ? -1 : 0;
  $('tab-tasks').tabIndex = tasks ? 0 : -1;
}

