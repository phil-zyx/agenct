// Agenct v2 entrypoint with classic left-rail + centered-chat + floating-avatar Muse drawer layout.
import { api, avatarById, setCsrfToken, showToast } from './api.js';
import { initChat } from './chat.js';
import { initDrawer } from './drawer.js';
import { connectEvents } from './sse.js';

const state = {
  user: null,
  profile: null,
  threads: [],
  messages: [],
  tasks: [],
  nodes: [],
  ledgers: [],
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
const railBadge = document.getElementById('rail-badge');
const railMore = document.getElementById('rail-more');
const railMenu = document.getElementById('rail-menu');
const museToggle = document.getElementById('muse-head-toggle');

const agentNameEl = document.getElementById('agent-name');
const drawerAgentNameEl = document.getElementById('drawer-agent-name');
const headAvatarEl = document.getElementById('head-avatar');
const drawerAvatarEl = document.getElementById('drawer-avatar');
const heroAvatarWrapEl = document.getElementById('hero-avatar-wrap');
const settingsIdentity = document.getElementById('settings-identity');
const nodePill = document.getElementById('node-pill');
const ssePill = document.getElementById('sse-pill');
const sseBolt = document.getElementById('sse-bolt');

function applyAvatar(avatarId, isBusy) {
  const item = avatarById(avatarId);
  for (const img of [headAvatarEl, drawerAvatarEl]) {
    if (!img) continue;
    if (img.getAttribute('src') !== item.src) img.setAttribute('src', item.src);
    img.classList.toggle('is-portrait', item.animated);
  }
  museToggle?.classList.toggle('has-portrait', item.animated);
  museToggle?.classList.toggle('is-thinking', Boolean(isBusy));
  heroAvatarWrapEl?.classList.toggle('has-portrait', item.animated);
  heroAvatarWrapEl?.classList.toggle('is-thinking', Boolean(isBusy));
}

let chatUI = null;
let drawerUI = null;
let refreshInFlight = null;

function setTasksOpen(open) {
  roomEl.classList.toggle('tasks-open', open);
  navTasks.setAttribute('aria-pressed', String(open));
  museToggle.setAttribute('aria-expanded', String(open));
}

function setContactsOpen(open) {
  contactSidebar.classList.toggle('hide', !open);
  roomEl.classList.toggle('contacts-open', open);
  navContacts.setAttribute('aria-expanded', String(open));
  navContacts.classList.toggle('active', open);
}

function setSettingsOpen(open) {
  settingsView.classList.toggle('hide', !open);
  chatPane.classList.toggle('hide', open);
  navChat.classList.toggle('active', !open);
  if (open) {
    setTasksOpen(false);
    setContactsOpen(false);
    drawerUI.loadIntegrations().then(() => drawerUI.renderSettingsPane());
  }
}

async function refresh() {
  if (refreshInFlight) return refreshInFlight;
  refreshInFlight = (async () => {
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
    applyAvatar(state.profile?.avatar_id, activeTasks > 0);
    chatUI.render();
    drawerUI.render();
  })().finally(() => {
    refreshInFlight = null;
  });
  return refreshInFlight;
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
        loginErr.textContent = err.message === 'invalid login token' ? '登录令牌不正确。' : `登录失败：${err.message}`;
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

  chatUI = initChat({ state, onRefresh: refresh });
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
  });

  // Left rail & floating avatar navigation
  navChat.onclick = () => {
    setSettingsOpen(false);
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
  };
  document.addEventListener('click', () => {
    railMenu.classList.add('hide');
    railMore.setAttribute('aria-expanded', 'false');
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
    },
    onStage(stage) {
      chatUI.setStage(stage);
    },
    onSync() {
      refresh().catch((e) => showToast(e.message));
    },
  });
}

boot();
