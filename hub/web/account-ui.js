/* Account settings share the right-hand surface with the directory. */
async function openAccount() {
  if (typeof closeProjects === 'function') closeProjects();
  const context = session(); if (!current(context)) return;
  profileUI.reset(); hideCollaborationSettings(); closeContacts();
  const version = contactsGeneration;
  const valid = () => current(context) && contactsGeneration === version;
  const host = $('contacts-view'); host.classList.remove('hide'); $('room').classList.add('contacts-open');
  $('open-account').setAttribute('aria-expanded', 'true');
  const header = el('header', 'profile-header');
  const close = el('button', 'icon-btn profile-close', '×'); close.type = 'button'; close.setAttribute('aria-label', '关闭账号设置');
  close.onclick = () => { closeContacts(); $('open-account').focus?.(); };
  header.append(el('h2', null, '账号与成员'), close);
  const content = el('div', 'contacts-body', '正在读取账号…'); host.append(header, content); close.focus?.();
  try {
    const me = await api(context, 'me'); if (!valid()) return;
    const own = me.member;
    if (!own) throw Error('请更新并重启服务以使用账号设置');
    content.replaceChildren(el('p', 'note', browserMode ? '已通过组织身份登录。' : '当前使用本地开发令牌。'));
    function editMember(member) {
      const card = el('section', 'member-card');
      const title = el('h3', 'member-title');
      const avatar = el('span', 'contact-avatar avatar-' + member.color, (member.display_name || member.id).slice(0, 1).toUpperCase());
      title.append(avatar, el('span', null, member.id + (member.admin ? ' · 管理员' : ''))); card.append(title);
      const name = el('input'); name.value = member.display_name; name.maxLength = 96; name.setAttribute('aria-label', member.id + ' 显示姓名');
      const color = el('select'); color.setAttribute('aria-label', member.id + ' 头像颜色');
      for (const [value,label] of [['green','松绿'],['blue','海蓝'],['purple','紫色'],['orange','橙色']]) {
        const option = el('option', null, label); option.value = value; color.append(option);
      }
      color.value = member.color;
      name.oninput = () => { avatar.textContent = (name.value || member.id).slice(0, 1).toUpperCase(); };
      color.onchange = () => { avatar.className = 'contact-avatar avatar-' + color.value; };
      const nameLabel = el('label', 'member-label', '显示姓名'); nameLabel.append(name);
      const colorLabel = el('label', 'member-label', '头像颜色'); colorLabel.append(color);
      const note = el('p', 'note', { active: '已启用', disabled: '已停用', invited: '等待接受邀请' }[member.status]);
      const save = el('button', 'primary', '保存资料'); save.type = 'button';
      let pending = false;
      async function update(statusValue) {
        if (!valid() || pending) return; pending = true; save.disabled = true;
        try {
          await api(context, 'members/' + encodeURIComponent(member.id), 'PUT', { expected_revision: member.revision, display_name: name.value, color: color.value, status: statusValue });
          if (valid()) { status('成员资料已更新。'); await openAccount(); }
        } catch (error) { if (valid()) note.textContent = error.message + '。请关闭后重新读取再操作。'; }
        finally { pending = false; if (valid()) save.disabled = false; }
      }
      save.onclick = () => update(member.status);
      card.append(nameLabel, colorLabel, note, save);
      if (own.admin && member.id !== own.id && !member.admin) {
        const toggle = el('button', null, member.status === 'active' ? '停用账号' : '启用账号'); toggle.type = 'button';
        toggle.onclick = () => { if (confirm(member.status === 'active' ? '停用后该成员将无法访问。是否继续？' : '允许该已绑定成员访问？')) return update(member.status === 'active' ? 'disabled' : 'active'); };
        card.append(toggle);
        if (browserMode && member.status !== 'active') {
          const invite = el('button', null, '生成邀请链接'); invite.type = 'button';
          invite.onclick = async () => {
            if (!valid() || pending) return; pending = true; invite.disabled = true;
            try {
              const result = await api(context, 'invitations', 'POST', { owner: member.id, expected_revision: member.revision });
              if (!valid()) return;
              const link = el('input'); link.readOnly = true; link.value = window.location.origin + result.path; link.setAttribute('aria-label', '邀请链接');
              card.append(link); note.textContent = '链接 24 小时有效，仅绑定的 GitLab 身份可接受。请复制分享；重新读取后可生成新链接。';
              save.disabled = true; toggle.disabled = true; link.focus?.(); link.select?.();
            } catch (error) { if (valid()) note.textContent = error.message + '。请重新读取成员状态。'; }
          }; card.append(invite);
        }
      }
      return card;
    }
    content.append(editMember(own));
    content.append($('account-tools'));
    if (own.admin) {
      const members = await api(context, 'members'); if (!valid()) return;
      content.append(el('h3', null, '组织成员'), el('p', 'note', '新成员须先绑定 GitLab 唯一身份。邀请不会按同名或邮箱关联已有账号。'));
      for (const member of members) if (member.id !== own.id) content.append(editMember(member));
    }
  } catch (error) { if (valid()) content.replaceChildren(el('p', 'note', '无法读取账号：' + error.message)); }
}
$('open-account').onclick = openAccount;
