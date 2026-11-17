/* Navigation, keyboard and viewport behavior. */
let mentionState = { owner: '', people: null, loading: false, items: [], index: 0, start: 0, end: 0 };
function closeMentionPicker() {
  const picker = $('mention-picker');
  if (picker) { picker.classList.add('hide'); picker.replaceChildren(); }
  mentionState.items = [];
  mentionState.index = 0;
}
function resetMentionState() {
  closeMentionPicker();
  mentionState = { owner: '', people: null, loading: false, items: [], index: 0, start: 0, end: 0 };
}
function applyMentionSelection(idx) {
  const person = mentionState.items[idx];
  if (!person) return false;
  const name = person.display_name || person.id;
  const box = $('text');
  const value = String(box.value || '');
  const before = value.slice(0, mentionState.start);
  const after = value.slice(mentionState.end);
  const inserted = '@' + name + ' ';
  box.value = before + inserted + after;
  const caret = before.length + inserted.length;
  closeMentionPicker();
  box.focus?.();
  box.setSelectionRange?.(caret, caret);
  return true;
}
function renderMentionItems() {
  const picker = $('mention-picker');
  if (!picker) return;
  if (!mentionState.items.length) { closeMentionPicker(); return; }
  const head = el('div', 'mention-head', '选择同事（直接发消息或派发任务，无需打开联系人列表）');
  const rows = mentionState.items.map((person, idx) => {
    const active = idx === mentionState.index;
    const name = person.display_name || person.id;
    const item = el('button', 'mention-item' + (active ? ' active' : ''));
    item.type = 'button';
    item.dataset.mention = name;
    item.setAttribute('role', 'option');
    item.setAttribute('aria-selected', String(active));
    item.append(
      el('span', 'contact-avatar avatar-' + (person.color || 'green'), name.slice(0, 1).toUpperCase()),
      el('strong', 'mention-name', name),
      el('span', 'mention-handle', '@' + name + (person.id !== name ? ' · ' + person.id : ''))
    );
    item.onmousedown = event => { event.preventDefault(); applyMentionSelection(idx); };
    item.onclick = () => applyMentionSelection(idx);
    return item;
  });
  picker.replaceChildren(head, ...rows);
  picker.classList.remove('hide');
}
function updateMentionPicker() {
  const picker = $('mention-picker'), box = $('text');
  if (!picker || !box) return;
  const context = session();
  if (!current(context)) { closeMentionPicker(); return; }
  const value = String(box.value || '');
  const pos = typeof box.selectionStart === 'number' ? box.selectionStart : value.length;
  const head = value.slice(0, pos);
  const match = /(?:^|[^A-Za-z0-9])@([A-Za-z0-9_.\u4e00-\u9fa5-]{0,32})$/.exec(head);
  if (!match) { closeMentionPicker(); return; }
  const atOffset = match[0].lastIndexOf('@');
  const start = match.index + atOffset;
  const query = match[1].toLowerCase();
  const ownerKey = context.epoch + ':' + context.user;
  if (mentionState.owner !== ownerKey) {
    mentionState = { owner: ownerKey, people: null, loading: false, items: [], index: 0, start, end: pos };
  } else {
    mentionState.start = start;
    mentionState.end = pos;
  }
  if (!mentionState.people) {
    if (typeof directoryState !== 'undefined' && directoryState?.contacts?.length) {
      mentionState.people = directoryState.contacts.map(id => directoryState.people.get(id) || { id, display_name: peerName(id) });
    } else if (!mentionState.loading) {
      mentionState.loading = true;
      api(context, 'people').then(list => {
        if (!current(context) || mentionState.owner !== ownerKey) return;
        mentionState.people = Array.isArray(list) ? list : [];
        for (const p of mentionState.people) if (typeof rememberPerson === 'function') rememberPerson(p?.id, p?.display_name);
        updateMentionPicker();
      }).catch(() => {
        if (mentionState.owner === ownerKey) mentionState.people = [];
      }).finally(() => {
        if (mentionState.owner === ownerKey) mentionState.loading = false;
      });
      return;
    } else {
      return;
    }
  }
  const matches = mentionState.people.filter(p => p && p.id && p.id !== context.user && (
    (p.display_name || '').toLowerCase().includes(query) || p.id.toLowerCase().includes(query)
  )).slice(0, 8);
  mentionState.items = matches;
  if (mentionState.index >= matches.length) mentionState.index = 0;
  renderMentionItems();
}

// Enter sends, Shift+Enter inserts a newline. Never send while an IME is composing
// (Chinese input confirms candidates with Enter).
$('text').onkeydown = event => {
  if (!$('mention-picker')?.classList.contains('hide') && mentionState.items.length) {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      mentionState.index = (mentionState.index + 1) % mentionState.items.length;
      renderMentionItems();
      return;
    }
    if (event.key === 'ArrowUp') {
      event.preventDefault();
      mentionState.index = (mentionState.index - 1 + mentionState.items.length) % mentionState.items.length;
      renderMentionItems();
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      closeMentionPicker();
      return;
    }
    if ((event.key === 'Tab' || (event.key === 'Enter' && !event.shiftKey)) && !event.isComposing && event.keyCode !== 229) {
      event.preventDefault();
      applyMentionSelection(mentionState.index);
      return;
    }
  }
  if (event.key !== 'Enter' || event.shiftKey || event.isComposing || event.keyCode === 229) return;
  event.preventDefault();
  if (!$('send').disabled) $('form').requestSubmit?.();
};

// Presentational chrome only: drawer, composer menu, help, toast, autogrow.
// Never stores conversation content; the drawer preference is a per-viewer nicety.
const preference = {
  get(key) { try { return localStorage.getItem(key); } catch { return null; } },
  set(key, value) { try { localStorage.setItem(key, value); } catch { /* storage unavailable */ } }
};
let contactsGeneration = 0;
let accountOpen = false;
const contactDrafts = new Map();
let contactDraftOwner = '';
function clearContactDrafts() { contactDrafts.clear(); contactDraftOwner = ''; }
function setRailMenuOpen(open) {
  const menu = $('rail-menu');
  if (!menu) return;
  menu.classList[open ? 'remove' : 'add']('hide');
  syncRailState();
}
function syncRailState() {
  const room = $('room');
  if (!room) return;
  const narrow = globalThis.innerWidth <= 900 && !room.classList.contains('hide');
  const projects = room.classList.contains('projects-open');
  const settings = room.classList.contains('settings-open');
  const account = room.classList.contains('contacts-open') && accountOpen;
  const contacts = !account && (room.classList.contains('contact-mode') || room.classList.contains('contact-list-open') || room.classList.contains('contacts-open') || !!peerState);
  const tasks = room.classList.contains('tasks-open') && !projects && !settings && !room.classList.contains('contacts-open');
  const menuOpen = !$('rail-menu')?.classList.contains('hide');
  const chat = !projects && !settings && !account && !contacts && !(narrow && tasks);
  const setRail = (id, active, attr) => {
    const button = $(id);
    if (!button) return;
    button.classList[active ? 'add' : 'remove']('active');
    if (attr) button.setAttribute(attr, String(active));
  };
  setRail('open-chat', chat);
  if (chat) $('open-chat')?.setAttribute('aria-current', 'page');
  else $('open-chat')?.removeAttribute?.('aria-current');
  $('toggle-tasks')?.classList[narrow && tasks ? 'add' : 'remove']('active');
  $('toggle-tasks')?.setAttribute('aria-pressed', String(tasks));
  setRail('open-contacts', contacts, 'aria-expanded');
  setRail('open-projects', projects, 'aria-expanded');
  setRail('open-account', account || settings, 'aria-expanded');
  $('rail-more')?.classList[settings || account || menuOpen ? 'add' : 'remove']('active');
  $('rail-more')?.setAttribute('aria-expanded', String(menuOpen));
}
if ($('rail-more')) {
  $('rail-more').onclick = () => setRailMenuOpen($('rail-menu').classList.contains('hide'));
}
setRailMenuOpen(false);
document.addEventListener('click', event => {
  if (!$('rail-menu') || $('rail-menu').classList.contains('hide')) return;
  if (!event.target?.closest?.('.rail-more-wrap')) setRailMenuOpen(false);
});
function setTasksOpen(open) {
  if (open) {
    setRailMenuOpen(false);
    if (globalThis.innerWidth <= 900) closeComposerOverlays();
    if (typeof closeProjects === 'function') closeProjects();
    closeContacts(); profileUI.reset(); hideCollaborationSettings();
  }
  $('room').classList[open ? 'add' : 'remove']('tasks-open');
  preference.set('agenct.tasksOpen', open ? '1' : '0');
  syncRailState();
  if (document.querySelector) updatePanelAccessibility();
}
$('toggle-tasks').onclick = () => setTasksOpen($('room').classList.contains('contacts-open') || $('room').classList.contains('settings-open') || !$('room').classList.contains('tasks-open'));
$('close-tasks').onclick = () => { setTasksOpen(false); $('toggle-tasks').focus?.(); };
function returnToChatOnNarrowScreen() {
  if (globalThis.innerWidth <= 900) setTasksOpen(false);
}
$('open-chat').onclick = restorePrivateChat;
// Default: docked open on wide screens, closed on phones where it would cover the chat.
const savedTasksOpen = preference.get('agenct.tasksOpen');
setTasksOpen(savedTasksOpen === null ? !(globalThis.innerWidth <= 900) : savedTasksOpen === '1');
function setMenuOpen(open) {
  $('composer-menu').classList[open ? 'remove' : 'add']('hide');
  $('composer-menu-toggle').setAttribute('aria-expanded', String(open));
}
function closeComposerOverlays() {
  setMenuOpen(false);
  closeMentionPicker();
  if (typeof closeRolePicker === 'function' && !roleLocked()) closeRolePicker();
  if (typeof closeRepositoryPicker === 'function' && !repositoryLocked()) closeRepositoryPicker();
}
setMenuOpen(false);
$('composer-menu-toggle').onclick = () => {
  const anyOpen = !$('composer-menu').classList.contains('hide')
    || !$('role-picker').classList.contains('hide')
    || !$('repository-picker').classList.contains('hide');
  if (anyOpen) closeComposerOverlays();
  else setMenuOpen(true);
};
// Same path as typing /agents: handled locally by the profile UI, never sent to a model.
$('open-agents').onclick = async () => {
  setRailMenuOpen(false);
  const context = session();
  if (!current(context) || $('send').disabled || $('open-agents').disabled) return;
  returnToChatOnNarrowScreen();
  $('open-agents').disabled = true;
  try {
    // Open the local editor directly. Never replace or submit an unsent message.
    await profileUI.handle(context, '/agents');

  } finally {
    if (current(context)) $('open-agents').disabled = false;
  }
};
if ($('manage-roles')) $('manage-roles').onclick = () => $('open-agents')?.onclick?.();
$('status').onclick = () => status('');
$('status').onkeydown = event => { if (event.key === 'Escape') { event.preventDefault(); status(''); } };
$('text').oninput = () => {
  updateMentionPicker();
  const box = $('text');
  if (!box.style) return;
  box.style.height = 'auto';
  box.style.height = Math.min(box.scrollHeight, 200) + 'px';
};
$('tab-activity').onclick = () => setPaneTab('activity');
$('tab-tasks').onclick = () => setPaneTab('tasks');
setPaneTab('activity');



$('task-filter').onchange = () => { taskFilter = $('task-filter').value; applyTaskFilter(); };

document.addEventListener('keydown', event => {
  if (!current(session())) return;
  if ((event.ctrlKey || event.metaKey) && event.shiftKey && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    const open = !$('room').classList.contains('tasks-open'); setTasksOpen(open);
    (open ? $('tab-activity') : $('text')).focus?.(); return;
  }
  if (event.key !== 'Escape' || event.defaultPrevented) return;
  if (!$('rail-menu').classList.contains('hide')) { setRailMenuOpen(false); $('rail-more')?.focus?.(); return; }
  if ($('room').classList.contains('projects-open')) { closeProjects(); $('open-projects').focus?.(); return; }
  if ($('room').classList.contains('contacts-open')) {
    const wasAccount = accountOpen;
    closeContacts();
    (wasAccount ? ($('rail-more') || $('open-account')) : $('open-contacts')).focus?.();
    return;
  }
  if ($('room').classList.contains('settings-open')) { profileUI.close(); return; }
  if ($('room').classList.contains('contact-list-open')) { restorePrivateChat(); return; }
  if (!$('repository-picker').classList.contains('hide')) { closeRepositoryPicker(); $('choose-repository').focus?.(); return; }
  if (!$('role-picker').classList.contains('hide')) { closeRolePicker(); $('choose-role').focus?.(); return; }
  const openMenu = document.querySelector?.('details.more[open]');
  if (openMenu) { openMenu.open = false; openMenu.querySelector('summary')?.focus(); return; }
  if (!$('composer-menu').classList.contains('hide')) { setMenuOpen(false); $('composer-menu-toggle').focus?.(); return; }
  if ($('room').classList.contains('tasks-open')) { setTasksOpen(false); $('toggle-tasks').focus?.(); }
});
// Browser zoom is left alone. At scale 1, track the visual viewport so a phone's
// software keyboard reduces the application viewport instead of covering send.
function updateVisualViewport() {
  const viewport = globalThis.visualViewport;
  if (!document.documentElement?.style) return;
  const height = viewport && viewport.scale === 1 ? viewport.height : globalThis.innerHeight;
  if (height) document.documentElement.style.setProperty('--viewport-height', height + 'px');
}
globalThis.visualViewport?.addEventListener('resize', updateVisualViewport);
globalThis.addEventListener?.('resize', updateVisualViewport);
updateVisualViewport();

for (const id of ['tab-activity', 'tab-tasks']) $(id).onkeydown = event => {
  if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  event.preventDefault();
  const target = event.key === 'Home' ? 'activity' : event.key === 'End' ? 'tasks' : id === 'tab-activity' ? 'tasks' : 'activity';
  setPaneTab(target); $('tab-' + target).focus?.();
};
$('choose-repository').onclick = openRepositoryPicker;
$('close-repository-picker').onclick = () => { closeRepositoryPicker(); $('choose-repository').focus?.(); };
$('clear-repository').onclick = () => { if (!current(session()) || repositoryLocked()) return; selectedRepository = null; renderRepositorySelection(); };

function closeContacts() {
  // Keep these controls and their handlers alive when the account panel closes.
  const home = $('account-tools-home');
  for (const id of ['open-agents', 'collaboration-profile-view', 'account-tools']) {
    const node = $(id);
    if (home && node && node.parentElement !== home) home.append(node);
  }
  if (typeof collaborationProfileUI !== 'undefined') collaborationProfileUI.reset();
  contactsGeneration++;
  accountOpen = false;
  $('contacts-view').replaceChildren();
  $('contacts-view').classList.add('hide');
  $('room').classList.remove('contacts-open');
  syncRailState();
}
async function openContacts() { return openContactDirectory(); }
$('open-contacts').onclick = openContacts;
void checkSession();

// Covered mobile content must not remain reachable by keyboard or assistive technology.
function updatePanelAccessibility() {
  syncRailState();
  const room = $('room'), chat = document.querySelector('.chat-pane');
  if (!chat) return;
  const narrow = globalThis.innerWidth <= 900 && !room.classList.contains('hide');
  const panel = room.classList.contains('projects-open') ? $('project-panel')
    : room.classList.contains('contact-list-open') ? $('contact-sidebar')
    : room.classList.contains('contacts-open') ? $('contacts-view')
    : room.classList.contains('settings-open') ? $('profile-view')
    : room.classList.contains('tasks-open') ? document.querySelector('.task-pane') : null;
  chat.inert = room.classList.contains('peer-active') || !!(narrow && panel);
  $('peer-view').inert = !!(narrow && panel);
  $('contact-sidebar').inert = !!(narrow && panel && panel !== $('contact-sidebar'));
  if (panel && narrow && (chat.contains(document.activeElement) || $('peer-view').contains(document.activeElement) || document.activeElement === $('toggle-tasks'))) {
    panel.querySelector('button, input, select, textarea')?.focus({ preventScroll: true });
  }
  const height = $('sync-error').getBoundingClientRect().height;
  room.style.setProperty('--sync-error-height', height + 'px');
}
if (typeof MutationObserver !== 'undefined') {
  new MutationObserver(updatePanelAccessibility).observe($('room'), { attributes: true, attributeFilter: ['class'] });
  new ResizeObserver(updatePanelAccessibility).observe($('sync-error'));
  globalThis.addEventListener('resize', updatePanelAccessibility);
  updatePanelAccessibility();
}
globalThis.addEventListener?.('beforeunload', event => {
  if ((typeof projectDraft !== 'undefined' && projectDraft) || profileUI.hasUnsaved() || collaborationProfileUI.hasUnsaved() || [...peerDrafts.values()].some(draft => draft.value.trim() || draft.pending) || [...contactDrafts.values()].some(draft => draft.value.trim())) {
    event.preventDefault(); event.returnValue = '';
  }
});
