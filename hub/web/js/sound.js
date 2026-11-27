// Synthesized interface micro-sounds using Web Audio API (no external asset files).
// Supports 3 modes: 'bubble' (soft warm), 'mech' (crisp mechanical), and 'off' (muted).
const STORAGE_KEY = 'agenct_ui_sound';
const MODES = ['bubble', 'mech', 'off'];
const MODE_LABELS = { bubble: '柔和', mech: '清脆', off: '静音' };

let audioCtx = null;

export function getSoundMode() {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw === '0' || raw === 'off') return 'off';
    if (raw === 'mech') return 'mech';
    return 'bubble';
  } catch {
    return 'bubble';
  }
}

export function soundModeLabel(mode = getSoundMode()) {
  return MODE_LABELS[mode] || MODE_LABELS.bubble;
}

export function isSoundEnabled() {
  return getSoundMode() !== 'off';
}

export function setSoundMode(mode) {
  const next = MODES.includes(mode) ? mode : 'bubble';
  try {
    localStorage.setItem(STORAGE_KEY, next);
  } catch {
    // ignore storage failures
  }
  if (next !== 'off') playToggle(true);
  return next;
}

export function cycleSoundMode() {
  const cur = getSoundMode();
  const next = MODES[(MODES.indexOf(cur) + 1) % MODES.length];
  return setSoundMode(next);
}

function getContext() {
  try {
    const Ctx = window.AudioContext || window.webkitAudioContext;
    if (!Ctx) return null;
    if (!audioCtx) audioCtx = new Ctx();
    if (audioCtx.state === 'suspended') {
      audioCtx.resume().catch(() => {});
    }
    return audioCtx.state === 'running' ? audioCtx : null;
  } catch {
    return null;
  }
}

function tone({ freq, endFreq = freq, dur = 0.04, gain = 0.045, type = 'sine', delay = 0 }) {
  const mode = getSoundMode();
  if (mode === 'off') return;
  const ctx = getContext();
  if (!ctx) return;
  try {
    const start = ctx.currentTime + delay;
    const stop = start + dur;
    const osc = ctx.createOscillator();
    const env = ctx.createGain();
    osc.type = type;
    osc.frequency.setValueAtTime(freq, start);
    if (endFreq !== freq) {
      osc.frequency.exponentialRampToValueAtTime(Math.max(40, endFreq), stop);
    }
    env.gain.setValueAtTime(0.0001, start);
    env.gain.exponentialRampToValueAtTime(gain, start + Math.min(0.006, dur * 0.25));
    env.gain.exponentialRampToValueAtTime(0.0001, stop);
    osc.connect(env);
    env.connect(ctx.destination);
    osc.start(start);
    osc.stop(stop + 0.005);
  } catch {
    // ignore audio scheduling errors
  }
}

export function playTap() {
  if (getSoundMode() === 'mech') {
    tone({ freq: 1450, endFreq: 720, dur: 0.018, gain: 0.036, type: 'square' });
    return;
  }
  tone({ freq: 880, endFreq: 520, dur: 0.028, gain: 0.04, type: 'triangle' });
}

export function playToggle(on = true) {
  if (getSoundMode() === 'mech') {
    if (on) {
      tone({ freq: 920, endFreq: 1380, dur: 0.024, gain: 0.035, type: 'triangle' });
    } else {
      tone({ freq: 1120, endFreq: 680, dur: 0.024, gain: 0.034, type: 'triangle' });
    }
    return;
  }
  if (on) {
    tone({ freq: 520, endFreq: 780, dur: 0.042, gain: 0.048, type: 'sine' });
  } else {
    tone({ freq: 640, endFreq: 420, dur: 0.042, gain: 0.045, type: 'sine' });
  }
}

export function playSend() {
  if (getSoundMode() === 'mech') {
    tone({ freq: 680, endFreq: 1180, dur: 0.034, gain: 0.038, type: 'triangle' });
    tone({ freq: 1480, endFreq: 1680, dur: 0.022, gain: 0.024, type: 'sine', delay: 0.022 });
    return;
  }
  tone({ freq: 380, endFreq: 680, dur: 0.065, gain: 0.052, type: 'sine' });
}

export function playDone() {
  if (getSoundMode() === 'mech') {
    tone({ freq: 988, dur: 0.032, gain: 0.032, type: 'triangle', delay: 0 });
    tone({ freq: 1319, dur: 0.05, gain: 0.034, type: 'sine', delay: 0.036 });
    return;
  }
  tone({ freq: 587, dur: 0.065, gain: 0.042, type: 'sine', delay: 0 });
  tone({ freq: 880, dur: 0.085, gain: 0.044, type: 'sine', delay: 0.048 });
}

export function playWarn() {
  if (getSoundMode() === 'mech') {
    tone({ freq: 340, endFreq: 210, dur: 0.035, gain: 0.04, type: 'sawtooth', delay: 0 });
    tone({ freq: 260, endFreq: 165, dur: 0.04, gain: 0.036, type: 'sawtooth', delay: 0.042 });
    return;
  }
  tone({ freq: 260, endFreq: 195, dur: 0.05, gain: 0.048, type: 'triangle', delay: 0 });
  tone({ freq: 220, endFreq: 170, dur: 0.055, gain: 0.042, type: 'triangle', delay: 0.055 });
}
