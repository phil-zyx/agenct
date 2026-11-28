// Agenct v2 entrypoint with classic left-rail + centered-chat + floating-avatar Muse drawer layout.
import { api, setCsrfToken, showToast } from './api.js';
import { createAvatarController } from './avatar.js';
import { initChat } from './chat.js';
import { initDrawer } from './drawer.js';
import { initCommandPalette } from './palette.js';
import { cycleSoundMode, getSoundMode, isSoundEnabled, playDone, playTap, playToggle, playWarn, soundModeLabel } from './sound.js';
import { connectEvents } from './sse.js';
import { cycleThemeMode, getThemeMode, initRailGlideTip, isEffectiveDark, themeModeLabel } from './ui.js';

const state = {
  user: null,
  profile: null,
  roles: [],
  threads: [],
  messages: [],
  has_more_messages: false,
  tasks: [],
  nodes: [],
  permissions: {},
  rules: [],
  crons: [],
  audit_logs: [],
  contacts: [],
};

const loginView = document.getElementById('login-view');
const roomEl = document.getElementById('room');
const chatPane = document.getElementById('chat-pane');
const settingsView = document.getElementById('settings-view');
const contactSidebar = document.getElementById('contact-sidebar');
const loginErr = document.getElementById('login-error');

const navChat = document.getElementById('nav-chat');
const navTasks = document.getElementById('nav-tasks');
const navContacts = document.getElementById('nav-contacts');
const navSearch = document.getElementById('nav-search');
const searchPanel = document.getElementById('search-panel');
const searchInput = document.getElementById('search-input');
const closeSearch = document.getElementById('close-search');
const searchResults = document.getElementById('search-results');
const railBadge = document.getElementById('rail-badge');
const railMore = document.getElementById('rail-more');
const railMenu = document.getElementById('rail-menu');
const railIndicator = document.getElementById('rail-indicator');
const museToggle = document.getElementById('muse-head-toggle');

const agentNameEl = document.getElementById('agent-name');
const headStateTextEl = document.getElementById('head-state-text');
const drawerAgentNameEl = document.getElementById('drawer-agent-name');
const headAvatarEl = document.getElementById('head-avatar');
const headBuddyEl = document.getElementById('head-avatar-buddy');
const headBadgeEl = document.getElementById('head-avatar-badge');
const drawerAvatarEl = document.getElementById('drawer-avatar');
const drawerBuddyEl = document.getElementById('drawer-avatar-buddy');
const drawerBadgeEl = document.getElementById('drawer-avatar-badge');
const heroAvatarWrapEl = document.getElementById('hero-avatar-wrap');
const avatarStatePill = document.getElementById('avatar-state-pill');
const settingsIdentity = document.getElementById('settings-identity');
const nodePill = document.getElementById('node-pill');
const ssePill = document.getElementById('sse-pill');
const sseBolt = document.getElementById('sse-bolt');

const avatarUI = createAvatarController({
  headToggleEl: museToggle,
  headAvatarEl,
  headBuddyEl,
  headBadgeEl,
  headStateTextEl,
  heroWrapEl: heroAvatarWrapEl,
  chatPaneEl: chatPane,
  drawerAvatarEl,
  drawerBuddyEl,
  drawerBadgeEl,
  statusPillEl: avatarStatePill,
});

let chatUI = null;
let drawerUI = null;
let refreshInFlight = null;
let refreshQueued = false;
let syncTimer = null;
let taskEventVersion = 0;

const isMobile = () => window.innerWidth <= 900;

function syncRailState() {
  const inSettings = !settingsView.classList.contains('hide');
  const tasksOpen = roomEl.classList.contains('tasks-open');
  const contactsOpen = !contactSidebar.classList.contains('hide');
  const searchOpen = Boolean(searchPanel && !searchPanel.classList.contains('hide'));
  const menuOpen = !railMenu.classList.contains('hide');

  navChat.classList.toggle('active', !inSettings);
  navChat.setAttribute('aria-current', inSettings ? 'false' : 'page');
  railMore.classList.toggle('active', inSettings);
  navTasks.classList.toggle('is-open-panel', tasksOpen);
  navContacts.classList.toggle('is-open-panel', contactsOpen);
  navSearch?.classList.toggle('is-open-panel', searchOpen);
  railMore.classList.toggle('is-open-panel', !inSettings && menuOpen);

  if (railIndicator) {
    if (inSettings) {
      railIndicator.style.opacity = '0';
    } else {
      railIndicator.style.opacity = '1';
      railIndicator.style.transform = `translateY(${navChat.offsetTop}px)`;
    }
  }
}

function setTasksOpen(open, silent = false) {
  const wasOpen = roomEl.classList.contains('tasks-open');
  if (open && isMobile()) setContactsOpen(false, true);
  roomEl.classList.toggle('tasks-open', open);
  navTasks.setAttribute('aria-pressed', String(open));
  museToggle.setAttribute('aria-expanded', String(open));
  if (!open) {
    document.getElementById('status-tray-wrap')?.classList.remove('is-open');
    document.getElementById('status-tray-wrap')?.setAttribute('aria-hidden', 'true');
    document.getElementById('status-tray')?.setAttribute('aria-hidden', 'true');
    document.getElementById('status-picker-btn')?.setAttribute('aria-expanded', 'false');
    document.getElementById('status-tray-tip')?.classList.remove('is-visible');
  }
  syncRailState();
  if (!silent && wasOpen !== open) playToggle(open);
}

function setContactsOpen(open, silent = false) {
  const wasOpen = !contactSidebar.classList.contains('hide');
  if (open && isMobile()) setTasksOpen(false, true);
  contactSidebar.classList.toggle('hide', !open);
  roomEl.classList.toggle('contacts-open', open);
  navContacts.setAttribute('aria-expanded', String(open));
  navContacts.classList.toggle('active', open);
  syncRailState();
  if (!silent && wasOpen !== open) playToggle(open);
}

function setSettingsOpen(open) {
  const wasOpen = !settingsView.classList.contains('hide');
  settingsView.classList.toggle('hide', !open);
  chatPane.classList.toggle('hide', open);
  if (!wasOpen && open) playToggle(true);
  else if (wasOpen && !open) playToggle(false);
  if (open) {
    setTasksOpen(false, true);
    setContactsOpen(false, true);
    drawerUI.loadIntegrations().then(() => drawerUI.renderSettingsPane());
  }
  syncRailState();
}

async function refresh() {
  if (syncTimer) {
    clearTimeout(syncTimer);
    syncTimer = null;
  }
  if (refreshInFlight) {
    refreshQueued = true;
    return refreshInFlight;
  }
  refreshInFlight = (async () => {
    const taskVersion = taskEventVersion;
    const data = await api('/api/bootstrap');
    Object.assign(state, data);
    const name = state.profile?.name || 'Agenct';
    agentNameEl.textContent = name;
    drawerAgentNameEl.textContent = name;
    settingsIdentity.textContent = state.user
      ? `${state.user.display_name} (@${state.user.username})`
      : '';
    const onlineCount = (state.nodes || []).filter((n) => n.online).length;
    nodePill.textContent = `${onlineCount} 节点在线`;
    const activeTasks = (state.tasks || []).filter(
      (t) => t.status === 'pending' || t.status === 'running',
    ).length;
    railBadge.textContent = activeTasks > 0 ? String(activeTasks) : '';
    avatarUI.setAvatar(state.profile?.avatar_id);
    chatUI.syncTasks(state.tasks || [], taskVersion);
    chatUI.render();
    chatUI.renderEnginePill?.();
    drawerUI.render();
  })().finally(() => {
    refreshInFlight = null;
    if (refreshQueued) {
      refreshQueued = false;
      refresh().catch((e) => showToast(e.message));
    }
  });
  return refreshInFlight;
}

function scheduleRefresh() {
  if (refreshInFlight) {
    refreshQueued = true;
    return;
  }
  if (syncTimer) return;
  syncTimer = setTimeout(() => {
    syncTimer = null;
    refresh().catch((e) => showToast(e.message));
  }, 80);
}

async function boot() {
  const params = new URLSearchParams(location.search);
  const errCode = params.get('login_error');
  if (errCode) {
    loginErr.textContent = `GitLab 登录失败（${errCode}），请重试。`;
    loginErr.classList.remove('hide');
  }

  let sess;
  try {
    sess = await api('/auth/session');
  } catch (err) {
    loginView.classList.remove('hide');
    loginErr.textContent = `无法连接服务：${err.message}`;
    loginErr.classList.remove('hide');
    return;
  }

  if (!sess?.authenticated) {
    loginView.classList.remove('hide');
    // Older Hubs don't advertise methods: they only support GitLab.
    const methods = sess?.login || { gitlab: true, local: false };
    document.getElementById('gitlab-login-btn').classList.toggle('hide', !methods.gitlab);
    const localForm = document.getElementById('local-login-form');
    localForm.classList.toggle('hide', !methods.local);
    localForm.onsubmit = async (ev) => {
      ev.preventDefault();
      const btn = localForm.querySelector('button[type="submit"]');
      btn.disabled = true;
      try {
        await api('/auth/local', { method: 'POST', body: { token: localForm.token.value.trim() } });
        location.replace('/');
      } catch (err) {
        if (err.status === 429) {
          loginErr.textContent = '尝试次数过多，请稍后再试。';
        } else {
          loginErr.textContent = err.message === 'invalid login token' ? '登录令牌不正确。' : `登录失败：${err.message}`;
        }
        loginErr.classList.remove('hide');
        btn.disabled = false;
      }
    };
    if (sess?.reason === 'disabled') {
      loginErr.textContent = '该账号已被停用，请联系管理员。';
      loginErr.classList.remove('hide');
    }
    return;
  }

  setCsrfToken(sess.csrf_token);
  loginView.classList.add('hide');
  roomEl.classList.remove('hide');

  chatUI = initChat({
    state,
    onRefresh: refresh,
    onActivityChange: (act) => avatarUI.setActivity(act),
  });
  drawerUI = initDrawer({
    state,
    onRefresh: refresh,
    onMention(username) {
      setSettingsOpen(false);
      setContactsOpen(false);
      chatUI.insertMention(username);
    },
    onPromptChat(prefix) {
      setSettingsOpen(false);
      if (window.innerWidth <= 900) setTasksOpen(false);
      const input = document.getElementById('chat-input');
      if (input) {
        if (!input.value.trim()) input.value = prefix;
        input.focus();
      }
    },
    onFilterThread(threadId, title) {
      setSettingsOpen(false);
      if (window.innerWidth <= 900) setTasksOpen(false);
      chatUI.setThreadFilter(threadId, title);
    },
  });

  let commandPalette = null;
  function openAvatarPicker() {
    setSettingsOpen(false);
    drawerUI.openTab('soul');
    const grid = document.querySelector('#drawer-body .avatar-grid');
    if (grid) {
      grid.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
      grid.classList.remove('is-highlighted');
      void grid.offsetWidth;
      grid.classList.add('is-highlighted');
      setTimeout(() => grid.classList.remove('is-highlighted'), 950);
    }
  }

  commandPalette = initCommandPalette({
    state,
    panelEl: searchPanel,
    inputEl: searchInput,
    resultsEl: searchResults,
    closeBtn: closeSearch,
    triggerBtn: navSearch,
    actions: {
      onBeforeOpen: () => {
        if (isMobile()) {
          setTasksOpen(false, true);
          setContactsOpen(false, true);
        }
      },
      onSyncRail: syncRailState,
      onSwitchEngine: async (node, ag) => {
        commandPalette.close(true);
        await chatUI.switchEngine(node, ag);
      },
      getSoundLabel: () => soundModeLabel(getSoundMode()),
      onCycleSound: () => {
        const nextMode = cycleSoundMode();
        syncSoundUI();
        showToast(`交互音效：${soundModeLabel(nextMode)}`);
        commandPalette.open();
      },
      getThemeLabel: () => themeModeLabel(getThemeMode()),
      onCycleTheme: (ev) => {
        const nextMode = cycleThemeMode(ev);
        syncThemeUI();
        showToast(`外观主题：${themeModeLabel(nextMode)}`);
        commandPalette.open();
      },
      onOpenAvatarPicker: () => {
        commandPalette.close(true);
        openAvatarPicker();
      },
      onRenameAgent: () => {
        commandPalette.close(true);
        setSettingsOpen(false);
        setTasksOpen(true);
        setTimeout(startEditAgentName, 60);
      },
      onToggleTasks: (force) => {
        commandPalette.close(true);
        setSettingsOpen(false);
        setTasksOpen(typeof force === 'boolean' ? force : !roomEl.classList.contains('tasks-open'));
      },
      isTasksOpen: () => roomEl.classList.contains('tasks-open'),
      onToggleContacts: () => {
        commandPalette.close(true);
        setSettingsOpen(false);
        setContactsOpen(contactSidebar.classList.contains('hide'));
      },
      isContactsOpen: () => !contactSidebar.classList.contains('hide'),
      onOpenSettings: () => {
        commandPalette.close(true);
        setSettingsOpen(true);
      },
      onOpenThread: (threadId, title) => {
        setSettingsOpen(false);
        if (window.innerWidth <= 900) setTasksOpen(false);
        chatUI.setThreadFilter(threadId, title);
      },
    },
  });

  const editAgentNameBtn = document.getElementById('edit-agent-name-btn');
  const agentNameInput = document.getElementById('drawer-agent-name-input');
  let editingAgentName = false;

  function startEditAgentName() {
    if (!editAgentNameBtn || !agentNameInput || editingAgentName) return;
    editingAgentName = true;
    playTap();
    agentNameInput.value = state.profile?.name || drawerAgentNameEl.textContent || 'Agenct';
    editAgentNameBtn.classList.add('hide');
    agentNameInput.classList.remove('hide');
    agentNameInput.focus();
    agentNameInput.select();
  }

  async function finishEditAgentName(commit) {
    if (!editAgentNameBtn || !agentNameInput || !editingAgentName) return;
    editingAgentName = false;
    const prevName = state.profile?.name || drawerAgentNameEl.textContent || 'Agenct';
    const nextName = agentNameInput.value.trim();
    agentNameInput.classList.add('hide');
    editAgentNameBtn.classList.remove('hide');
    if (!commit) return;
    if (!nextName) {
      showToast('名字不能为空');
      return;
    }
    if (nextName === prevName) return;
    agentNameEl.textContent = nextName;
    drawerAgentNameEl.textContent = nextName;
    try {
      await api('/api/brain', { method: 'PUT', body: { name: nextName } });
      playDone();
      showToast(`已更名为：${nextName}`);
      await refresh();
    } catch (err) {
      playWarn();
      agentNameEl.textContent = prevName;
      drawerAgentNameEl.textContent = prevName;
      showToast(`更名失败：${err.message}`);
    }
  }

  editAgentNameBtn?.addEventListener('click', startEditAgentName);
  agentNameInput?.addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter' && !ev.isComposing) {
      ev.preventDefault();
      finishEditAgentName(true);
    } else if (ev.key === 'Escape') {
      ev.preventDefault();
      ev.stopPropagation();
      finishEditAgentName(false);
    }
  });
  agentNameInput?.addEventListener('blur', () => finishEditAgentName(true));

  document.addEventListener('keydown', (ev) => {
    if (roomEl.classList.contains('hide')) return;
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === 'k') {
      ev.preventDefault();
      commandPalette.toggle();
    } else if (ev.key === 'Escape') {
      if (commandPalette.isOpen()) commandPalette.close();
      setStatusTrayOpen(false, true);
    }
  });

  // Left rail & floating avatar navigation
  navChat.onclick = () => {
    playTap();
    setSettingsOpen(false);
    if (isMobile()) {
      setTasksOpen(false, true);
      setContactsOpen(false, true);
    }
    railMenu.classList.add('hide');
  };
  navTasks.onclick = () => {
    setSettingsOpen(false);
    setTasksOpen(!roomEl.classList.contains('tasks-open'));
  };
  museToggle.onclick = () => setTasksOpen(!roomEl.classList.contains('tasks-open'));
  document.getElementById('close-drawer').onclick = () => setTasksOpen(false);

  navContacts.onclick = () => {
    setSettingsOpen(false);
    setContactsOpen(contactSidebar.classList.contains('hide'));
  };
  document.getElementById('close-contacts').onclick = () => setContactsOpen(false);

  railMore.onclick = (ev) => {
    ev.stopPropagation();
    const open = railMenu.classList.contains('hide');
    railMenu.classList.toggle('hide', !open);
    railMore.setAttribute('aria-expanded', String(open));
    syncRailState();
    playTap();
  };
  document.addEventListener('click', () => {
    railMenu.classList.add('hide');
    railMore.setAttribute('aria-expanded', 'false');
    setStatusTrayOpen(false, true);
    syncRailState();
  });

  const soundBtn = document.getElementById('toggle-sound');
  const soundLabel = document.getElementById('sound-label');
  const themeBtn = document.getElementById('toggle-theme');
  const themeLabel = document.getElementById('theme-label');
  const themeMenuIc = document.getElementById('theme-menu-ic');
  const statusPickerBtn = document.getElementById('status-picker-btn');
  const statusTrayWrap = document.getElementById('status-tray-wrap');
  const statusTray = document.getElementById('status-tray');
  const statusTrayTip = document.getElementById('status-tray-tip');
  const statusSoundOrb = document.getElementById('status-sound-orb');
  const statusThemeOrb = document.getElementById('status-theme-orb');
  const statusThemeIc = document.getElementById('status-theme-ic');

  function syncSoundUI() {
    const mode = getSoundMode();
    const label = soundModeLabel(mode);
    const on = isSoundEnabled();
    if (soundLabel) soundLabel.textContent = `交互音效：${label}`;
    soundBtn?.setAttribute('aria-pressed', String(on));
    if (statusSoundOrb) {
      statusSoundOrb.dataset.tip = `交互音效：${label}（点击切换）`;
      statusSoundOrb.setAttribute('aria-pressed', String(on));
      statusSoundOrb.classList.toggle('is-muted', !on);
    }
  }
  function syncThemeUI() {
    const mode = getThemeMode();
    const label = themeModeLabel(mode);
    const iconHref = mode === 'system' ? '#ic-theme' : isEffectiveDark(mode) ? '#ic-moon' : '#ic-sun';
    if (themeLabel) themeLabel.textContent = `外观主题：${label}`;
    themeMenuIc?.setAttribute('href', iconHref);
    statusThemeIc?.setAttribute('href', iconHref);
    if (statusThemeOrb) {
      statusThemeOrb.dataset.tip = `外观主题：${label}（点击切换）`;
      if (statusTrayTip?.classList.contains('is-visible') && statusTrayTip.textContent.startsWith('外观主题：')) {
        statusTrayTip.textContent = statusThemeOrb.dataset.tip;
      }
    }
  }
  syncSoundUI();
  syncThemeUI();
  window.matchMedia?.('(prefers-color-scheme: dark)')?.addEventListener?.('change', syncThemeUI);
  syncRailState();
  initRailGlideTip(document.querySelector('.rail'));

  function setStatusTrayOpen(open, silent = false) {
    if (!statusPickerBtn || !statusTrayWrap || !statusTray) return;
    const wasOpen = statusTrayWrap.classList.contains('is-open');
    statusTrayWrap.classList.toggle('is-open', open);
    statusTrayWrap.setAttribute('aria-hidden', String(!open));
    statusTray.setAttribute('aria-hidden', String(!open));
    statusPickerBtn.setAttribute('aria-expanded', String(open));
    if (!open) statusTrayTip?.classList.remove('is-visible');
    if (!silent && wasOpen !== open) playToggle(open);
  }

  statusPickerBtn?.addEventListener('click', (ev) => {
    ev.stopPropagation();
    setStatusTrayOpen(!statusTrayWrap?.classList.contains('is-open'));
  });
  avatarStatePill?.addEventListener('click', (ev) => {
    ev.stopPropagation();
    setStatusTrayOpen(!statusTrayWrap?.classList.contains('is-open'));
  });
  statusTray?.addEventListener('click', (ev) => ev.stopPropagation());
  statusTray?.addEventListener('pointerleave', () => statusTrayTip?.classList.remove('is-visible'));
  statusTray?.querySelectorAll('.status-orb').forEach((orb) => {
    const showTip = () => {
      if (!statusTrayTip || !statusTray) return;
      statusTrayTip.textContent = orb.dataset.tip || '';
      const centerX = orb.offsetLeft + orb.offsetWidth / 2;
      const trayLeft = statusTray.offsetLeft;
      statusTrayWrap.style.setProperty('--tip-x', `${trayLeft + centerX}px`);
      statusTrayTip.classList.add('is-visible');
    };
    orb.addEventListener('pointerenter', showTip);
    orb.addEventListener('focus', showTip);
    orb.addEventListener('blur', () => statusTrayTip?.classList.remove('is-visible'));
    orb.addEventListener('click', (ev) => {
      const act = orb.dataset.statusAct;
      if (act === 'poke') {
        avatarUI.poke({ force: true });
        showToast('已打招呼，随时待命');
      } else if (act === 'avatar') {
        playTap();
        openAvatarPicker();
      } else if (act === 'search') {
        commandPalette.open();
      } else if (act === 'sound') {
        const nextMode = cycleSoundMode();
        syncSoundUI();
        showToast(`交互音效已切为：${soundModeLabel(nextMode)}`);
        if (statusTrayTip?.classList.contains('is-visible')) {
          statusTrayTip.textContent = orb.dataset.tip || '';
        }
        return;
      } else if (act === 'theme') {
        const nextMode = cycleThemeMode(ev);
        syncThemeUI();
        showToast(`外观主题已切为：${themeModeLabel(nextMode)}`);
        return;
      } else if (act === 'settings') {
        setSettingsOpen(true);
      }
      setStatusTrayOpen(false, true);
    });
  });

  soundBtn?.addEventListener('click', (ev) => {
    ev.stopPropagation();
    const nextMode = cycleSoundMode();
    syncSoundUI();
    showToast(`交互音效：${soundModeLabel(nextMode)}`);
  });
  themeBtn?.addEventListener('click', (ev) => {
    ev.stopPropagation();
    const nextMode = cycleThemeMode(ev);
    syncThemeUI();
    showToast(`外观主题：${themeModeLabel(nextMode)}`);
  });

  document.getElementById('open-settings').onclick = () => {
    railMenu.classList.add('hide');
    setSettingsOpen(true);
  };
  document.getElementById('close-settings').onclick = () => setSettingsOpen(false);

  document.getElementById('logout-btn').onclick = async () => {
    try {
      await api('/auth/logout', { method: 'POST' });
    } finally {
      location.reload();
    }
  };

  await refresh();

  connectEvents({
    onStatus(connected) {
      ssePill.textContent = connected ? '实时同步' : '重连中…';
      sseBolt.classList.toggle('warn', !connected);
      avatarUI.setActivity({ sseConnected: connected });
      if (connected) scheduleRefresh();
    },
    onStage(stage) {
      chatUI.setStage(stage);
    },
    onDelta(delta) {
      chatUI.onDelta(delta);
    },
    onTask(task) {
      chatUI.onTask(task, ++taskEventVersion);
      if (task?.status === 'done') {
        chatUI.flashIslandDone?.();
        playDone();
      } else if (task?.status === 'failed') playWarn();
    },
    onMessage(msg) {
      chatUI.onMessageEvent?.(msg);
    },
    onSync(kind) {
      if (kind === 'resync') chatUI.setStage('');
      if (kind === 'im' && !settingsView.classList.contains('hide')) {
        const active = document.activeElement;
        const editing = active && settingsView.contains(active) && ['INPUT', 'TEXTAREA', 'SELECT'].includes(active.tagName);
        if (!editing) {
          drawerUI.loadIntegrations().then(() => drawerUI.renderSettingsPane());
        }
      }
      scheduleRefresh();
    },
  });
}

// Track the visual viewport (mobile soft keyboard & rotation) so the fixed top bar and workspace fit cleanly.
function initVisualViewport() {
  const vv = window.visualViewport;
  if (!vv) return;
  const root = document.documentElement;
  let frame = 0;
  const apply = () => {
    if (frame) {
      cancelAnimationFrame(frame);
      frame = 0;
    }
    if (vv.height < window.innerHeight - 1 || vv.offsetTop > 0) {
      root.style.setProperty('--vvh', vv.height + 'px');
      root.style.setProperty('--vvt', vv.offsetTop + 'px');
    } else {
      root.style.removeProperty('--vvh');
      root.style.removeProperty('--vvt');
    }
  };
  const schedule = () => {
    if (!frame) frame = requestAnimationFrame(apply);
  };
  vv.addEventListener('resize', apply);
  window.addEventListener('resize', apply);
  vv.addEventListener('scroll', schedule);
  apply();
}

initVisualViewport();
boot();
