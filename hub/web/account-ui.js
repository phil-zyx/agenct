/* Account settings share the right-hand surface with the directory. */
async function openAccount() {
  if (typeof setRailMenuOpen === 'function') setRailMenuOpen(false);
  if (typeof closeComposerOverlays === 'function') closeComposerOverlays();
  if (typeof closeProjects === 'function') closeProjects();
  const context = session(); if (!current(context)) return;
  if (typeof closePeerConversation === 'function') closePeerConversation();
  profileUI.reset(); hideCollaborationSettings(); closeContacts();
  accountOpen = true;
  const version = contactsGeneration;
  const valid = () => current(context) && contactsGeneration === version;
  const host = $('contacts-view'); host.classList.remove('hide'); $('room').classList.add('contacts-open');
  if (typeof syncRailState === 'function') syncRailState();
  const header = el('header', 'profile-header');
  const close = el('button', 'icon-btn profile-close', '×'); close.type = 'button'; close.setAttribute('aria-label', '关闭账号设置');
  close.onclick = () => { closeContacts(); ($('rail-more') || $('open-account')).focus?.(); };
  const tabs = el('div', 'seg');
  tabs.setAttribute('role', 'tablist');
  tabs.setAttribute('aria-label', '账号与设置');
  const accountTab = el('button', 'on', '个人与账号');
  accountTab.type = 'button'; accountTab.setAttribute('role', 'tab'); accountTab.setAttribute('aria-selected', 'true');
  const agentsTab = $('open-agents');
  if (agentsTab) {
    agentsTab.className = ''; agentsTab.textContent = '助手分身';
    agentsTab.setAttribute('role', 'tab'); agentsTab.setAttribute('aria-selected', 'false');
    tabs.append(accountTab, agentsTab);
  } else {
    tabs.append(accountTab);
  }
  header.append(tabs, close);
  const content = el('div', 'contacts-body');
  const accountSection = el('div', 'account-section', '正在读取账号…');
  const gitlabSection = el('div', 'gitlab-section');
  const nodesSection = el('div', 'nodes-section');
  const adminSection = el('div', 'admin-section');
  content.replaceChildren(...[accountSection, $('collaboration-profile-view'), nodesSection, gitlabSection, $('account-tools'), adminSection].filter(Boolean));
  host.append(header, content); close.focus?.();
  if (typeof collaborationProfileUI !== 'undefined') void collaborationProfileUI.open(context);
  void renderNodesSection(context, valid, nodesSection);
  void renderGitlabSection(context, valid, gitlabSection);
  try {
    const me = await api(context, 'me'); if (!valid()) return;
    const own = me.member;
    if (!own) throw Error('请更新并重启服务以使用账号设置');
    rememberPerson(own.id, own.display_name);
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
          if (valid()) {
            if (rememberPerson(member.id, name.value)) void refresh(context, false);
            status('成员资料已更新。', { transient: true }); await openAccount();
          }
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
    const accountNodes = [el('p', 'note', browserMode ? '已通过组织身份登录。' : '当前使用本地开发令牌。'), editMember(own)];
    if (browserMode && openSignup) accountNodes.push(inviteColleagues());
    accountSection.replaceChildren(...accountNodes);
    if (own.admin) {
      const members = await api(context, 'members'); if (!valid()) return;
      for (const member of members) rememberPerson(member.id, member.display_name);
      adminSection.replaceChildren(
        el('h3', null, '组织成员'),
        el('p', 'note', '新成员须先绑定 GitLab 唯一身份。邀请不会按同名或邮箱关联已有账号。'),
        ...members.filter(member => member.id !== own.id).map(editMember)
      );
    }
  } catch (error) { if (valid()) accountSection.replaceChildren(el('p', 'note', '无法读取账号：' + error.message)); }
}
/* Open GitLab signup: an invitation is just the entry link plus how to join. */
function inviteColleagues() {
  const box = el('section', 'member-card invite-colleagues');
  box.append(el('h3', null, '邀请同事'), el('p', 'note', '公司 GitLab 的在职账号打开链接并登录即可加入。分享前确认对方可以访问内网。'));
  const message = '我在用 Agenct 和同事协作，打开 ' + window.location.origin + '/ 用公司 GitLab 账号登录即可加入。';
  const text = el('textarea'); text.readOnly = true; text.rows = 3; text.value = message; text.setAttribute('aria-label', '邀请文字');
  const copy = el('button', 'primary', '复制邀请'); copy.type = 'button';
  copy.onclick = async () => {
    let done = false;
    try { if (navigator.clipboard && window.isSecureContext) { await navigator.clipboard.writeText(message); done = true; } } catch { /* fall back */ }
    if (!done) { text.focus?.(); text.select?.(); try { done = document.execCommand('copy'); } catch { done = false; } }
    copy.textContent = done ? '已复制' : '请手动复制上方文字';
  };
  box.append(text, copy);
  return box;
}
async function copyTextWithFallback(button, field, value, doneLabel = '已复制') {
  let done = false;
  try { if (navigator.clipboard && window.isSecureContext) { await navigator.clipboard.writeText(value); done = true; } } catch { /* fall back */ }
  if (!done) { field.focus?.(); field.select?.(); try { done = document.execCommand('copy'); } catch { done = false; } }
  button.textContent = done ? doneLabel : '请手动复制上方内容';
}
async function renderNodesSection(context, valid, container) {
  const card = el('section', 'member-card nodes-card');
  card.append(
    el('h3', null, '我的 PC 节点（一键接入多端算力与本地军团）'),
    el('p', 'note', '添加节点后直接复制一条命令到对应电脑终端粘贴运行，无需手动下载仓库；在任务卡片上可将工作指派给指定 PC。')
  );
  const list = el('div', 'nodes-list', '正在读取 PC 节点…');
  const tokenBox = el('div', 'node-token-box hide');
  const form = el('div', 'node-create-form');
  const presets = el('div', 'profile-guide-chips');
  const idInput = el('input'); idInput.maxLength = 32; idInput.placeholder = '留空自动生成（如 pc-1）'; idInput.setAttribute('aria-label', '节点标识');
  const nameInput = el('input'); nameInput.maxLength = 64; nameInput.placeholder = '如 办公主力机（点上方标签可快速填入）'; nameInput.setAttribute('aria-label', '节点名称');
  const capInput = el('input'); capInput.maxLength = 200; capInput.placeholder = '留空将在 PC 连上后自动识别本机环境与 Agent'; capInput.setAttribute('aria-label', '环境说明');
  for (const [label, presetName, presetHint] of [
    ['💻 办公主力机', '办公主力机', 'office-pc'],
    ['🔥 本地算力机', '本地算力机', 'gpu-box'],
    ['🏠 家里电脑', '家里电脑', 'home-pc'],
  ]) {
    const chip = el('button', 'guide-chip', label); chip.type = 'button';
    chip.onclick = () => {
      nameInput.value = presetName;
      if (!idInput.value.trim()) idInput.placeholder = '留空自动生成（如 ' + presetHint + '）';
      nameInput.focus?.();
    };
    presets.append(chip);
  }
  const nameLabel = el('label', 'member-label', '节点名称'); nameLabel.append(nameInput);
  const idLabel = el('label', 'member-label', '节点标识（可选，字母/数字/-/_）'); idLabel.append(idInput);
  const capLabel = el('label', 'member-label', '环境说明（可选）'); capLabel.append(capInput);
  const formNote = el('p', 'note');
  const addBtn = el('button', 'primary', '添加 PC 节点'); addBtn.type = 'button';
  form.append(presets, nameLabel, idLabel, capLabel, formNote, addBtn);
  card.append(list, tokenBox, form);
  container.replaceChildren(card);

  function showToken(nodeId, token) {
    const origin = window.location.origin;
    const unixCmd = 'curl -fsSL ' + origin + '/node/install.sh | sh -s -- ' + token;
    const winCmd = '$env:AGENCT_NODE_TOKEN="' + token + '"; irm ' + origin + '/node/install.ps1 | iex';
    const note = el('p', 'note', '节点 ' + nodeId + ' 一键接入命令（令牌只显示这一次，复制到对应 PC 终端粘贴运行即可；会自动识别本机 Claude / Codex / Gemini / Ollama 或使用内置模板）：');
    const unixLabel = el('label', 'member-label', 'Linux / macOS / WSL 一键命令');
    const unixArea = el('textarea'); unixArea.readOnly = true; unixArea.rows = 2; unixArea.value = unixCmd; unixArea.setAttribute('aria-label', 'Linux/macOS 节点启动命令');
    unixLabel.append(unixArea);
    const winLabel = el('label', 'member-label', 'Windows PowerShell 一键命令');
    const winArea = el('textarea'); winArea.readOnly = true; winArea.rows = 2; winArea.value = winCmd; winArea.setAttribute('aria-label', 'Windows 节点启动命令');
    winLabel.append(winArea);
    const unixCopy = el('button', 'primary', '复制一键启动命令'); unixCopy.type = 'button';
    unixCopy.onclick = () => copyTextWithFallback(unixCopy, unixArea, unixCmd);
    const winCopy = el('button', null, '复制 Windows 命令'); winCopy.type = 'button';
    winCopy.onclick = () => copyTextWithFallback(winCopy, winArea, winCmd);
    const actions = el('div', 'actions');
    actions.append(unixCopy, winCopy);
    tokenBox.classList.remove('hide');
    tokenBox.replaceChildren(note, unixLabel, winLabel, actions);
    unixArea.focus?.(); unixArea.select?.();
  }

  async function loadList() {
    try {
      const nodes = await api(context, 'nodes'); if (!valid()) return;
      if (!Array.isArray(nodes)) { list.replaceChildren(el('p', 'note', '节点列表格式无效')); return; }
      if (typeof userNodes !== 'undefined') userNodes = nodes;
      if (!nodes.length) {
        list.replaceChildren(el('p', 'note', '还没有注册 PC 节点。点下方预设标签添加一台，生成一键接入命令。'));
        return;
      }
      const rows = nodes.map(node => {
        const row = el('div', 'node-row');
        const head = el('div', 'node-row-head');
        const badge = el('span', 'node-badge ' + (node.online ? 'online' : 'offline'), node.online ? '在线' : '离线');
        head.append(el('strong', null, node.name + '（' + node.id + '）'), badge);
        row.append(head);
        if (node.capabilities) row.append(el('p', 'note', node.capabilities));
        const actions = el('div', 'actions');
        const rotate = el('button', null, '重置令牌'); rotate.type = 'button';
        rotate.onclick = async () => {
          if (!valid() || rotate.disabled) return;
          rotate.disabled = true;
          try {
            const res = await api(context, 'nodes/' + encodeURIComponent(node.id) + '/token', 'POST');
            if (!valid()) return;
            showToken(node.id, res.token);
            status('已生成新的节点令牌。', { transient: true });
          } catch (error) { if (valid()) formNote.textContent = error.message; }
          finally { if (valid()) rotate.disabled = false; }
        };
        const remove = el('button', 'danger', '移除'); remove.type = 'button';
        remove.onclick = async () => {
          if (!valid() || remove.disabled) return;
          if (!confirm('移除节点「' + node.name + '」后，该机器令牌立即失效。确认继续？')) return;
          remove.disabled = true;
          try {
            await api(context, 'nodes/' + encodeURIComponent(node.id) + '/remove', 'POST');
            if (!valid()) return;
            tokenBox.classList.add('hide'); tokenBox.replaceChildren();
            await loadList();
            void refresh(context, false);
          } catch (error) { if (valid()) formNote.textContent = error.message; }
          finally { if (valid()) remove.disabled = false; }
        };
        actions.append(rotate, remove);
        row.append(actions);
        return row;
      });
      list.replaceChildren(...rows);
    } catch (error) {
      if (valid()) list.replaceChildren(el('p', 'note', '无法读取 PC 节点：' + error.message));
    }
  }

  addBtn.onclick = async () => {
    if (!valid() || addBtn.disabled) return;
    const id = idInput.value.trim(), name = nameInput.value.trim(), capabilities = capInput.value.trim();
    if (!name || (id && !/^[A-Za-z0-9_-]{1,32}$/.test(id))) {
      formNote.textContent = '请填写节点名称；节点标识如填写需为 1–32 位字母/数字/-/_。';
      return;
    }
    addBtn.disabled = true; formNote.textContent = '';
    try {
      const res = await api(context, 'nodes', 'POST', { id, name, capabilities });
      if (!valid()) return;
      idInput.value = ''; nameInput.value = ''; capInput.value = '';
      showToken(res.node.id, res.token);
      await loadList();
      void refresh(context, false);
    } catch (error) {
      if (valid()) formNote.textContent = error.message;
    } finally {
      if (valid()) addBtn.disabled = false;
    }
  };

  await loadList();
}

async function renderGitlabSection(context, valid, container) {
  const card = el('article', 'member-card gitlab-card');
  const head = el('div', 'node-row-head');
  const badge = el('span', 'node-badge offline', '读取中…');
  head.append(el('h3', null, '🦊 GitLab 双向协同'), badge);
  const intro = el('p', 'note', '在任意 GitLab Issue / MR 中 @你的名字，管家会自动接管并调度你的 PC 节点处理，完成后自动将回执写回原评论区。');

  const hookBox = el('div', 'gitlab-webhook-row');
  const hookNote = el('p', 'note', '方式 A（实时触发）· 仓库 Webhook URL（填入 GitLab 项目 Settings → Webhooks，勾选 Comments / Issues / Merge request events；Secret Token 可留空或填你的任意 PC 节点令牌）：');
  const hookArea = el('textarea'); hookArea.readOnly = true; hookArea.rows = 2; hookArea.setAttribute('aria-label', 'GitLab Webhook URL');
  const hookActions = el('div', 'actions');
  const copyHookBtn = el('button', null, '复制 Webhook 地址'); copyHookBtn.type = 'button';
  copyHookBtn.onclick = () => copyTextWithFallback(copyHookBtn, hookArea, hookArea.value);
  hookActions.append(copyHookBtn);
  hookBox.append(hookNote, hookArea, hookActions);

  const form = el('div', 'node-create-form');
  const patHint = el('p', 'note', '方式 B（免仓库管理员权限 + 自动回写评论）· 填写你的 GitLab Personal Access Token（需勾选 api 权限）：');
  const baseUrlInput = el('input'); baseUrlInput.type = 'url'; baseUrlInput.maxLength = 256; baseUrlInput.placeholder = 'https://git.example.com'; baseUrlInput.setAttribute('aria-label', 'GitLab 地址');
  const userInput = el('input'); userInput.maxLength = 64; userInput.placeholder = '如 zhouyaoxu（用于精准匹配 @你的名字）'; userInput.setAttribute('aria-label', 'GitLab 用户名');
  const tokenInput = el('input'); tokenInput.type = 'password'; tokenInput.maxLength = 256; tokenInput.placeholder = '填写 GitLab Personal Access Token（glpat-...）'; tokenInput.setAttribute('aria-label', 'GitLab Token');
  const pollInput = el('input'); pollInput.type = 'checkbox'; pollInput.checked = true; pollInput.setAttribute('aria-label', '后台自动拉取 GitLab 待办');
  const pollLabel = el('label', 'gitlab-check');
  pollLabel.append(pollInput, el('span', null, '7×24h 后台每 30 秒自动拉取 @我的 GitLab 待办'));

  const baseLabel = el('label', 'member-label', 'GitLab 地址'); baseLabel.append(baseUrlInput);
  const userLabel = el('label', 'member-label', 'GitLab 用户名'); userLabel.append(userInput);
  const tokenLabel = el('label', 'member-label', 'Personal Access Token'); tokenLabel.append(tokenInput);
  const formNote = el('p', 'note');
  const saveBtn = el('button', 'primary', '保存 GitLab 配置'); saveBtn.type = 'button';
  const syncBtn = el('button', null, '立即拉取 GitLab 待办'); syncBtn.type = 'button';
  const clearBtn = el('button', 'danger hide', '清除 Token'); clearBtn.type = 'button';
  const btnRow = el('div', 'actions');
  btnRow.append(saveBtn, syncBtn, clearBtn);

  form.append(patHint, baseLabel, userLabel, tokenLabel, pollLabel, formNote, btnRow);
  card.append(head, intro, hookBox, form);
  container.replaceChildren(card);

  function applyConfig(cfg) {
    if (!cfg || typeof cfg !== 'object') return;
    hookArea.value = cfg.webhook_url || (window.location.origin + '/webhooks/gitlab');
    if (cfg.base_url) baseUrlInput.value = cfg.base_url;
    if (cfg.gitlab_username !== undefined) userInput.value = cfg.gitlab_username;
    pollInput.checked = cfg.auto_poll_todos !== false;
    tokenInput.value = '';
    if (cfg.has_token) {
      tokenInput.placeholder = '已保存（' + (cfg.token_preview || '***') + '），留空保持不变';
      clearBtn.classList.remove('hide');
      badge.className = 'node-badge online';
      badge.textContent = pollInput.checked ? '已连通 · 自动轮询中' : '已连接 · 仅回写';
    } else {
      tokenInput.placeholder = '填写 GitLab Personal Access Token（glpat-...）';
      clearBtn.classList.add('hide');
      badge.className = 'node-badge offline';
      badge.textContent = '仅 Webhook';
    }
  }

  async function submitConfig(clearToken = false) {
    if (!valid() || saveBtn.disabled) return;
    const base_url = baseUrlInput.value.trim();
    const gitlab_username = userInput.value.trim();
    if (!base_url) { formNote.textContent = '请填写 GitLab 地址。'; return; }
    saveBtn.disabled = true; clearBtn.disabled = true; formNote.textContent = '';
    try {
      const payload = {
        base_url,
        gitlab_username,
        auto_poll_todos: !!pollInput.checked,
      };
      if (clearToken) {
        payload.clear_token = true;
      } else if (tokenInput.value.trim()) {
        payload.token = tokenInput.value.trim();
      }
      const res = await api(context, 'gitlab/config', 'POST', payload);
      if (!valid()) return;
      applyConfig(res);
      formNote.textContent = clearToken ? '已清除 GitLab Token。' : '✅ GitLab 配置已保存，7×24h 管家已激活。';
    } catch (error) {
      if (valid()) formNote.textContent = error.message;
    } finally {
      if (valid()) { saveBtn.disabled = false; clearBtn.disabled = false; }
    }
  }

  saveBtn.onclick = () => submitConfig(false);
  clearBtn.onclick = () => submitConfig(true);
  syncBtn.onclick = async () => {
    if (!valid() || syncBtn.disabled) return;
    syncBtn.disabled = true; formNote.textContent = '正在连接 GitLab 拉取待办…';
    try {
      const res = await api(context, 'gitlab/sync', 'POST');
      if (!valid()) return;
      formNote.textContent = '✅ 已扫描 ' + (res.fetched || 0) + ' 条待办，新派发 ' + (res.dispatched || 0) + ' 条任务给管家/PC 节点。';
      void refresh(context, false);
    } catch (error) {
      if (valid()) formNote.textContent = error.message;
    } finally {
      if (valid()) syncBtn.disabled = false;
    }
  };

  try {
    const cfg = await api(context, 'gitlab/config');
    if (valid()) applyConfig(cfg);
  } catch (error) {
    if (valid()) {
      badge.textContent = '读取失败';
      formNote.textContent = '无法读取 GitLab 配置：' + error.message;
    }
  }
}
$('open-account').onclick = openAccount;
