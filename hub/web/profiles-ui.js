/* Personal role controls in a separate settings view, never a model/tool grant. */
function createProfileUI({ api, current, article, host, status, onVisibility = () => {}, onSaved = () => {} }) {
  let generation = 0, card = null, draft = null;
  const hasUnsaved = () => !!draft && JSON.stringify(draft.agents) !== draft.original;
  const notice = '聊天默认由管家回复。点击输入框旁的「+」可为下一条消息选择其他角色，各角色使用同一段聊天记录。每次只使用一个模型，不启动 Agent 团队，也不会增加工具权限。离线演示不调用模型，请勿在这里填写密钥。';
  function reset({ discard = false } = {}) { if (discard) draft = null; generation++; card = null; host().replaceChildren(); onVisibility(false); }
  function close() { reset(); document.getElementById?.('open-agents')?.focus?.(); }
  function discard() { draft = null; close(); }
  async function open(context) {
    if (draft && (draft.user !== context.user || draft.epoch !== context.epoch)) draft = null;
    const version = ++generation;
    if (card) card.replaceChildren(article('已关闭旧配置卡，正在重新读取。'));
    card = null;
    const valid = () => current(context) && generation === version;
    const panel = article('正在读取个人角色设置…');
    panel.className = 'profile-settings';
    panel.setAttribute?.('aria-label', '个人角色设置');
    card = panel;
    const back = document.createElement('button');
    back.type = 'button'; back.textContent = '×'; back.className = 'icon-btn profile-close';
    back.setAttribute?.('aria-label', '关闭设置'); back.title = '关闭设置'; back.onclick = close;
    const header = document.createElement('header'); header.className = 'profile-header';
    const heading = document.createElement('h2'); heading.textContent = '个人角色设置';
    header.append(heading); header.append(back);
    host().replaceChildren(header, panel);
    onVisibility(true);
    back.focus?.();
    let profile, catalog;
    try {
      [profile, catalog] = await Promise.all([api(context, 'profile'), api(context, 'profile/catalog')]);
    } catch (error) {
      if (!valid()) return;
      panel.replaceChildren(article('无法读取个人配置：' + error.message + '。未发送保存请求，可重新读取。'));
      const retry = document.createElement('button');
      retry.type = 'button'; retry.textContent = '重新读取配置';
      retry.onclick = async () => { if (valid() && !retry.disabled) { retry.disabled = true; await open(context); } };
      panel.append(retry);
      return;
    }
    if (!valid()) return;
    panel.textContent = '';
    const introduction = document.createElement('p'); introduction.className = 'profile-intro'; introduction.textContent = '设置你的默认管家，也可添加其他角色。';
    const details = document.createElement('details'); details.className = 'profile-help';
    const summary = document.createElement('summary'); summary.textContent = '角色如何使用';
    const explanation = document.createElement('p'); explanation.textContent = notice;
    details.append(summary); details.append(explanation);
    panel.append(introduction); panel.append(details);
    if (draft && JSON.stringify(profile.agents) === JSON.stringify(draft.agents)) draft = null;
    if (!hasUnsaved()) draft = { user: context.user, epoch: context.epoch, revision: profile.revision, original: JSON.stringify(profile.agents), agents: profile.agents.map(agent => ({ ...agent })) };
    const editing = draft;
    let agents = editing.agents;
    if (hasUnsaved()) introduction.textContent = profile.revision !== editing.revision
      ? '服务端配置已有更新。你的未保存更改仍保留；请先核对，或取消更改后读取最新配置。'
      : '已恢复你刚才的修改。检查后保存，或选择取消。';
    let saving = false;
    const controls = document.createElement('div'); controls.className = 'profile-controls';
    panel.append(controls);
    function button(label, action, disabled = false) {
      const b = document.createElement('button');
      b.type = 'button'; b.textContent = label; b.disabled = disabled;
      if (['检查更改', '确认保存个人配置'].includes(label)) b.className = 'primary';
      b.onclick = async () => { if (valid() && !saving && !b.disabled) await action(); };
      return b;
    }
    function field(label, input) {
      const wrapper = document.createElement('label');
      wrapper.textContent = label + ' '; wrapper.append(input); return wrapper;
    }
    function select(values, value, change) {
      const input = document.createElement('select');
      for (const item of values) {
        const option = document.createElement('option'); option.value = item; option.textContent = ({ coordinator: '管家', researcher: '研究', writer: '写作', reviewer: '审阅', 'server-default': '默认模型' })[item] || item;
        input.append(option);
      }
      input.value = value; input.onchange = () => { if (valid() && !saving) change(input.value); };
      return input;
    }
    function edit() {
      controls.replaceChildren();
      agents.forEach((agent, index) => {
        const row = document.createElement('section'); row.className = 'profile-role';
        const title = document.createElement('h3'); title.textContent = agent.name; row.append(title);
        const description = document.createElement('p'); description.className = 'note';
        const descriptions = { coordinator: '管家负责理解需求与组织协作。', researcher: '研究角色帮助分析资料、区分证据与推测。', writer: '写作角色帮助组织内容和表达。', reviewer: '审阅角色帮助检查遗漏与问题。' };
        description.textContent = descriptions[agent.role] || '选择这个角色擅长的工作。'; row.append(description);
        const name = document.createElement('input'); name.value = agent.name; name.maxLength = 96;
        name.oninput = () => { if (valid() && !saving) { agent.name = name.value; title.textContent = name.value || '未命名角色'; enabled.setAttribute?.('aria-label', '启用 ' + name.value); } };
        row.append(field('名称', name));
        row.append(field('角色', select(catalog.roles, agent.role, value => { agent.role = value; description.textContent = descriptions[value] || ''; })));
        row.append(field('使用模型', select(catalog.models.map(model => model.id), agent.model_ref, value => { agent.model_ref = value; })));
        const enabled = document.createElement('input'); enabled.type = 'checkbox'; enabled.checked = agent.enabled; enabled.setAttribute?.('role', 'switch'); enabled.setAttribute?.('aria-label', '启用 ' + agent.name);
        enabled.onchange = () => { if (valid() && !saving) agent.enabled = enabled.checked; };
        const roleActions = document.createElement('div'); roleActions.className = 'profile-role-actions';
        roleActions.append(field('启用角色', enabled));
        const remove = button('移除', () => { agents.splice(index, 1); edit(); }, agents.length <= 1);
        remove.className = 'text-btn profile-remove';
        if (agents.length <= 1) remove.title = '至少保留一个角色';
        roleActions.append(remove); row.append(roleActions);
        controls.append(row);
      });
      const add = button('添加角色', () => {
        let id = 1; while (agents.some(agent => agent.id === 'agent-' + id)) id++;
        agents.push({ id: 'agent-' + id, name: '新角色', role: 'researcher', model_ref: catalog.models[0].id, enabled: true }); edit();
      }, agents.length >= catalog.max_agents);
      add.className = 'profile-add'; controls.append(add);
      const actions = document.createElement('div'); actions.className = 'profile-actions';
      actions.append(button('取消，不保存', discard)); actions.append(button('检查更改', preview));
      controls.append(actions);
    }
    function preview() {
      if (agents.filter(agent => agent.enabled && agent.role === 'coordinator').length !== 1) {
        status('请保留一位启用的管家，其他管家请停用或更换角色。'); return;
      }
      if (agents.some(agent => !agent.name.trim() || new TextEncoder().encode(agent.name).length > 96 || /[\x00-\x1f\x7f-\x9f]/.test(agent.name))) {
        status('请填写名称；名称过长或含特殊控制字符时，请缩短或修改。'); return;
      }
      const snapshot = agents.map(agent => ({ ...agent }));
      controls.replaceChildren(article('即将替换你自己的配置，不会共享给其他用户：\n' + snapshot.map(agent => `${agent.id} · ${agent.name} · ${agent.role} · ${agent.model_ref} · ${agent.enabled ? '启用' : '停用'}`).join('\n') + '\n\n' + notice));
      const confirm = button('确认保存个人配置', async () => {
        saving = true; confirm.disabled = true; back.disabled = true; cancel.disabled = true;
        try {
          const saved = await api(context, 'profile', 'PUT', { expected_revision: editing.revision, agents: snapshot });
          if (current(context) && draft === editing && JSON.stringify(editing.agents) === JSON.stringify(snapshot)) draft = null;
          if (!valid()) return;
          onSaved(saved.revision);
          controls.replaceChildren(article('已保存个人配置 · 版本 ' + saved.revision + '\n' + notice));
          saving = false;
          controls.append(button('重新读取配置', () => open(context)));
        } catch (error) {
          if (!valid()) return;
          saving = false;
          controls.replaceChildren(article('保存未确认：' + error.message + '。请重新读取核对服务端配置；不会自动重试。'));
          controls.append(button('重新读取配置', () => open(context)));
        }
      });
      const back = button('返回修改', edit);
      const cancel = button('取消，不保存', discard);
      const actions = document.createElement('div'); actions.className = 'profile-actions';
      actions.append(back); actions.append(cancel); actions.append(confirm); controls.append(actions);
    }
    panel.onkeydown = event => { if (event.key === 'Escape' && !saving) { event.stopPropagation?.(); close(); } };
    edit();
  }
  return { reset, close, hasUnsaved, async handle(context, text) {
    if (!['/agents', '配置个人agent', '配置个人Agent', '配置个人 Agent', '配置我的agent', '配置我的Agent'].includes(text)) return false;
    await open(context); return true;
  }};
}
