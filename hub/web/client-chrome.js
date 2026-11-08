/* Navigation, keyboard and viewport behavior. */
// Enter sends, Shift+Enter inserts a newline. Never send while an IME is composing
// (Chinese input confirms candidates with Enter).
$('text').onkeydown = event => {
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
const contactDrafts = new Map();
let contactDraftOwner = '';
function clearContactDrafts() { contactDrafts.clear(); contactDraftOwner = ''; }
function setTasksOpen(open) {
  if (open) { closeContacts(); profileUI.reset(); hideCollaborationSettings(); }
  $('room').classList[open ? 'add' : 'remove']('tasks-open');
  $('toggle-tasks').setAttribute('aria-pressed', String(open));
  preference.set('agenct.tasksOpen', open ? '1' : '0');
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
setMenuOpen(false);
$('composer-menu-toggle').onclick = () => setMenuOpen($('composer-menu').classList.contains('hide'));
// Same path as typing /agents: handled locally by the profile UI, never sent to a model.
$('open-agents').onclick = async () => {
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
$('status').onclick = () => { $('status').textContent = ''; };
$('text').oninput = () => {
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
  if ($('room').classList.contains('collaboration-settings-open')) { collaborationProfileUI.close(); $('open-collaboration').focus?.(); return; }
  if ($('room').classList.contains('contacts-open')) { closeContacts(); $('open-contacts').focus?.(); return; }
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
  const home = $('account-tools-home'), tools = $('account-tools');
  if (home && tools && tools.parentElement !== home) home.append(tools);
  contactsGeneration++;
  $('contacts-view').replaceChildren();
  $('contacts-view').classList.add('hide');
  $('room').classList.remove('contacts-open');
  if (!peerState) $('open-contacts').classList.remove('active');
  $('open-account').setAttribute('aria-expanded', 'false');
  $('open-contacts').setAttribute('aria-expanded', String(!!peerState));
  $('toggle-tasks').setAttribute('aria-pressed', String($('room').classList.contains('tasks-open') && !$('room').classList.contains('settings-open')));
}
async function openContacts() { return openContactDirectory(); }
$('open-contacts').onclick = openContacts;
void checkSession();

// Covered mobile content must not remain reachable by keyboard or assistive technology.
function updatePanelAccessibility() {
  const room = $('room'), chat = document.querySelector('.chat-pane');
  if (!chat) return;
  const narrow = globalThis.innerWidth <= 900 && !room.classList.contains('hide');
  const panel = room.classList.contains('collaboration-settings-open') ? $('collaboration-profile-view')
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
  if (profileUI.hasUnsaved() || collaborationProfileUI.hasUnsaved() || [...peerDrafts.values()].some(draft => draft.value.trim() || draft.pending) || [...contactDrafts.values()].some(draft => draft.value.trim())) {
    event.preventDefault(); event.returnValue = '';
  }
});
