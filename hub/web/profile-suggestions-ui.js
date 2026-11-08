/* Model-proposed personal changes remain private and inert until explicitly confirmed. */
const profileSuggestionActions = new Map();
const PROFILE_CHANGE_FIELDS = {
  public: { team: '团队', responsibilities: '负责的工作', help_with: '可以找我帮忙', request_requirements: '发请求时请提供' },
  private: { work_context: '工作背景', response_preferences: '回复偏好' },
  permissions: { auto_reply_public: '依据公开资料答复', auto_prepare_drafts: '接手后准备私密草稿' }
};
function editPersonalInfo(prefix = '记住：') {
  const context = session(); if (!current(context)) return;
  restorePrivateChat();
  if (!$('text').value.trim()) { $('text').value = prefix; $('text').oninput?.(); }
  else status('已保留输入框里的内容。可以直接告诉我想记住或修改什么。');
  $('text').focus?.();
}
function resetProfileSuggestions() { profileSuggestionActions.clear(); }
function renderProfileSuggestions(context, suggestions) {
  if (!Array.isArray(suggestions)) return [];
  return [...suggestions].reverse().filter(item => item && ['pending', 'confirmed', 'rejected'].includes(item.status)).map(item => {
    const card = el('article', 'profile-suggestion' + (item.status === 'pending' ? '' : ' settled'));
    card.dataset.suggestion = item.id;
    card.append(el('h3', null, item.status === 'confirmed' ? '已记住' : item.status === 'rejected' ? '这次没有保存' : '这样记住，可以吗？'));
    const values = el('dl', 'profile-change-list');
    for (const [group, names] of Object.entries(PROFILE_CHANGE_FIELDS)) {
      for (const [key, value] of Object.entries(item.changes?.[group] || {})) {
        if (!names[key]) continue;
        const label = names[key] + (group === 'public' ? ' · 同事可见' : group === 'private' ? ' · 仅我和助手' : ' · 助手权限');
        values.append(el('dt', null, label), el('dd', null, typeof value === 'boolean' ? value ? '允许' : '不允许' : value || '清除这项内容'));
      }
    }
    card.append(values);
    if (item.status !== 'pending') {
      const details = el('details', 'profile-suggestion-saved');
      details.append(el('summary', null, '查看这次记录'), values); card.append(details);
    }
    const scopeNote = Object.keys(item.changes?.public || {}).length ? '确认后，标记为同事可见的信息会向有权联系你的同事公开。'
      : Object.keys(item.changes?.permissions || {}).length ? '确认后，助手会按上面的代办范围处理。' : '这些内容只供你和自己的助手使用，不会公开给同事。';
    const note = el('p', 'note', item.status === 'pending' ? scopeNote : item.status === 'confirmed' ? '以后会按这些资料和偏好与你协作。' : '你的原有资料保持不变。');
    note.setAttribute('role', 'status'); card.append(note);
    if (item.status !== 'pending') return card;
    const actions = el('div', 'actions');
    const confirm = el('button', 'primary', '确认记住'); confirm.type = 'button';
    const reject = el('button', null, '暂不保存'); reject.type = 'button';
    const revise = el('button', 'text-btn', '继续修改'); revise.type = 'button';
    revise.onclick = () => { if (current(context)) editPersonalInfo('把刚才的个人设置改为：'); };
    const stored = profileSuggestionActions.get(item.id);
    if (stored?.pending) { confirm.disabled = true; reject.disabled = true; }
    if (stored?.error) note.textContent = stored.error;
    async function act(action) {
      if (!current(context) || profileSuggestionActions.get(item.id)?.pending) return;
      const operation = { pending: true, error: '' }; profileSuggestionActions.set(item.id, operation);
      const busyKey = 'profile-suggestion-' + item.id; busy.add(busyKey);
      confirm.disabled = true; reject.disabled = true;
      try {
        await contactApi(context, 'profile-suggestions/' + encodeURIComponent(item.id) + '/actions', 'POST', { action });
        if (!current(context)) return;
        profileSuggestionActions.delete(item.id);
        status(action === 'confirm' ? '已记住。' : '这次没有保存。');
        await refresh(context);
      } catch (error) {
        if (!current(context)) return;
        operation.error = error.httpStatus === 409
          ? '资料或这条建议已更新，请在聊天中重新说明要修改的内容。原有资料没有被覆盖。'
          : '暂时无法确认保存结果：' + error.message + '。可以重试同一操作，不会重复保存。';
        note.textContent = operation.error;
      } finally {
        operation.pending = false;
        if (current(context)) busy.delete(busyKey);
        if (current(context)) { confirm.disabled = false; reject.disabled = false; }
      }
    }
    confirm.onclick = () => act('confirm'); reject.onclick = () => act('reject');
    actions.append(confirm, reject, revise); card.append(actions); return card;
  });
}
