/* Personal information is edited in conversation. This drawer only shows saved facts. */
function createCollaborationProfileUI({ api, current, host, status = () => {}, onVisibilityChange = () => {}, onEdit = () => {}, onClose = () => {} }) {
  let generation = 0;
  const element = (tag, text, className) => {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (className) node.className = className;
    return node;
  };
  function reset() { generation++; host().replaceChildren(); onVisibilityChange(false); }
  function close() { reset(); onClose(); }
  async function open(context) {
    if (!current(context)) return;
    const version = ++generation;
    const valid = () => current(context) && generation === version;
    const header = element('header', undefined, 'profile-header');
    const dismiss = element('button', '关闭', 'text-btn'); dismiss.type = 'button';
    dismiss.setAttribute('aria-label', '关闭个人资料'); dismiss.onclick = () => { if (valid()) close(); };
    header.append(element('h2', '助手记住的我'), dismiss);
    const body = element('div', undefined, 'profile-memory');
    const note = element('p', '正在读取已保存的资料…', 'note'); note.setAttribute('role', 'status'); body.append(note);
    host().replaceChildren(header, body); onVisibilityChange(true); dismiss.focus?.();
    try {
      const profile = await api(context, 'collaboration-profile'); if (!valid()) return;
      if (!profile?.public || !profile?.private || !profile?.permissions) throw Error('个人资料响应无效');
      note.textContent = '直接在聊天里告诉我你的工作、偏好或需要修改的内容。这里展示你已确认记住的信息。';
      function group(title, visibility, fields) {
        const section = element('section', undefined, 'collaboration-section');
        section.append(element('h3', title), element('p', visibility, 'note'));
        const details = element('dl', undefined, 'contact-public-profile');
        for (const [label, value] of fields.filter(([, value]) => value)) {
          details.append(element('dt', label), element('dd', value));
        }
        if (!details.children.length) details.append(element('p', title === '向同事公开的资料' ? '还没有公开的协作资料。' : '还没有保存的背景或偏好。', 'note'));
        section.append(details); body.append(section);
      }
      group('仅我和助手知道', '用于帮助自己的助手理解你，不会出现在联系人资料中。', [
        ['工作背景', profile.private.work_context], ['回复偏好', profile.private.response_preferences]
      ]);
      group('向同事公开的资料', '有权联系你的同事可以看到。只有你确认公开的内容才会放在这里。', [
        ['团队', profile.public.team], ['负责的工作', profile.public.responsibilities],
        ['可以找我帮忙', profile.public.help_with], ['发请求时请提供', profile.public.request_requirements]
      ]);
      group('助手的代办范围', '接手任务、承诺时间和正式交付仍由你确认。', [
        ['依据公开资料答复', profile.permissions.auto_reply_public ? '已允许' : '未允许'],
        ['接手后准备私密草稿', profile.permissions.auto_prepare_drafts ? '已允许' : '未允许']
      ]);
      const actions = element('footer', undefined, 'profile-actions');
      const edit = element('button', '通过聊天修改', 'primary'); edit.type = 'button';
      edit.onclick = () => { if (valid()) { close(); onEdit(); } };
      actions.append(edit); body.append(actions);
    } catch (error) {
      if (!valid()) return;
      note.textContent = '暂时无法读取资料：' + error.message;
      const retry = element('button', '重新读取'); retry.type = 'button'; retry.onclick = () => { if (valid()) return open(context); }; body.append(retry);
    }
  }
  return { open, close, reset, hasUnsaved: () => false };
}
