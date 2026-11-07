/* Personal role controls in the existing conversation, never a model/tool grant. */
function createProfileUI({ api, current, article, conversation, status, onSaved = () => {} }) {
  let generation = 0, card = null;
  const notice = '聊天默认使用启用的管家角色，其他启用角色可在输入框上方单次选择。每次只调用一个服务端模型，不启动 Agent 团队，也不授予工具权限；MOCK 不调用模型。请勿填写密钥。';
  function reset() { generation++; card = null; }
  function append(entries) { if (card) entries.push(card); }
  async function open(context) {
    const version = ++generation;
    if (card) card.replaceChildren(article('已关闭旧配置卡，正在重新读取。'));
    card = null;
    const valid = () => current(context) && generation === version;
    const panel = article('正在读取个人角色设置…');
    panel.className = 'profile-settings';
    panel.setAttribute?.('aria-label', '个人角色设置');
    card = panel;
    conversation().append(panel);
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
    const heading = document.createElement('h2'); heading.textContent = '个人角色设置';
    const introduction = document.createElement('p'); introduction.className = 'note'; introduction.textContent = notice;
    panel.append(heading); panel.append(introduction);
    let agents = profile.agents.map(agent => ({ ...agent }));
    let saving = false;
    const controls = document.createElement('div');
    panel.append(controls);
    function button(label, action, disabled = false) {
      const b = document.createElement('button');
      b.type = 'button'; b.textContent = label; b.disabled = disabled;
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
        name.oninput = () => { if (valid() && !saving) agent.name = name.value; };
        row.append(field('名称', name));
        row.append(field('角色', select(catalog.roles, agent.role, value => { agent.role = value; description.textContent = descriptions[value] || ''; })));
        row.append(field('使用模型', select(catalog.models.map(model => model.id), agent.model_ref, value => { agent.model_ref = value; })));
        const enabled = document.createElement('input'); enabled.type = 'checkbox'; enabled.checked = agent.enabled; enabled.setAttribute?.('role', 'switch'); enabled.setAttribute?.('aria-label', '启用 ' + agent.name);
        enabled.onchange = () => { if (valid() && !saving) agent.enabled = enabled.checked; };
        row.append(field('启用角色', enabled));
        row.append(button('移除', () => { agents.splice(index, 1); edit(); }, agents.length <= 1));
        controls.append(row);
      });
      controls.append(button('添加角色', () => {
        let id = 1; while (agents.some(agent => agent.id === 'agent-' + id)) id++;
        agents.push({ id: 'agent-' + id, name: '新角色', role: 'researcher', model_ref: catalog.models[0].id, enabled: true }); edit();
      }, agents.length >= catalog.max_agents));
      controls.append(button('检查更改', preview));
      controls.append(button('取消，不保存', close));
    }
    function close() {
      if (!valid()) return;
      generation++; card = null;
      panel.replaceChildren(article('个人配置编辑已关闭，未发送新的保存请求。'));
      document.getElementById?.('open-agents')?.focus?.();
    }
    function preview() {
      if (agents.filter(agent => agent.enabled && agent.role === 'coordinator').length !== 1) {
        status('请保留且只保留一位启用的 管家角色。'); return;
      }
      if (agents.some(agent => !agent.name.trim() || new TextEncoder().encode(agent.name).length > 96 || /[\x00-\x1f\x7f-\x9f]/.test(agent.name))) {
        status('名称需为 1–96 字节，不含控制字符。'); return;
      }
      const snapshot = agents.map(agent => ({ ...agent }));
      controls.replaceChildren(article('即将替换你自己的配置，不会共享给其他用户：\n' + snapshot.map(agent => `${agent.id} · ${agent.name} · ${agent.role} · ${agent.model_ref} · ${agent.enabled ? '启用' : '停用'}`).join('\n') + '\n\n' + notice));
      const confirm = button('确认保存个人配置', async () => {
        saving = true; confirm.disabled = true; back.disabled = true; cancel.disabled = true;
        try {
          const saved = await api(context, 'profile', 'PUT', { expected_revision: profile.revision, agents: snapshot });
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
      const cancel = button('取消，不保存', close);
      controls.append(confirm); controls.append(back); controls.append(cancel);
    }
    panel.onkeydown = event => { if (event.key === 'Escape' && !saving) { event.stopPropagation?.(); close(); } };
    edit();
  }
  return { reset, append, async handle(context, text) {
    if (!['/agents', '配置个人agent', '配置个人Agent', '配置个人 Agent', '配置我的agent', '配置我的Agent'].includes(text)) return false;
    await open(context); return true;
  }};
}
