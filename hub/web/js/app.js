// Minimalist Agenct v2 entrypoint: single /api/bootstrap + single SSE EventSource.
import { api, setCsrfToken, showToast } from './api.js';
import { initChat } from './chat.js';
import { initDrawer } from './drawer.js';
import { connectEvents } from './sse.js';

const state = {
  user: null,
  profile: null,
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
const appView = document.getElementById('app-view');
const loginErr = document.getElementById('login-error');
const agentNameEl = document.getElementById('agent-name');
const userLabelEl = document.getElementById('user-label');
const nodePill = document.getElementById('node-pill');
const ssePill = document.getElementById('sse-pill');

let chatUI = null;
let drawerUI = null;
let refreshInFlight = null;

async function refresh() {
  if (refreshInFlight) return refreshInFlight;
  refreshInFlight = (async () => {
    const data = await api('/api/bootstrap');
    Object.assign(state, data);
    agentNameEl.textContent = state.profile?.name || 'Agenct';
    userLabelEl.textContent = state.user ? `${state.user.display_name} (@${state.user.username})` : '';
    const onlineCount = (state.nodes || []).filter((n) => n.online).length;
    nodePill.textContent = `${onlineCount} 节点在线`;
    nodePill.className = `pill ${onlineCount > 0 ? 'ok' : 'muted'}`;
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
    if (sess?.reason === 'disabled') {
      loginErr.textContent = '该账号已被停用，请联系管理员。';
      loginErr.classList.remove('hide');
    }
    return;
  }

  setCsrfToken(sess.csrf_token);
  loginView.classList.add('hide');
  appView.classList.remove('hide');

  chatUI = initChat({ state, onRefresh: refresh });
  drawerUI = initDrawer({ state, onRefresh: refresh });

  document.getElementById('toggle-drawer').onclick = () => {
    appView.classList.toggle('drawer-closed');
  };
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
      ssePill.className = `pill ${connected ? 'ok' : 'warn'}`;
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
