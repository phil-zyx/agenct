/* Navigation, keyboard, onboarding and viewport behavior. */
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
function setTasksOpen(open) {
  $('room').classList[open ? 'add' : 'remove']('tasks-open');
  $('toggle-tasks').setAttribute('aria-pressed', String(open));
  preference.set('agenct.tasksOpen', open ? '1' : '0');
}
$('toggle-tasks').onclick = () => setTasksOpen(!$('room').classList.contains('tasks-open'));
$('close-tasks').onclick = () => { setTasksOpen(false); $('toggle-tasks').focus?.(); };
function returnToChatOnNarrowScreen() {
  if (globalThis.innerWidth <= 900) setTasksOpen(false);
}
$('open-chat').onclick = () => { setTasksOpen(false); $('text').focus?.(); };
// Default: docked open on wide screens, closed on phones where it would cover the chat.
const savedTasksOpen = preference.get('agenct.tasksOpen');
setTasksOpen(savedTasksOpen === null ? !(globalThis.innerWidth <= 900) : savedTasksOpen === '1');
function setMenuOpen(open) {
  $('composer-menu').classList[open ? 'remove' : 'add']('hide');
  $('composer-menu-toggle').setAttribute('aria-expanded', String(open));
}
setMenuOpen(false);
$('composer-menu-toggle').onclick = () => setMenuOpen($('composer-menu').classList.contains('hide'));
$('open-help').onclick = () => { returnToChatOnNarrowScreen(); setMenuOpen(true); $('help').open = true; $('help').focus?.(); };
// Same path as typing /agents: handled locally by the profile UI, never sent to a model.
$('open-agents').onclick = async () => {
  const context = session();
  if (!current(context) || $('send').disabled || $('open-agents').disabled) return;
  returnToChatOnNarrowScreen();
  $('open-agents').disabled = true;
  try {
    // Open the local editor directly. Never replace or submit an unsent message.
    await profileUI.handle(context, '/agents');
    if (current(context)) $('conversation').lastElementChild?.scrollIntoView?.({ block: 'nearest' });
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

let onboardingStep = 0;
const onboardingSteps = [
  '1 / 3 · 先与管家聊需求。你可以直接输入，也可以点空白会话中的示例。私聊只属于你。',
  '2 / 3 · 请别人帮忙时，先核对提案中的接收人、摘要和材料。只有确认后，对方才会收到。',
  '3 / 3 · 在「工作与任务」查看进度。草稿先由你审阅，交付后再由委派人验收。Ctrl / ⌘ + Shift + K 可切换任务面板。'
];
function showOnboarding(context) {
  if (!current(context) || preference.get('agenct.onboarding.v1:' + context.user) === 'done') return;
  onboardingStep = 0;
  $('onboarding').classList.remove('hide');
  $('onboarding-step').textContent = onboardingSteps[0];
  $('onboarding-next').textContent = '下一步';
}
function dismissOnboarding() {
  if (user) preference.set('agenct.onboarding.v1:' + user, 'done');
  $('onboarding').classList.add('hide'); $('text').focus?.();
}
$('onboarding-next').onclick = () => {
  if (!current(session())) return;
  if (++onboardingStep >= onboardingSteps.length) { dismissOnboarding(); return; }
  $('onboarding-step').textContent = onboardingSteps[onboardingStep];
  $('onboarding-next').textContent = onboardingStep === 2 ? '开始使用' : '下一步';
};
$('onboarding-skip').onclick = dismissOnboarding;
document.addEventListener('keydown', event => {
  if (!current(session())) return;
  if ((event.ctrlKey || event.metaKey) && event.shiftKey && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    const open = !$('room').classList.contains('tasks-open'); setTasksOpen(open);
    (open ? $('tab-activity') : $('text')).focus?.(); return;
  }
  if (event.key !== 'Escape' || event.defaultPrevented) return;
  if (!$('repository-picker').classList.contains('hide')) { closeRepositoryPicker(); $('choose-repository').focus?.(); return; }
  if (!$('role-picker').classList.contains('hide')) { closeRolePicker(); $('choose-role').focus?.(); return; }
  const openMenu = document.querySelector?.('details.more[open]');
  if (openMenu) { openMenu.open = false; openMenu.querySelector('summary')?.focus(); return; }
  if (!$('composer-menu').classList.contains('hide')) { setMenuOpen(false); $('composer-menu-toggle').focus?.(); return; }
  if (!$('onboarding').classList.contains('hide')) { dismissOnboarding(); return; }
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

$('replay-onboarding').onclick = () => {
  const context = session(); if (!current(context)) return;
  preference.set('agenct.onboarding.v1:' + context.user, ''); setMenuOpen(false); showOnboarding(context);
};
for (const id of ['tab-activity', 'tab-tasks']) $(id).onkeydown = event => {
  if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  event.preventDefault();
  const target = event.key === 'Home' ? 'activity' : event.key === 'End' ? 'tasks' : id === 'tab-activity' ? 'tasks' : 'activity';
  setPaneTab(target); $('tab-' + target).focus?.();
};
$('choose-repository').onclick = openRepositoryPicker;
$('close-repository-picker').onclick = () => { closeRepositoryPicker(); $('choose-repository').focus?.(); };
$('clear-repository').onclick = () => { if (!current(session()) || repositoryLocked()) return; selectedRepository = null; renderRepositorySelection(); };
void checkSession();
