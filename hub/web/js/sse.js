// Single Server-Sent Events (SSE) subscription replacing 7x3s polling.
export function connectEvents({ onStatus, onStage, onSync }) {
  let es = null;
  let retryMs = 1500;
  let closed = false;

  function open() {
    if (closed) return;
    es = new EventSource('/api/events', { withCredentials: true });
    es.addEventListener('connected', () => {
      retryMs = 1500;
      onStatus(true);
    });
    es.addEventListener('stage', (ev) => {
      try {
        const d = JSON.parse(ev.data);
        onStage(d.stage || '');
      } catch {
        onStage('');
      }
    });
    for (const kind of ['message', 'task', 'node', 'brain', 'automation', 'resync']) {
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
