/* Personal information is edited in conversation. This drawer only shows saved facts. */
function createCollaborationProfileUI({ api, current, host, status = () => {}, embedded = false, onVisibilityChange = () => {}, onEdit = () => {}, onClose = () => {} }) {
  let generation = 0;
  const element = (tag, text, className) => {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (className) node.className = className;
    return node;
  };
  function reset() {
    generation++;
    host().replaceChildren();
    if (!embedded) onVisibilityChange(false);
  }
  function close() { reset(); onClose(); }
  async function open(context) {
    if (!current(context)) return;
    const version = ++generation;
    const valid = () => current(context) && generation === version;
    const body = element('div', undefined, 'profile-memory');
    const note = element('p', '正在读取已保存的资料…', 'note'); note.setAttribute('role', 'status');
    if (embedded) {
      body.append(element('h3', '协作资料与助手记忆'), note);
      host().replaceChildren(body);
    } else {
      const header = element('header', undefined, 'profile-header');
      const dismiss = element('button', '关闭', 'text-btn'); dismiss.type = 'button';
      dismiss.setAttribute('aria-label', '关闭个人资料'); dismiss.onclick = () => { if (valid()) close(); };
      header.append(element('h2', '助手记住的我'), dismiss);
      body.append(note);
      host().replaceChildren(header, body); onVisibilityChange(true); dismiss.focus?.();
    }
    try {
      const profile = await api(context, 'collaboration-profile'); if (!valid()) return;
      if (!profile?.public || !profile?.private || !profile?.permissions) throw Error('个人资料响应无效');
      note.textContent = '直接在聊天里告诉我你的工作、偏好或需要修改的内容。这里展示你已确认记住的信息。';
      function group(title, visibility, fields, chips = []) {
        const section = element('section', undefined, 'collaboration-section');
        section.append(element('h3', title), element('p', visibility, 'note'));
        if (fields.every(([, value]) => !value)) {
          section.append(element('p', title === '向同事公开的资料' ? '还没有公开的协作资料。' : '还没有保存的背景或偏好。', 'note'));
        }
        const details = element('dl', undefined, 'contact-public-profile');
        for (const [label, value, placeholder] of fields) {
          details.append(element('dt', label), value ? element('dd', value) : element('dd', placeholder || '', 'empty-hint'));
        }
        section.append(details);
        if (chips.length) {
          const guide = element('div', undefined, 'profile-guide-chips');
          for (const [chipLabel, promptText] of chips) {
            const chip = element('button', chipLabel, 'guide-chip'); chip.type = 'button';
            chip.onclick = () => { if (valid()) { close(); onEdit(promptText); } };
            guide.append(chip);
          }
          section.append(guide);
        }
        body.append(section);
      }
      group('仅我和助手知道', '用于帮助自己的助手理解你，不会出现在联系人资料中。', [
        ['工作背景', profile.private.work_context, '未设置 · 例：在平台组负责后端接口与系统稳定性'],
        ['回复偏好', profile.private.response_preferences, '未设置 · 例：先给结论，表达简洁，列出关键风险']
      ], [
        ['＋ 介绍工作背景', '记住：我在 [所属团队] 负责 [核心工作与方向]，日常重点关注 [关键目标]。'],
        ['＋ 偏好：先给结论', '记住：回复我时请先给结论，表达简洁直接，涉及方案时列出关键风险。'],
        ['＋ 偏好：结构化要点', '记住：回复我时请用结构化要点整理，区分事实与推测。']
      ]);
      group('向同事公开的资料', '有权联系你的同事可以看到。只有你确认公开的内容才会放在这里。', [
        ['团队', profile.public.team, '未设置 · 例：研发部 / 基础设施组'],
        ['负责的工作', profile.public.responsibilities, '未设置 · 例：核心接口设计与线上排障'],
        ['可以找我帮忙', profile.public.help_with, '未设置 · 例：接口联调、架构评审、性能排查'],
        ['发请求时请提供', profile.public.request_requirements, '未设置 · 例：背景说明、相关文档与期望时间']
      ], [
        ['＋ 一句话生成公开名片', '把我的公开资料设为：团队是 [所属团队]，负责 [主要职责]，同事可以找我帮忙 [可协助事项]，发起请求时请提供 [所需材料与期望时间]。']
      ]);
      group('助手的代办范围', '接手任务、承诺时间和正式交付仍由你确认。', [
        ['依据公开资料答复', profile.permissions.auto_reply_public ? '已允许' : '未允许'],
        ['接手后准备私密草稿', profile.permissions.auto_prepare_drafts ? '已允许' : '未允许']
      ], [
        [profile.permissions.auto_reply_public ? '关闭公开资料答复' : '＋ 允许用公开资料答复', profile.permissions.auto_reply_public ? '关闭公开资料答复' : '允许助手用公开资料答复'],
        [profile.permissions.auto_prepare_drafts ? '关闭自动准备草稿' : '＋ 开启自动准备草稿', profile.permissions.auto_prepare_drafts ? '关闭自动起草' : '开启自动起草']
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
