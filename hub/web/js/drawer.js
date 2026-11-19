// Right workbench drawer: 3 clean tabs (Tasks & Nodes / Ledgers & Crons / Brain & Settings).
import { api, esc, fmtTime, showToast } from './api.js';

export function initDrawer({ state, onRefresh }) {
  const bodyEl = document.getElementById('drawer-body');
  const tabBtns = Array.from(document.querySelectorAll('.tab'));
  let activeTab = 'work';
  let lastInstallCmd = '';
  let integrations = { gitlab: null, im: null };

  tabBtns.forEach((btn) => {
    btn.onclick = async () => {
      activeTab = btn.dataset.tab;
      tabBtns.forEach((b) => {
        const on = b === btn;
        b.classList.toggle('active', on);
        b.setAttribute('aria-selected', String(on));
      });
      if (activeTab === 'settings') {
        await loadIntegrations();
      }
      render();
    };
  });

  async function loadIntegrations() {
    try {
      const [gitlab, im] = await Promise.all([
        api('/api/integrations/gitlab'),
        api('/api/integrations/im'),
      ]);
      integrations = { gitlab, im };
    } catch {
      // ignore transient errors
    }
  }

  function renderWork() {
    const tasks = state.tasks || [];
    const nodes = state.nodes || [];
    const uid = state.user?.id;

    const tasksHtml = tasks.length
      ? tasks
          .map((t) => {
            const isOwner = t.owner_id === uid;
            const actions = [];
            if (isOwner && (t.status === 'pending' || t.status === 'failed')) {
              actions.push(`<button class="btn sm primary" data-assign="${esc(t.id)}">派给节点</button>`);
            }
            if (isOwner && t.status !== 'done' && t.status !== 'canceled') {
              actions.push(`<button class="btn sm" data-deliver="${esc(t.id)}">交付结果</button>`);
            }
            if (t.status === 'pending' || t.status === 'running') {
              actions.push(`<button class="btn sm danger" data-cancel="${esc(t.id)}">取消</button>`);
            }
            const progress = t.stage ? `<div class="item-sub">进度：${esc(t.stage)}${t.log_tail ? `\n${esc(t.log_tail)}` : ''}</div>` : '';
            const res = t.result ? `<div class="item-sub">${esc(t.result.slice(0, 220))}</div>` : '';
            return `
              <div class="item">
                <div class="item-head">
                  <span>${esc(t.title)}</span>
                  <span class="pill ${t.status === 'done' ? 'ok' : t.status === 'running' ? 'warn' : ''}">${esc(t.status)}</span>
                </div>
                ${progress}${res}
                ${actions.length ? `<div class="row">${actions.join('')}</div>` : ''}
              </div>`;
          })
          .join('')
      : '<p class="muted small">暂无任务</p>';

    const nodesHtml = nodes.length
      ? nodes
          .map(
            (n) => `
          <div class="item">
            <div class="item-head">
              <span>${esc(n.name)} <span class="muted small">(${esc(n.kind)} · ${esc(n.id)})</span></span>
              <span class="pill ${n.online ? 'ok' : ''}">${n.online ? '在线' : '离线'}</span>
            </div>
            ${n.capabilities ? `<div class="item-sub">${esc(n.capabilities)}</div>` : ''}
            <div class="row">
              <button class="btn sm" data-rotate="${esc(n.id)}">重置令牌</button>
              <button class="btn sm danger" data-rm-node="${esc(n.id)}">移除</button>
            </div>
          </div>`,
          )
          .join('')
      : '<p class="muted small">尚未接入 PC 或云端执行节点</p>';

    bodyEl.innerHTML = `
      <section class="card">
        <h3>任务与协作 (${tasks.length})</h3>
        ${tasksHtml}
      </section>
      <section class="card">
        <h3>执行节点 · PC & Cloud (${nodes.length})</h3>
        ${nodesHtml}
        <form id="add-node-form" class="mt-10">
          <div class="row">
            <input type="text" name="name" placeholder="节点名称 (如 MacBook / 云端容器)" required class="flex-1">
            <select name="kind" class="w-kind">
              <option value="pc">PC</option>
              <option value="cloud">Cloud</option>
            </select>
            <button type="submit" class="btn primary sm">添加</button>
          </div>
        </form>
        ${lastInstallCmd ? `<div class="install-box"><strong>一次性接入命令：</strong><br><code>${esc(lastInstallCmd)}</code></div>` : ''}
      </section>`;

    bodyEl.querySelectorAll('[data-assign]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/tasks/${b.dataset.assign}/action`, {
            method: 'POST',
            body: { action: 'assign_node' },
          });
          showToast('已派发给节点');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-deliver]').forEach((b) => {
      b.onclick = async () => {
        const result = window.prompt('输入交付结果内容：');
        if (!result?.trim()) return;
        try {
          await api(`/api/tasks/${b.dataset.deliver}/action`, {
            method: 'POST',
            body: { action: 'deliver', result: result.trim() },
          });
          showToast('已交付');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-cancel]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/tasks/${b.dataset.cancel}/action`, {
            method: 'POST',
            body: { action: 'cancel' },
          });
          showToast('任务已取消');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-rotate]').forEach((b) => {
      b.onclick = async () => {
        try {
          const r = await api(`/api/nodes/${b.dataset.rotate}/token`, { method: 'POST' });
          lastInstallCmd = `AGENCT_NODE_TOKEN=${r.token}`;
          showToast('节点令牌已重置');
          render();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    bodyEl.querySelectorAll('[data-rm-node]').forEach((b) => {
      b.onclick = async () => {
        try {
          await api(`/api/nodes/${b.dataset.rmNode}`, { method: 'DELETE' });
          showToast('已移除节点');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    });
    const addForm = document.getElementById('add-node-form');
    if (addForm) {
      addForm.onsubmit = async (ev) => {
        ev.preventDefault();
        const fd = new FormData(addForm);
        try {
          const r = await api('/api/nodes', {
            method: 'POST',
            body: { name: fd.get('name'), kind: fd.get('kind') },
          });
          lastInstallCmd = r.install_unix;
          showToast('节点已创建，请复制下方接入命令');
          await onRefresh();
        } catch (err) {
          showToast(err.message);
        }
      };
    }
  }

  function renderMemory() {
    const ledgers = state.ledgers || [];
    const crons = state.crons || [];
    const audits = state.audit_logs || [];

    bodyEl.innerHTML = `
      <section class="card">
        <h3>专项活账本 (${ledgers.length})</h3>
        ${
          ledgers.length
            ? ledgers
                .map(
                  (l) => `
              <div class="item">
                <div class="item-head">
                  <span>${esc(l.title)}</span>
                  <span class="pill">${esc(l.status)}</span>
                </div>
                <div class="item-sub">${esc(l.notes)}</div>
                <div class="row"><button class="btn sm danger" data-del-ledger="${esc(l.id)}">删除</button></div>
              </div>`,
                )
                .join('')
            : '<p class="muted small">暂无活账本</p>'
        }
        <form id="add-ledger-form" class="mt-10">
          <input type="text" name="title" placeholder="新增台账标题" required>
          <textarea class="field mt-6" name="notes" rows="2" placeholder="要点 / 规则…"></textarea>
          <div class="row"><button type="submit" class="btn primary sm">保存台账</button></div>
        </form>
      </section>

      <section class="card">
        <h3>定时任务 (${crons.length})</h3>
        ${
          crons.length
            ? crons
                .map(
                  (c) => `
              <div class="item">
                <div class="item-head">
                  <span>${c.daily_time_cst ? `每天 ${esc(c.daily_time_cst)}` : `每 ${c.interval_minutes} 分钟`}</span>
                  <span class="pill">${esc(c.target)}</span>
                </div>
                <div class="item-sub">${esc(c.instruction)}</div>
                <div class="row"><button class="btn sm danger" data-del-cron="${esc(c.id)}">删除</button></div>
              </div>`,
                )
                .join('')
            : '<p class="muted small">暂无定时任务</p>'
        }
        <form id="add-cron-form" class="mt-10">
          <input type="text" name="instruction" placeholder="定时指令（如：汇总今日 GitLab 提交）" required>
          <div class="row">
            <input type="text" name="daily_time_cst" placeholder="每日 HH:MM (可选)" class="flex-1">
            <select name="target" class="w-kind">
              <option value="cloud">云端直跑</option>
              <option value="cloud-node">云端节点</option>
              <option value="auto">自动节点</option>
            </select>
            <button type="submit" class="btn primary sm">添加</button>
          </div>
        </form>
      </section>

      <section class="card">
        <h3>审计记录 (只追加)</h3>
        ${
          audits.length
            ? audits
                .slice(0, 15)
                .map(
                  (a) => `
              <div class="item">
                <div class="item-head"><span>${esc(a.title)}</span><span class="muted small">${esc(fmtTime(a.created_at))}</span></div>
                <div class="item-sub">${esc(a.action)} · ${esc(a.status)}${a.detail ? ` — ${esc(a.detail)}` : ''}</div>
              </div>`,
                )
                .join('')
            : '<p class="muted small">暂无审计记录</p>'
        }
      </section>`;

    bodyEl.querySelectorAll('[data-del-ledger]').forEach((b) => {
      b.onclick = async () => {
        await api(`/api/ledgers/${b.dataset.delLedger}`, { method: 'DELETE' });
        await onRefresh();
      };
    });
    bodyEl.querySelectorAll('[data-del-cron]').forEach((b) => {
      b.onclick = async () => {
        await api(`/api/automation/crons/${b.dataset.delCron}`, { method: 'DELETE' });
        await onRefresh();
      };
    });
    document.getElementById('add-ledger-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const fd = new FormData(ev.target);
      await api('/api/ledgers', {
        method: 'POST',
        body: { title: fd.get('title'), notes: fd.get('notes'), status: 'active' },
      });
      showToast('台账已保存');
      await onRefresh();
    };
    document.getElementById('add-cron-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const fd = new FormData(ev.target);
      const daily = String(fd.get('daily_time_cst') || '').trim();
      await api('/api/automation/crons', {
        method: 'POST',
        body: {
          instruction: fd.get('instruction'),
          daily_time_cst: daily || null,
          interval_minutes: 60,
          target: fd.get('target'),
        },
      });
      showToast('定时任务已创建');
      await onRefresh();
    };
  }

  function renderSettings() {
    const p = state.profile || { name: 'Agenct', soul_md: '', memory_md: '' };
    const perms = state.permissions || {};
    const gl = integrations.gitlab || {};
    const im = integrations.im || {};
    const contacts = state.contacts || [];

    bodyEl.innerHTML = `
      <section class="card">
        <h3>单脑人设与记忆</h3>
        <form id="brain-form">
          <label class="field-label">管家名称</label>
          <input type="text" name="name" value="${esc(p.name)}" maxlength="48" required>
          <label class="field-label">SOUL.md</label>
          <textarea class="field" name="soul_md" rows="5">${esc(p.soul_md)}</textarea>
          <label class="field-label">MEMORY.md</label>
          <textarea class="field" name="memory_md" rows="6">${esc(p.memory_md)}</textarea>
          <div class="row"><button type="submit" class="btn primary wide">保存人设与记忆</button></div>
        </form>
      </section>

      <section class="card">
        <h3>自动化权限（默认全关）</h3>
        <form id="perms-form">
          <label class="toggle"><span>自动接手同事协作并派发节点</span><input type="checkbox" name="auto_accept" ${perms.auto_accept ? 'checked' : ''}></label>
          <label class="toggle"><span>节点完成后自动交付给同事</span><input type="checkbox" name="auto_deliver" ${perms.auto_deliver ? 'checked' : ''}></label>
          <label class="toggle"><span>GitLab @提及后自动派发节点执行</span><input type="checkbox" name="gitlab_execute" ${perms.gitlab_execute ? 'checked' : ''}></label>
          <label class="toggle"><span>GitLab 执行结果自动评论回写</span><input type="checkbox" name="gitlab_reply" ${perms.gitlab_reply ? 'checked' : ''}></label>
          <div class="row"><button type="submit" class="btn primary sm">保存权限</button></div>
        </form>
      </section>

      <section class="card">
        <h3>GitLab 接入</h3>
        <form id="gl-form">
          <label class="field-label">GitLab 实例（受服务器白名单限制）</label>
          <input type="url" name="base_url" value="${esc(gl.base_url || '')}">
          <label class="field-label">Personal Access Token ${gl.configured ? `(已保存 ${esc(gl.token_preview)})` : '(未配置)'}</label>
          <input type="password" name="token" placeholder="留空保持不变">
          <label class="toggle"><span>定时轮询 GitLab Todo</span><input type="checkbox" name="auto_poll_todos" ${gl.auto_poll_todos ? 'checked' : ''}></label>
          <div class="row"><button type="submit" class="btn primary sm">保存 GitLab 配置</button></div>
        </form>
      </section>

      <section class="card">
        <h3>IM 通知通道（防 SSRF 域名白名单）</h3>
        <form id="im-form">
          <label class="field-label">平台</label>
          <select name="provider">
            ${['feishu', 'wecom', 'dingtalk'].map((k) => `<option value="${k}" ${im.provider === k ? 'selected' : ''}>${k}</option>`).join('')}
          </select>
          <label class="field-label">Webhook URL ${im.configured ? '(已配置)' : '(未配置)'}</label>
          <input type="url" name="webhook_url" placeholder="https://open.feishu.cn/open-apis/bot/v2/hook/...">
          <div class="row">
            <button type="submit" class="btn primary sm">保存</button>
            <button type="button" id="im-test-btn" class="btn sm">发送测试</button>
          </div>
        </form>
      </section>

      <section class="card">
        <h3>活跃同事 (${contacts.length})</h3>
        ${
          contacts.length
            ? contacts
                .map((c) => `<div class="item"><strong>@${esc(c.username)}</strong> <span class="muted small">${esc(c.display_name)}</span></div>`)
                .join('')
            : '<p class="muted small">暂无其他同事登录</p>'
        }
      </section>`;

    document.getElementById('brain-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const fd = new FormData(ev.target);
      await api('/api/brain', {
        method: 'PUT',
        body: { name: fd.get('name'), soul_md: fd.get('soul_md'), memory_md: fd.get('memory_md') },
      });
      showToast('已保存人设与记忆');
      await onRefresh();
    };
    document.getElementById('perms-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const f = ev.target;
      await api('/api/automation/permissions', {
        method: 'PUT',
        body: {
          auto_accept: f.auto_accept.checked,
          auto_deliver: f.auto_deliver.checked,
          gitlab_execute: f.gitlab_execute.checked,
          gitlab_reply: f.gitlab_reply.checked,
        },
      });
      showToast('权限已更新');
      await onRefresh();
    };
    document.getElementById('gl-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const f = ev.target;
      try {
        await api('/api/integrations/gitlab', {
          method: 'PUT',
          body: {
            base_url: f.base_url.value.trim(),
            token: f.token.value.trim() || null,
            auto_poll_todos: f.auto_poll_todos.checked,
          },
        });
        showToast('GitLab 配置已保存');
        await loadIntegrations();
        render();
      } catch (err) {
        showToast(err.message);
      }
    };
    document.getElementById('im-form').onsubmit = async (ev) => {
      ev.preventDefault();
      const f = ev.target;
      try {
        const res = await api('/api/integrations/im', {
          method: 'PUT',
          body: {
            provider: f.provider.value,
            webhook_url: f.webhook_url.value.trim() || null,
          },
        });
        showToast(res.inbound_token ? `已保存（入站令牌：${res.inbound_token}）` : 'IM 配置已保存');
        await loadIntegrations();
        render();
      } catch (err) {
        showToast(err.message);
      }
    };
    document.getElementById('im-test-btn').onclick = async () => {
      try {
        await api('/api/integrations/im/test', { method: 'POST' });
        showToast('测试消息已发送');
      } catch (err) {
        showToast(err.message);
      }
    };
  }

  function render() {
    if (activeTab === 'work') renderWork();
    else if (activeTab === 'memory') renderMemory();
    else renderSettings();
  }

  return { render };
}
