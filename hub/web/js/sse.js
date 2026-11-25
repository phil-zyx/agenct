// Single Server-Sent Events (SSE) subscription replacing 7x3s polling.
export function connectEvents({ onStatus, onStage, onDelta, onTask, onSync }) {
  let es = null;
  let retryMs = 1500;
  let closed = false;
  let hasConnectedOnce = false;

  function open() {
    if (closed) return;
    es = new EventSource('/api/events', { withCredentials: true });
    es.addEventListener('connected', () => {
      const isReconnect = hasConnectedOnce;
      hasConnectedOnce = true;
      retryMs = 1500;
      onStatus(true);
      if (isReconnect) onSync('resync');
    });
    es.addEventListener('stage', (ev) => {
      try {
        const d = JSON.parse(ev.data);
        onStage(d.stage || '');
      } catch {
        onStage('');
      }
    });
    es.addEventListener('delta', (ev) => {
      if (!onDelta) return;
      try {
        onDelta(JSON.parse(ev.data));
      } catch {
        // ignore malformed delta
      }
    });
    es.addEventListener('task', (ev) => {
      let task;
      try {
        task = JSON.parse(ev.data);
      } catch {
        // A bootstrap refresh can still reconcile malformed task events.
      }
      if (task) onTask(task);
      onSync('task');
    });
    for (const kind of ['message', 'node', 'brain', 'automation', 'resync']) {
      es.addEventListener(kind, () => onSync(kind));
    }
    es.onerror = () => {
      onStatus(false);
      es.close();
      if (!closed) {
        setTimeout(open, retryMs);
        retryMs = Math.min(retryMs * 2, 15000);
      }
    };
  }

  open();
  return () => {
    closed = true;
    if (es) es.close();
  };
}
