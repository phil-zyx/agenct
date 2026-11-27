// Real-time state-driven animated avatar ("buddy") + universal corner status badge.
import { avatarById } from './api.js';
import { playTap } from './sound.js';

const BUDDY_AVATAR_ID = 'buddy';
const IDLE_TO_REST_MS = 35_000;

const STATUS_META = {
  idle: { label: '在线待命', hint: '在线待命' },
  resting: { label: '小憩中', hint: '暂无任务，小憩一会儿' },
  thinking: { label: '思考中', hint: '正在梳理思路与方案' },
  queued: { label: '待接单', hint: '任务已发出，等待节点接单' },
  working: { label: '执行中', hint: '正在专注处理任务' },
  streaming: { label: '回复中', hint: '正在整理回复内容' },
  offline: { label: '重连中', hint: '实时连接中断，正在重连' },
};

const BUDDY_SVG = `
<svg class="buddy-svg" viewBox="0 0 96 96" aria-hidden="true">
  <circle class="buddy-bg" cx="48" cy="48" r="46" />
  <circle class="buddy-halo" cx="48" cy="48" r="41" />
  <g class="buddy-body">
    <path class="buddy-ear left" d="M23 41 18 19 37 29Z" />
    <path class="buddy-ear right" d="M73 41 78 19 59 29Z" />
    <path class="buddy-ear-inner left" d="M24.5 37 21.5 23.5 33.5 30.5Z" />
    <path class="buddy-ear-inner right" d="M71.5 37 74.5 23.5 62.5 30.5Z" />
    <path class="buddy-torso" d="M20 88C21 68 32 61 48 61C64 61 75 68 76 88Z" />
    <rect class="buddy-hood" x="22" y="24" width="52" height="44" rx="24" />
    <rect class="buddy-face-pad" x="27" y="30" width="42" height="33" rx="18" />
    <ellipse class="buddy-cheek" cx="34" cy="51" rx="4.2" ry="2.6" />
    <ellipse class="buddy-cheek" cx="62" cy="51" rx="4.2" ry="2.6" />

    <g class="buddy-face face-idle">
      <g class="buddy-blink">
        <ellipse class="buddy-eye" cx="39.5" cy="44.5" rx="3" ry="3.8" />
        <ellipse class="buddy-eye" cx="56.5" cy="44.5" rx="3" ry="3.8" />
        <circle class="buddy-eye-glint" cx="40.6" cy="43.1" r="1.1" />
        <circle class="buddy-eye-glint" cx="57.6" cy="43.1" r="1.1" />
      </g>
      <path class="buddy-mouth" d="M44 52.5Q48 56 52 52.5" />
    </g>

    <g class="buddy-face face-resting">
      <path class="buddy-eye-closed" d="M35.5 45Q39.5 48.2 43.5 45" />
      <path class="buddy-eye-closed" d="M52.5 45Q56.5 48.2 60.5 45" />
      <circle class="buddy-mouth-sleep" cx="48" cy="53.2" r="2" />
    </g>

    <g class="buddy-face face-thinking">
      <ellipse class="buddy-eye" cx="39" cy="43" rx="2.9" ry="3.6" />
      <ellipse class="buddy-eye" cx="56" cy="43" rx="2.9" ry="3.6" />
      <circle class="buddy-eye-glint" cx="40.3" cy="41.6" r="1.1" />
      <circle class="buddy-eye-glint" cx="57.3" cy="41.6" r="1.1" />
      <path class="buddy-brow" d="M35.5 38Q39 36.2 42.5 37.8" />
      <path class="buddy-mouth" d="M44.5 53Q48 51.5 51.5 53" />
    </g>

    <g class="buddy-face face-queued">
      <ellipse class="buddy-eye" cx="40.5" cy="43.8" rx="3.1" ry="3.7" />
      <ellipse class="buddy-eye" cx="57.5" cy="43.8" rx="3.1" ry="3.7" />
      <circle class="buddy-eye-glint" cx="41.8" cy="42.4" r="1.15" />
      <circle class="buddy-eye-glint" cx="58.8" cy="42.4" r="1.15" />
      <path class="buddy-mouth" d="M45 53Q48 54.8 51 53" />
    </g>

    <g class="buddy-face face-working">
      <ellipse class="buddy-eye" cx="39.5" cy="44.8" rx="3.1" ry="3.5" />
      <ellipse class="buddy-eye" cx="56.5" cy="44.8" rx="3.1" ry="3.5" />
      <circle class="buddy-eye-glint" cx="40.7" cy="43.6" r="1.1" />
      <circle class="buddy-eye-glint" cx="57.7" cy="43.6" r="1.1" />
      <path class="buddy-brow" d="M35.5 39.5 42.8 41.2" />
      <path class="buddy-brow" d="M60.5 39.5 53.2 41.2" />
      <path class="buddy-mouth" d="M44.5 53Q48 55 51.5 53" />
    </g>

    <g class="buddy-face face-streaming">
      <path class="buddy-eye-happy" d="M35.5 45.5Q39.5 41 43.5 45.5" />
      <path class="buddy-eye-happy" d="M52.5 45.5Q56.5 41 60.5 45.5" />
      <path class="buddy-mouth-open" d="M43.5 51.5Q48 57.5 52.5 51.5Z" />
    </g>

    <g class="buddy-face face-offline">
      <path class="buddy-eye-dizzy" d="M36 42 42 45 36 48" />
      <path class="buddy-eye-dizzy" d="M60 42 54 45 60 48" />
      <path class="buddy-mouth" d="M44 54Q48 51.5 52 54" />
    </g>
  </g>

  <g class="buddy-scene scene-idle">
    <path class="buddy-steam steam-1" d="M44 66Q42 62 45 58" />
    <path class="buddy-steam steam-2" d="M51.5 65Q53.5 61 50.5 57" />
    <ellipse class="buddy-saucer" cx="48" cy="82.5" rx="13.5" ry="3.2" />
    <path class="buddy-cup" d="M39.5 69.5H56.5V76.5C56.5 80.5 52.8 82.5 48 82.5C43.2 82.5 39.5 80.5 39.5 76.5Z" />
    <path class="buddy-cup-handle" d="M56.5 71.5C60 71.5 60.5 77.5 56 78.2" />
    <circle class="buddy-paw" cx="35.5" cy="75.5" r="4.8" />
    <circle class="buddy-paw" cx="60.5" cy="75.5" r="4.8" />
  </g>

  <g class="buddy-scene scene-resting">
    <ellipse class="buddy-paw" cx="38" cy="76" rx="6" ry="4.5" />
    <ellipse class="buddy-paw" cx="58" cy="76" rx="6" ry="4.5" />
    <path class="sleep-z z1" d="M63 32H69L63 38H69" />
    <path class="sleep-z z2" d="M70 23H77L70 30H77" />
    <path class="sleep-z z3" d="M77 14H83L77 20H83" />
  </g>

  <g class="buddy-scene scene-thinking">
    <circle class="buddy-paw think-paw" cx="36.5" cy="64.5" r="5" />
    <circle class="buddy-paw" cx="59.5" cy="76.5" r="4.8" />
    <g class="think-card">
      <rect class="buddy-prop-card" x="62" y="59" width="20" height="15" rx="3.5" />
      <line class="buddy-prop-line" x1="66" y1="64" x2="78" y2="64" />
      <line class="buddy-prop-line" x1="66" y1="68.5" x2="74" y2="68.5" />
    </g>
    <path class="think-star" d="M75 18 76.9 23.1 82 25 76.9 26.9 75 32 73.1 26.9 68 25 73.1 23.1Z" />
  </g>

  <g class="buddy-scene scene-queued">
    <circle class="buddy-paw" cx="36" cy="76" r="4.8" />
    <circle class="buddy-paw" cx="58" cy="76" r="4.8" />
    <g class="queued-glass">
      <line class="buddy-glass-cap" x1="66.5" y1="56" x2="79.5" y2="56" />
      <line class="buddy-glass-cap" x1="66.5" y1="74" x2="79.5" y2="74" />
      <path class="buddy-glass-body" d="M68 56.5H78L73 65L78 73.5H68L73 65Z" />
    </g>
    <circle class="queued-dot" cx="76" cy="24" r="3.6" />
  </g>

  <g class="buddy-scene scene-working">
    <rect class="buddy-console" x="23" y="69" width="50" height="15" rx="5.5" />
    <circle class="buddy-key k1" cx="33" cy="76.5" r="2.1" />
    <circle class="buddy-key k2" cx="42" cy="76.5" r="2.1" />
    <circle class="buddy-key k3" cx="54" cy="76.5" r="2.1" />
    <circle class="buddy-key k4" cx="63" cy="76.5" r="2.1" />
    <circle class="buddy-paw paw-left" cx="35.5" cy="71" r="5" />
    <circle class="buddy-paw paw-right" cx="60.5" cy="71" r="5" />
    <rect class="code-spark s1" x="26" y="54" width="6.5" height="3.2" rx="1.6" />
    <rect class="code-spark s2" x="45" y="51" width="7.5" height="3.2" rx="1.6" />
    <rect class="code-spark s3" x="64" y="54" width="6.5" height="3.2" rx="1.6" />
  </g>

  <g class="buddy-scene scene-streaming">
    <circle class="buddy-paw paw-wave-left" cx="35.5" cy="73.5" r="5" />
    <circle class="buddy-paw paw-wave-right" cx="60.5" cy="73.5" r="5" />
    <rect class="stream-bubble" x="62" y="16" width="22" height="15" rx="7" />
    <circle class="wave-dot d1" cx="67.5" cy="23.5" r="1.9" />
    <circle class="wave-dot d2" cx="73" cy="23.5" r="1.9" />
    <circle class="wave-dot d3" cx="78.5" cy="23.5" r="1.9" />
  </g>

  <g class="buddy-scene scene-offline">
    <circle class="buddy-paw" cx="37" cy="76" r="4.8" />
    <circle class="buddy-paw" cx="59" cy="76" r="4.8" />
    <path class="offline-arc a1" d="M65 28A10 10 0 0 1 78 21" />
    <path class="offline-arc a2" d="M68.5 31.5A6 6 0 0 1 76.5 26" />
    <circle class="offline-dot" cx="74" cy="32.5" r="2.2" />
  </g>
</svg>`.trim();

const BADGE_SVG = `
<svg class="avatar-status-svg" viewBox="0 0 24 24" aria-hidden="true">
  <g class="badge-icon icon-idle">
    <path d="M6 10h9v4a4 4 0 0 1-4 4h-1a4 4 0 0 1-4-4v-4Z" />
    <path d="M15 11h1.8a2 2 0 0 1 0 4H15" />
    <path d="M8.5 6.5c0-1 1-1.5 1-2.5M12 6.5c0-1 1-1.5 1-2.5" />
  </g>
  <g class="badge-icon icon-resting">
    <path d="M15.5 15.5A6 6 0 0 1 8.5 8.5a6 6 0 1 0 7 7Z" />
  </g>
  <g class="badge-icon icon-thinking">
    <path d="m12 4 1.8 5.2L19 11l-5.2 1.8L12 18l-1.8-5.2L5 11l5.2-1.8Z" />
  </g>
  <g class="badge-icon icon-queued">
    <path d="M7.5 5.5h9M7.5 18.5h9M8.5 5.5 12 12l3.5-6.5M8.5 18.5 12 12l3.5 6.5" />
  </g>
  <g class="badge-icon icon-working">
    <rect x="4" y="6.5" width="16" height="11" rx="2.5" />
    <path d="M7.5 10.5h2M11.5 10.5h5M7.5 13.8h9" />
  </g>
  <g class="badge-icon icon-streaming">
    <path d="M5 12a7 7 0 1 1 3 5.7L5 19l1-3.1A6.9 6.9 0 0 1 5 12Z" />
    <path d="M9.5 12h.01M12 12h.01M14.5 12h.01" />
  </g>
  <g class="badge-icon icon-offline">
    <path d="M6.5 9.5a8.5 8.5 0 0 1 11 0M9.2 12.6a4.5 4.5 0 0 1 5.6 0" />
    <path d="M12 16.5h.01" />
  </g>
</svg>`.trim();

export function renderBuddySvg() {
  return BUDDY_SVG;
}

function deriveAgentState({
  sseConnected = true,
  brainStage = '',
  streaming = false,
  taskStatus = '',
  taskStage = '',
  resting = false,
} = {}) {
  if (!sseConnected) return 'offline';
  if (taskStatus === 'running') {
    const st = String(taskStage || '').toLowerCase();
    if (st.includes('plan') || st.includes('readonly') || st.includes('只读')) {
      return 'thinking';
    }
    return 'working';
  }
  if (taskStatus === 'queued') return 'queued';
  if (streaming) return 'streaming';
  if (brainStage) return 'thinking';
  if (resting) return 'resting';
  return 'idle';
}

export function createAvatarController({
  headToggleEl,
  headAvatarEl,
  headBuddyEl,
  headBadgeEl,
  headStateTextEl,
  heroWrapEl,
  chatPaneEl,
  drawerAvatarEl,
  drawerBuddyEl,
  drawerBadgeEl,
  statusPillEl,
}) {
  if (headBuddyEl) headBuddyEl.innerHTML = BUDDY_SVG;
  if (drawerBuddyEl) drawerBuddyEl.innerHTML = BUDDY_SVG;
  if (headBadgeEl) headBadgeEl.innerHTML = BADGE_SVG;
  if (drawerBadgeEl) drawerBadgeEl.innerHTML = BADGE_SVG;

  let avatarId = 'agenct';
  let resting = false;
  let idleTimer = null;
  let pokeTimer = null;
  let activity = {
    sseConnected: true,
    brainStage: '',
    streaming: false,
    taskStatus: '',
    taskStage: '',
  };

  function isActive() {
    return (
      !activity.sseConnected ||
      Boolean(activity.brainStage) ||
      Boolean(activity.streaming) ||
      activity.taskStatus === 'running' ||
      activity.taskStatus === 'queued'
    );
  }

  function scheduleRestTimer() {
    if (isActive()) {
      clearTimeout(idleTimer);
      idleTimer = null;
      resting = false;
      return;
    }
    if (resting || idleTimer !== null) return;
    idleTimer = setTimeout(() => {
      idleTimer = null;
      if (!isActive()) {
        resting = true;
        applyState();
      }
    }, IDLE_TO_REST_MS);
  }

  function applyState() {
    const next = deriveAgentState({ ...activity, resting });
    const meta = STATUS_META[next] || STATUS_META.idle;
    for (const el of [headToggleEl, heroWrapEl, chatPaneEl]) {
      if (!el) continue;
      if (el.dataset.agentState !== next) el.dataset.agentState = next;
    }
    if (headStateTextEl) {
      const custom = typeof activity.statusLabel === 'string' ? activity.statusLabel.trim() : '';
      const subText = next === 'idle' || next === 'resting' || next === 'offline'
        ? meta.label
        : (custom || meta.label);
      if (headStateTextEl.textContent !== subText) headStateTextEl.textContent = subText;
    }
    if (statusPillEl) {
      if (statusPillEl.textContent !== meta.label) statusPillEl.textContent = meta.label;
      if (statusPillEl.dataset.agentState !== next) statusPillEl.dataset.agentState = next;
    }
    if (headToggleEl) {
      const nextTitle = `管家工作台 · ${meta.label}`;
      if (headToggleEl.title !== nextTitle) headToggleEl.title = nextTitle;
    }
  }

  function setAvatar(id) {
    const item = avatarById(id);
    avatarId = item.id;
    const liveBuddy = Boolean(item.liveSvg);

    for (const img of [headAvatarEl, drawerAvatarEl]) {
      if (!img) continue;
      img.classList.toggle('hide', liveBuddy);
      if (!liveBuddy && item.src && img.getAttribute('src') !== item.src) {
        img.setAttribute('src', item.src);
      }
      img.classList.toggle('is-portrait', !liveBuddy && item.animated);
    }
    for (const buddy of [headBuddyEl, drawerBuddyEl]) {
      if (buddy) buddy.classList.toggle('hide', !liveBuddy);
    }
    for (const host of [headToggleEl, heroWrapEl]) {
      if (!host) continue;
      host.classList.toggle('is-live-buddy', liveBuddy);
      host.classList.toggle('has-portrait', Boolean(item.animated));
    }
    applyState();
  }

  function setActivity(patch) {
    activity = { ...activity, ...patch };
    scheduleRestTimer();
    applyState();
  }

  function poke({ force = false } = {}) {
    if (!heroWrapEl) return false;
    const cur = heroWrapEl.dataset.agentState;
    if (!force && (avatarId !== BUDDY_AVATAR_ID || (cur !== 'idle' && cur !== 'resting'))) {
      return false;
    }
    playTap();
    resting = false;
    clearTimeout(idleTimer);
    idleTimer = null;
    scheduleRestTimer();
    applyState();
    delete heroWrapEl.dataset.agentPoke;
    void heroWrapEl.offsetWidth;
    heroWrapEl.dataset.agentPoke = '1';
    clearTimeout(pokeTimer);
    pokeTimer = setTimeout(() => {
      if (heroWrapEl.dataset.agentPoke) delete heroWrapEl.dataset.agentPoke;
    }, 900);
    return true;
  }

  if (heroWrapEl) {
    heroWrapEl.addEventListener('click', () => poke());
  }

  scheduleRestTimer();
  applyState();

  return { setAvatar, setActivity, poke };
}
