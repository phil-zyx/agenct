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
  const imSection = el('div', 'im-section');
  const nodesSection = el('div', 'nodes-section');
  const automationSection = el('div', 'automation-section');
  const rulesSection = el('div', 'rules-section');
  const adminSection = el('div', 'admin-section');
  content.replaceChildren(...[accountSection, $('collaboration-profile-view'), nodesSection, automationSection, rulesSection, imSection, gitlabSection, $('account-tools'), adminSection].filter(Boolean));
  host.append(header, content); close.focus?.();
  if (typeof collaborationProfileUI !== 'undefined') void collaborationProfileUI.open(context);
  void renderNodesSection(context, valid, nodesSection);
  void renderAutomationPermissions(context, valid, automationSection);
  void renderRulesSection(context, valid, rulesSection);
  void renderImWebhookSection(context, valid, imSection);
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
  const intro = el('p', 'note', '接收 GitLab Issue / MR 中提及你的请求。自动执行与评论回写需在「自动化权限」中分别开启。');

  const hookBox = el('div', 'gitlab-webhook-row');
  const hookNote = el('p', 'note', '方式 A（实时触发）· 仓库 Webhook URL（填入 GitLab 项目 Settings → Webhooks，勾选 Comments / Issues / Merge request events；Secret Token 必填：填写你的 PC 节点令牌，仅处理提及你本人的请求；跨成员触发需由管理员配置专用 Webhook 密钥）：');
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

async function renderAutomationPermissions(context, valid, container) {
  const section = el('section', 'member-card');
  section.append(el('h3', 'member-title', '自动化权限'));
  const hint = el('p', 'note', '仅在你明确授权后自动执行。关闭执行权限会取消尚未完成的自动派发；已经启动的本机进程仍需在该电脑停止。');
  const note = el('p', 'note', '正在读取权限…'); note.setAttribute('role', 'status');
  const fields = [
    ['auto_accept', '自动接手他人委派并调度我的节点'],
    ['auto_deliver', '自动将节点草稿交付给委派人（未经我逐次审阅）'],
    ['gitlab_execute', '收到 GitLab 提及后自动在我的节点执行'],
    ['gitlab_reply', '自动将完成结果发布为 GitLab 评论']
  ];
  const inputs = new Map();
  section.append(hint);
  for (const [key, title] of fields) {
    const label = el('label', 'gitlab-check');
    const input = el('input'); input.type = 'checkbox'; input.disabled = true;
    input.setAttribute('aria-label', title); inputs.set(key, input);
    label.append(input, el('span', null, title)); section.append(label);
  }
  const save = el('button', 'primary', '保存自动化权限'); save.type = 'button'; save.disabled = true;
  const reload = el('button', null, '重新读取权限'); reload.type = 'button';
  const actions = el('div', 'actions'); actions.append(save, reload);
  section.append(note, actions); container.replaceChildren(section);
  let revision = null, pending = false;
  function lock(value) { save.disabled = value; reload.disabled = value; for (const input of inputs.values()) input.disabled = value; }
  function apply(value) {
    if (!value || !Number.isSafeInteger(value.revision) || fields.some(([key]) => typeof value[key] !== 'boolean')) throw Error('权限响应无效，请重新读取');
    revision = value.revision;
    for (const [key, input] of inputs) input.checked = value[key];
  }
  async function load() {
    if (!valid() || pending) return;
    pending = true; lock(true); revision = null;
    try {
      const value = await api(context, 'automation/permissions');
      if (!valid()) return;
      apply(value); note.textContent = '勾选只修改本页，点击保存后生效。'; lock(false);
    } catch (error) { if (valid()) { note.textContent = error.message; reload.disabled = false; } }
    finally { pending = false; }
  }
  reload.onclick = load;
  save.onclick = async () => {
    if (!valid() || pending || revision === null) return;
    const body = { revision }; for (const [key, input] of inputs) body[key] = input.checked;
    pending = true; lock(true);
    try {
      const value = await api(context, 'automation/permissions', 'PUT', body);
      if (!valid()) return;
      apply(value); note.textContent = '自动化权限已保存。'; lock(false);
    } catch (error) {
      if (valid()) { revision = null; note.textContent = error.message + '。请重新读取权限，核对后再保存。'; reload.disabled = false; }
    } finally { pending = false; }
  };
  await load();
}

async function renderRulesSection(context, valid, container) {
  const card = el('section', 'member-card rules-card');
  card.append(
    el('h3', 'member-title', '🛡️ 个人管家规则与真人接手策略'),
    el('p', 'note', '内置三层策略管线：① 系统公共护栏（点名「找你本人/真人」、排期与发版承诺、连续退回 ≥ 2 次熔断、节点离线）；② 你的个人定制规则（下方可增删，或在主对话发送「规则：包含 生产数据库 的任务需要真人接手」）；③ 大模型 [NEED_HUMAN] 自检兜底。')
  );
  const list = el('div', 'nodes-list', '正在读取个人规则…');
  const form = el('div', 'node-create-form');
  const presets = el('div', 'profile-guide-chips');
  const keywordInput = el('input'); keywordInput.maxLength = 120; keywordInput.placeholder = '如 生产数据库/合同/报价（多个词用 / 分隔，* 代表全部）'; keywordInput.setAttribute('aria-label', '触发关键词');
  const actionSelect = el('select'); actionSelect.setAttribute('aria-label', '命中动作');
  for (const [val, label] of [
    ['human', '🚨 转交我本人接手（暂停自动执行/回复）'],
    ['clarify', '📋 先向对方追问补充信息'],
    ['route', '💻 自动路由到指定 PC 节点']
  ]) {
    const opt = el('option', null, label); opt.value = val; actionSelect.append(opt);
  }
  const paramInput = el('input'); paramInput.maxLength = 120; paramInput.placeholder = '当前为「转交本人接手」，无需额外参数'; paramInput.setAttribute('aria-label', '动作附加参数');
  actionSelect.onchange = () => {
    if (actionSelect.value === 'human') paramInput.placeholder = '当前为「转交本人接手」，无需额外参数';
    else if (actionSelect.value === 'clarify') paramInput.placeholder = '必填：要求对方提供的信息（如 复现步骤与截图）';
    else paramInput.placeholder = '必填：目标 PC 节点 ID（如 homelab 或 cc-team）';
  };
  for (const [chipLabel, kw, mode, param] of [
    ['🚨 生产数据库/线上配置 → 本人接手', '生产数据库/线上配置', 'human', ''],
    ['🚨 合同/报价/薪资 → 本人接手', '合同/报价/薪资', 'human', ''],
    ['📋 前端/UI问题 → 追问截图', '前端/UI/页面', 'clarify', '截图与复现步骤']
  ]) {
    const chip = el('button', 'guide-chip', chipLabel); chip.type = 'button';
    chip.onclick = () => {
      keywordInput.value = kw;
      actionSelect.value = mode;
      actionSelect.onchange();
      paramInput.value = param;
    };
    presets.append(chip);
  }
  const kwLabel = el('label', 'member-label', '触发关键词（支持 / 分隔多个词）'); kwLabel.append(keywordInput);
  const actLabel = el('label', 'member-label', '命中后动作'); actLabel.append(actionSelect);
  const paramLabel = el('label', 'member-label', '补充说明 / 目标节点 ID（按动作选填）'); paramLabel.append(paramInput);
  const formNote = el('p', 'note');
  const addBtn = el('button', 'primary', '添加个人规则'); addBtn.type = 'button';
  form.append(presets, kwLabel, actLabel, paramLabel, formNote, addBtn);
  card.append(list, form);
  container.replaceChildren(card);

  async function loadRules() {
    try {
      const data = await api(context, 'automation'); if (!valid()) return;
      const rules = Array.isArray(data?.rules) ? data.rules : [];
      if (!rules.length) {
        list.replaceChildren(el('p', 'note', '暂未配置个人定制规则（当前已启用系统公共护栏）。可点上方快捷标签一键添加。'));
        return;
      }
      const rows = rules.map(rule => {
        const row = el('div', 'node-row');
        const head = el('div', 'node-row-head');
        const summary = [];
        if (rule.require_human_approval) summary.push('🚨 转交本人接手');
        if (rule.require_clarification) summary.push('📋 需补充：' + rule.require_clarification);
        if (rule.target_node_id) summary.push('💻 路由节点：' + rule.target_node_id);
        head.append(
          el('strong', null, '关键词：「' + rule.keyword + '」'),
          el('span', 'node-badge online', summary.join(' · ') || '默认规则')
        );
        row.append(head);
        const actions = el('div', 'actions');
        const remove = el('button', 'danger', '删除规则'); remove.type = 'button';
        remove.onclick = async () => {
          if (!valid() || remove.disabled) return;
          remove.disabled = true;
          try {
            await api(context, 'automation/rules/' + encodeURIComponent(rule.id) + '/remove', 'POST');
            if (!valid()) return;
            await loadRules();
          } catch (error) { if (valid()) formNote.textContent = error.message; }
          finally { if (valid()) remove.disabled = false; }
        };
        actions.append(remove);
        row.append(actions);
        return row;
      });
      list.replaceChildren(...rows);
    } catch (error) {
      if (valid()) list.replaceChildren(el('p', 'note', '无法读取个人规则：' + error.message));
    }
  }

  addBtn.onclick = async () => {
    if (!valid() || addBtn.disabled) return;
    const keyword = keywordInput.value.trim();
    const mode = actionSelect.value;
    const param = paramInput.value.trim();
    if (!keyword) { formNote.textContent = '请填写触发关键词。'; return; }
    if ((mode === 'clarify' || mode === 'route') && !param) {
      formNote.textContent = mode === 'clarify' ? '请填写需要对方补充的信息。' : '请填写目标 PC 节点 ID。';
      return;
    }
    addBtn.disabled = true; formNote.textContent = '';
    try {
      const payload = {
        keyword,
        require_human_approval: mode === 'human',
        require_clarification: mode === 'clarify' ? param : null,
        target_node_id: mode === 'route' ? param : ''
      };
      await api(context, 'automation/rules', 'POST', payload);
      if (!valid()) return;
      keywordInput.value = ''; paramInput.value = '';
      formNote.textContent = '✅ 已添加个人规则，实时生效。';
      await loadRules();
    } catch (error) {
      if (valid()) formNote.textContent = error.message;
    } finally {
      if (valid()) addBtn.disabled = false;
    }
  };

  await loadRules();
}

async function renderImWebhookSection(context, valid, container) {
  const card = el('article', 'member-card im-card');
  const head = el('div', 'node-row-head');
  const badge = el('span', 'node-badge offline', '读取中…');
  head.append(el('h3', null, '🔔 飞书 / 企微 / 钉钉 实时通知卡片'), badge);
  const intro = el('p', 'note', '当有【🚨 需本人接手】、【🤝 新任务委派】或【✅ 节点完成待验收】时，管家自动向你的群机器人或个人通知通道推送交互卡片，无需时刻盯着网页。');

  const form = el('div', 'node-create-form');
  const providerSelect = el('select'); providerSelect.setAttribute('aria-label', '通知平台');
  for (const [val, label] of [
    ['feishu', '🪶 飞书自定义机器人（Interactive 彩色卡片）'],
    ['wecom', '💬 企业微信机器人（Markdown 卡片）'],
    ['dingtalk', '📌 钉钉自定义机器人（Markdown 卡片）'],
    ['generic', '🔗 通用 Webhook（标准 JSON）']
  ]) {
    const opt = el('option', null, label); opt.value = val; providerSelect.append(opt);
  }
  const urlInput = el('input'); urlInput.type = 'url'; urlInput.maxLength = 512;
  urlInput.placeholder = 'https://open.feishu.cn/open-apis/bot/v2/hook/...';
  urlInput.setAttribute('aria-label', 'IM Webhook 地址');

  const takeoverCheck = el('input'); takeoverCheck.type = 'checkbox'; takeoverCheck.checked = true; takeoverCheck.setAttribute('aria-label', '推送需本人接手提醒');
  const delegationCheck = el('input'); delegationCheck.type = 'checkbox'; delegationCheck.checked = true; delegationCheck.setAttribute('aria-label', '推送新委派提醒');
  const deliveryCheck = el('input'); deliveryCheck.type = 'checkbox'; deliveryCheck.checked = true; deliveryCheck.setAttribute('aria-label', '推送待验收提醒');

  const mkCheck = (input, text) => { const l = el('label', 'gitlab-check'); l.append(input, el('span', null, text)); return l; };
  const providerLabel = el('label', 'member-label', '通知平台'); providerLabel.append(providerSelect);
  const urlLabel = el('label', 'member-label', '机器人 Webhook 地址'); urlLabel.append(urlInput);
  const formNote = el('p', 'note');
  const saveBtn = el('button', 'primary', '保存通知配置'); saveBtn.type = 'button';
  const testBtn = el('button', null, '发送测试卡片'); testBtn.type = 'button';
  const clearBtn = el('button', 'danger hide', '清除地址'); clearBtn.type = 'button';
  const btnRow = el('div', 'actions'); btnRow.append(saveBtn, testBtn, clearBtn);

  form.append(
    providerLabel,
    urlLabel,
    mkCheck(takeoverCheck, '🚨 当触发「需本人接手」护栏或连续退回熔断时推送'),
    mkCheck(delegationCheck, '🤝 当收到新委派或需补充材料时推送'),
    mkCheck(deliveryCheck, '✅ 当节点完成任务交付、等待验收时推送'),
    formNote,
    btnRow
  );
  card.append(head, intro, form);
  container.replaceChildren(card);

  function applyImConfig(cfg) {
    if (!cfg || typeof cfg !== 'object') return;
    if (cfg.provider) providerSelect.value = cfg.provider;
    takeoverCheck.checked = cfg.notify_takeover !== false;
    delegationCheck.checked = cfg.notify_delegation !== false;
    deliveryCheck.checked = cfg.notify_delivery !== false;
    urlInput.value = '';
    if (cfg.configured) {
      urlInput.placeholder = '已配置（' + (cfg.webhook_url_preview || '***') + '），留空保持不变';
      clearBtn.classList.remove('hide');
      badge.className = 'node-badge online';
      badge.textContent = '已启用实时卡片';
    } else {
      urlInput.placeholder = 'https://open.feishu.cn/open-apis/bot/v2/hook/...';
      clearBtn.classList.add('hide');
      badge.className = 'node-badge offline';
      badge.textContent = '未配置';
    }
  }

  async function submitImConfig(clearWebhook = false) {
    if (!valid() || saveBtn.disabled) return;
    saveBtn.disabled = true; clearBtn.disabled = true; formNote.textContent = '';
    try {
      const payload = {
        provider: providerSelect.value,
        notify_takeover: !!takeoverCheck.checked,
        notify_delegation: !!delegationCheck.checked,
        notify_delivery: !!deliveryCheck.checked,
      };
      if (clearWebhook) payload.clear_webhook = true;
      else if (urlInput.value.trim()) payload.webhook_url = urlInput.value.trim();
      const res = await api(context, 'im/config', 'POST', payload);
      if (!valid()) return;
      applyImConfig(res);
      formNote.textContent = clearWebhook ? '已清除通知 Webhook。' : '✅ 实时通知卡片配置已保存。';
    } catch (error) {
      if (valid()) formNote.textContent = error.message;
    } finally {
      if (valid()) { saveBtn.disabled = false; clearBtn.disabled = false; }
    }
  }

  saveBtn.onclick = () => submitImConfig(false);
  clearBtn.onclick = () => submitImConfig(true);
  testBtn.onclick = async () => {
    if (!valid() || testBtn.disabled) return;
    testBtn.disabled = true; formNote.textContent = '正在发送测试卡片…';
    try {
      await api(context, 'im/test', 'POST');
      if (valid()) formNote.textContent = '🎉 测试卡片已成功推送到目标群/应用！';
    } catch (error) {
      if (valid()) formNote.textContent = error.message;
    } finally {
      if (valid()) testBtn.disabled = false;
    }
  };

  try {
    const cfg = await api(context, 'im/config');
    if (valid()) applyImConfig(cfg);
  } catch (error) {
    if (valid()) {
      badge.textContent = '未连接';
      formNote.textContent = '读取通知配置失败：' + error.message;
    }
  }
}
