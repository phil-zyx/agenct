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
  const api = { kinds, confirmations, confirmer, permissions, status };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.ProjectStateModel = api;
})(globalThis);
