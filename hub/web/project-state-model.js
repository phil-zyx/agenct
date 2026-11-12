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
  function alignment(project, items) {
    // A deterministic, read-only projection of an already authorized snapshot.
    // Do not consume free text, private chat, tasks, drafts or model output here.
    if (!project || project.status !== 'active' || typeof project.id !== 'string'
        || !Number.isSafeInteger(project.revision) || !Array.isArray(items)
        || items.some(item => !item || item.project_id !== project.id || typeof item.id !== 'string'
          || !Number.isSafeInteger(item.version) || !Object.hasOwn(kinds, item.kind)
          || !['proposed', 'confirmed', 'withdrawn'].includes(item.status))
        || new Set(items.map(item => item.id)).size !== items.length) return null;
    const live = items.filter(item => item.status !== 'withdrawn').sort((a, b) => a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
    const suggestions = [];
    const add = (key, title, detail, sources = []) => suggestions.push({ key, title, detail,
      sources: sources.map(item => ({ item_id: item.id, version: item.version })) });
    if (!live.some(item => item.kind === 'goal')) add('missing_goal', '核对是否需要共同目标',
      '当前没有未撤回的目标条目。如需共同目标，可先讨论再手动发布。项目说明不代表成员已确认。');
    if (!live.some(item => item.kind === 'assignment')) add('missing_assignment', '核对是否需要明确分工',
      '当前没有未撤回的分工条目。未发布不代表无人负责；如需分工，请先讨论人选，再由负责人本人确认。');
    const proposed = live.filter(item => item.status === 'proposed');
    if (proposed.length) add('pending_confirmation', proposed.length + ' 条等待具名确认',
      '建议按来源逐条核对，由条目指定的人自行决定是否确认。待确认不表示存在分歧，也不是全员共识。', proposed);
    const invalid = live.filter(item => item.status === 'confirmed' && item.confirmation_valid !== true);
    if (invalid.length) add('invalid_confirmation', invalid.length + ' 条确认已失效',
      '建议先核对成员资格和当前版本。旧确认不能作为当前承诺；需要更新时，请按原有修订和本人确认流程处理。', invalid);
    const blockers = live.filter(item => item.kind === 'blocker');
    if (blockers.length) add('reported_blockers', '核对 ' + blockers.length + ' 条已发布阻塞',
      '建议与报告人核对是否仍需处理，再讨论下一步。条目状态不代表阻塞已解决或仍未解决。', blockers);
    return { project_id: project.id, project_revision: project.revision, item_count: items.length, suggestions };
  }
  const api = { kinds, confirmations, confirmer, permissions, status, chatCandidate, boundedText, alignment };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.ProjectStateModel = api;
})(globalThis);
