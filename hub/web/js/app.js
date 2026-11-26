// Agenct v2 entrypoint with classic left-rail + centered-chat + floating-avatar Muse drawer layout.
import { api, esc, fmtTime, setCsrfToken, showToast } from './api.js';
import { createAvatarController } from './avatar.js';
import { initChat } from './chat.js';
import { initDrawer } from './drawer.js';
import { connectEvents } from './sse.js';

const state = {
  user: null,
  profile: null,
  roles: [],
  threads: [],
  messages: [],
  has_more_messages: false,
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
const navSearch = document.getElementById('nav-search');
const searchPanel = document.getElementById('search-panel');
const searchInput = document.getElementById('search-input');
const closeSearch = document.getElementById('close-search');
const searchResults = document.getElementById('search-results');
const railBadge = document.getElementById('rail-badge');
const railMore = document.getElementById('rail-more');
const railMenu = document.getElementById('rail-menu');
const museToggle = document.getElementById('muse-head-toggle');

const agentNameEl = document.getElementById('agent-name');
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
  heroWrapEl: heroAvatarWrapEl,
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

function setTasksOpen(open) {
  if (open && isMobile()) setContactsOpen(false);
  roomEl.classList.toggle('tasks-open', open);
  navTasks.setAttribute('aria-pressed', String(open));
  museToggle.setAttribute('aria-expanded', String(open));
}

function setContactsOpen(open) {
  if (open && isMobile()) setTasksOpen(false);
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

  let searchTimer = null;
  function setSearchOpen(open) {
    if (!searchPanel) return;
    if (open && isMobile()) {
      setTasksOpen(false);
      setContactsOpen(false);
    }
    searchPanel.classList.toggle('hide', !open);
    navSearch?.classList.toggle('active', open);
    navSearch?.setAttribute('aria-expanded', String(open));
    if (open) {
      setSettingsOpen(false);
      searchInput?.focus();
    }
  }

  async function runSearch(q) {
    if (!searchResults) return;
    const trimmed = q.trim();
    if (!trimmed) {
      searchResults.innerHTML = '';
      return;
    }
    try {
      const res = await api(`/api/search?q=${encodeURIComponent(trimmed)}`);
      const parts = [];
      if ((res.messages || []).length) {
        parts.push('<div class="search-group-title">对话记录</div>');
        for (const m of res.messages) {
          const thrTitle =
            (state.threads || []).find((t) => t.id === m.thread_id)?.title || m.thread_id || '';
          parts.push(`
            <button type="button" class="search-hit" data-hit-thread="${esc(m.thread_id || '')}" data-hit-title="${esc(thrTitle)}">
              <div class="search-hit-meta">
                <span>${esc(m.role === 'user' ? '我' : 'Agenct')}</span>
                <span>${esc(fmtTime(m.created_at))}</span>
                ${thrTitle ? `<span class="badge">${esc(thrTitle)}</span>` : ''}
              </div>
              <div>${esc(m.snippet)}</div>
            </button>`);
        }
      }
      if ((res.ledgers || []).length) {
        parts.push('<div class="search-group-title">持续事项</div>');
        for (const l of res.ledgers) {
          parts.push(`
            <div class="search-hit">
              <div class="search-hit-meta"><strong>${esc(l.title)}</strong><span class="badge">${esc(l.status)}</span></div>
              <div>${esc((l.notes || '').slice(0, 160))}</div>
            </div>`);
        }
      }
      if ((res.memory_matches || []).length) {
        parts.push('<div class="search-group-title">长期记忆</div>');
        for (const line of res.memory_matches) {
          parts.push(`<div class="search-hit"><div>${esc(line)}</div></div>`);
        }
      }
      searchResults.innerHTML = parts.length ? parts.join('') : '<p class="note">未找到匹配内容</p>';
      searchResults.querySelectorAll('[data-hit-thread]').forEach((btn) => {
        if (!btn.dataset.hitThread) return;
        btn.onclick = () => {
          setSearchOpen(false);
          chatUI.setThreadFilter(btn.dataset.hitThread, btn.dataset.hitTitle);
        };
      });
    } catch (err) {
      searchResults.innerHTML = `<p class="note">搜索失败：${esc(err.message)}</p>`;
    }
  }

  navSearch && (navSearch.onclick = () => setSearchOpen(searchPanel.classList.contains('hide')));
  closeSearch && (closeSearch.onclick = () => setSearchOpen(false));
  searchInput?.addEventListener('input', () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => runSearch(searchInput.value), 220);
  });
  document.addEventListener('keydown', (ev) => {
    if (roomEl.classList.contains('hide')) return;
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === 'k') {
      ev.preventDefault();
      setSearchOpen(searchPanel.classList.contains('hide'));
    } else if (ev.key === 'Escape' && searchPanel && !searchPanel.classList.contains('hide')) {
      setSearchOpen(false);
    }
  });

  // Left rail & floating avatar navigation
  navChat.onclick = () => {
    setSettingsOpen(false);
    if (isMobile()) {
      setTasksOpen(false);
      setContactsOpen(false);
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
    },
    onSync(kind) {
      if (kind === 'resync') chatUI.setStage('');
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
