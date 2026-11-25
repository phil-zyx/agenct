const CONFIRMATION = /^(?:待(?:用户|人工)?(?:确认|决策)(?:\s*[:：]|$)|(?:请(?:你|您|用户)?|(?:仍)?需要(?:你|您|用户|人工))(?:先)?(?:确认|决定|选择)|(?:please confirm|(?:user|human) (?:confirmation|approval) (?:is )?required)\b)/i;

export function stripNoise(raw) {
  return String(raw || '')
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '')
    .replace(/\u25af\[\d+(?:;\d+)*m/g, '')
    .replace(/\r\n/g, '\n')
    .trim();
}

function previewLines(raw) {
  return stripNoise(raw)
    .replace(/^[ \t]*```[^\n]*(?:\n[\s\S]*?^[ \t]*```[^\n]*|[\s\S]*$)/gm, '')
    .split('\n')
    .map((line) => line.trim().replace(/^(?:#+\s*|>\s*|[-*+]\s+|\d+[.)]\s+)+/, '').replace(/\*\*/g, '').trim())
    .filter((line) => line && !/^[-*_]{3,}$/.test(line));
}

function cleanPreviewLine(raw, limit = 96) {
  const line = previewLines(raw)[0] || '';
  return line.length > limit ? `${line.slice(0, limit)}…` : line;
}

function externalLinks(task) {
  const links = new Set();
  for (const raw of [task.result, task.instruction]) {
    const matches = stripNoise(raw).matchAll(/https?:\/\/[^\s<>"'`()，。；！？、]+/gi);
    for (const [match] of matches) {
      try {
        const url = new URL(match.replace(/[.,;!?]+$/, ''));
        if (!url.username && !url.password) links.add(url.href);
      } catch {
        continue;
      }
      if (links.size === 3) return [...links];
    }
  }
  return [...links];
}

export function triageTasks(tasks) {
  const groups = { decision: [], conclusion: [], summary: [] };
  for (const task of tasks) {
    const confirmation = task.status === 'done' && previewLines(task.result).find((line) => CONFIRMATION.test(line));
    if (task.status === 'failed' || confirmation) {
      groups.decision.push({ task, preview: cleanPreviewLine(confirmation || task.result) });
    } else if (task.status === 'done' || task.status === 'canceled') {
      groups.conclusion.push({ task, preview: cleanPreviewLine(task.result) });
    } else {
      groups.summary.push({
        task,
        preview: cleanPreviewLine(task.result || task.instruction),
        progress: {
          stage: stripNoise(task.stage),
          elapsed: Number.isFinite(task.elapsed_secs) && task.elapsed_secs >= 0 ? task.elapsed_secs : null,
          logTail: stripNoise(task.log_tail).slice(-360),
          links: externalLinks(task),
        },
      });
    }
  }
  return groups;
}
