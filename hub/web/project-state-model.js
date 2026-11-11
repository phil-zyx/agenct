/* Presentation rules only. The Hub remains authoritative for every permission. */
(function (root) {
  const kinds = { goal: '目标', assignment: '分工', blocker: '阻塞', decision: '待决定项' };
  const confirmations = { goal: '确认这项目标', assignment: '确认这项分工', blocker: '确认这个阻塞', decision: '确认这项决定' };
  function confirmer(project, item) {
    return item.kind === 'assignment' ? item.assignee : item.kind === 'blocker' ? item.created_by : project.creator;
  }
  function permissions(project, item, user) {
    const active = project.status === 'active' && item.status !== 'withdrawn';
    const editor = user === project.creator || user === item.created_by;
    const named = user === confirmer(project, item);
    return { confirm: active && item.status === 'proposed' && named, revise: active && editor, withdraw: active && (editor || named) };
  }
  function status(item) {
    if (item.status === 'withdrawn') return '已撤回';
    if (item.status === 'confirmed' && item.confirmation_valid) return '已由 ' + item.confirmed_by + ' 确认';
    return item.status === 'confirmed' ? '确认已失效，需重新核对' : '待本人确认';
  }
  function chatCandidate(message, owner) {
    if (!message || message.status !== 'complete' || !['user', 'assistant'].includes(message.role)
        || typeof message.content !== 'string' || !message.content.trim()
        || (message.role === 'assistant' && /^\[(系统提示|MOCK)/.test(message.content))) return null;
    const model = message.role === 'assistant';
    // Only metadata is added to provenance. No neighboring/private message text is copied.
    const timestamp = typeof message.created_at === 'string' && /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(message.created_at)
      ? message.created_at + ' UTC' : '时间未记录';
    return Object.freeze({ content: message.content, source_kind: model ? 'model_suggestion' : 'human_statement',
      source_note: '由 ' + owner + ' 从自己的私聊中选取并整理；来源：' + (model ? '助手回复（模型建议，不是任何人的承诺）' : '本人消息') + '；' + timestamp + '。原始私聊不向成员开放。' });
  }
  function boundedText(value, maximum) {
    return typeof value === 'string' && !!value.trim() && [...value.trim()].length <= maximum
      && !/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/.test(value);
  }
  const api = { kinds, confirmations, confirmer, permissions, status, chatCandidate, boundedText };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.ProjectStateModel = api;
})(globalThis);
