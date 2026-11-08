/* Safe chat presentation and explicit message actions. */
// Markdown is built from DOM text nodes only: HTML and URLs are never executed.
function renderMarkdown(value) {
  const root = el('div', 'msg-body markdown');
  const lines = String(value ?? '').split('\n');
  function inline(parent, text) {
    const pattern = /(`[^`]+`|\*\*[^*]+\*\*|\*[^*]+\*)/g;
    let at = 0;
    for (const match of text.matchAll(pattern)) {
      if (match.index > at) parent.append(el('span', null, text.slice(at, match.index)));
      const token = match[0], strong = token.startsWith('**');
      parent.append(el(token[0] === '`' ? 'code' : strong ? 'strong' : 'em', null, token.slice(strong ? 2 : 1, strong ? -2 : -1)));
      at = match.index + token.length;
    }
    if (at < text.length) parent.append(el('span', null, text.slice(at)));
  }
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (/^\s*```/.test(line)) {
      const content = [];
      while (++i < lines.length && !/^\s*```/.test(lines[i])) content.push(lines[i]);
      const block = el('div', 'code-block'), pre = el('pre'), code = el('code', null, content.join('\n'));
      const copy = el('button', 'copy-code', '复制代码'); copy.type = 'button';
      copy.onclick = async () => {
        try { await navigator.clipboard.writeText(content.join('\n')); copy.textContent = '已复制'; }
        catch { copy.textContent = '复制失败，请选择代码复制'; }
      };
      pre.append(code); block.append(copy, pre); root.append(block); continue;
    }
    if (line.includes('|') && i + 1 < lines.length && /^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(lines[i + 1])) {
      const wrap = el('div', 'markdown-table'), table = el('table');
      const cells = row => row.trim().replace(/^\||\|$/g, '').split('|').map(s => s.trim());
      const head = el('tr'); for (const cell of cells(line)) { const th = el('th'); th.scope = 'col'; inline(th, cell); head.append(th); }
      const thead = el('thead'); thead.append(head); table.append(thead); i++;
      const body = el('tbody');
      while (i + 1 < lines.length && lines[i + 1].includes('|') && lines[i + 1].trim()) {
        const tr = el('tr'); for (const cell of cells(lines[++i])) { const td = el('td'); inline(td, cell); tr.append(td); } body.append(tr);
      }
      table.append(body); wrap.append(table); root.append(wrap); continue;
    }
    const list = line.match(/^\s*(?:[-*+] |\d+\. )(.+)/);
    if (list) {
      const ordered = /^\s*\d+\./.test(line), ul = el(ordered ? 'ol' : 'ul');
      let match = list;
      while (match) {
        const li = el('li'); inline(li, match[1]); ul.append(li);
        const next = lines[i + 1];
        match = next?.match(ordered ? /^\s*\d+\. (.+)/ : /^\s*[-*+] (.+)/);
        if (match) i++;
      }
      root.append(ul); continue;
    }
    if (!line.trim()) continue;
    const heading = line.match(/^(#{1,4}) (.+)/), quote = line.match(/^> ?(.*)/);
    const paragraph = el(heading ? 'h' + Math.min(heading[1].length + 2, 6) : quote ? 'blockquote' : 'p');
    inline(paragraph, heading ? heading[2] : quote ? quote[1] : line); root.append(paragraph);
  }
  return root;
}


const messagePanels = new Map();
function addMessageActions(bubble, context, message) {
  const messageKey = JSON.stringify([message.role, message.created_at, message.content]);
  const actions = el('div', 'message-actions');
  const copy = el('button', 'text-btn', '复制'); copy.type = 'button';
  copy.onclick = async () => {
    if (!current(context)) return;
    try { await navigator.clipboard.writeText(message.content); if (current(context)) status('已复制消息。'); }
    catch { if (current(context)) status('无法复制，请选择消息文本后复制。'); }
  };
  const delegate = el('button', 'text-btn', '把这段交给某人'); delegate.type = 'button';
  delegate.onclick = () => {
    if (!current(context) || messagePanels.has(messageKey)) return;
    const panel = el('section', 'message-delegation'); messagePanels.set(messageKey, panel);
    const recipient = el('input'); recipient.setAttribute('aria-label', '消息委派接收人'); recipient.placeholder = '接收人账号，可从联系人列表查看';
    const summary = el('textarea'); summary.value = message.content; summary.rows = 4; summary.setAttribute('aria-label', '消息委派摘要');
    const note = el('p', 'note', '先整理成提案，只有你能看到。核对并确认后，才会把摘要发给对方。');
    const create = el('button', 'primary', '生成委派提案'); create.type = 'button';
    const cancel = el('button', 'text-btn', '关闭'); cancel.type = 'button';
    let requestId = crypto.randomUUID(), requestValue = null;
    create.onclick = async () => {
      if (!current(context) || create.disabled) return;
      const owner = recipient.value.trim(), text = summary.value.trim();
      if (!/^[A-Za-z0-9_-]{1,64}$/.test(owner) || !text || new TextEncoder().encode(text).length > 4000) { note.textContent = '请填写接收人和任务摘要。摘要过长时，请删减后再试。'; return; }
      const value = JSON.stringify([owner, text]); if (requestValue !== value) { requestId = crypto.randomUUID(); requestValue = value; }
      create.disabled = true; cancel.disabled = true;
      try {
        await api(context, 'proposals', 'POST', { owner, summary: text, request_id: requestId });
        if (!current(context)) return;
        panel.remove?.(); messagePanels.delete(messageKey);
        status('已生成提案，尚未共享。请核对后确认。'); await refresh(context);
      } catch (error) { if (current(context)) note.textContent = error.message + '。已保留你填写的内容，可以重试。'; }
      finally { if (current(context)) { create.disabled = false; cancel.disabled = false; } }
    };
    cancel.onclick = () => { if (create.disabled) return; panel.remove?.(); messagePanels.delete(messageKey); delegate.focus?.(); };
    panel.onkeydown = event => { if (event.key === 'Escape') { event.stopPropagation(); cancel.onclick(); } };
    panel.append(note, recipient, summary, create, cancel); bubble.append(panel); recipient.focus?.();
  };
  const retry = el('button', 'text-btn', message.role === 'user' ? '重新问' : '继续追问'); retry.type = 'button';
  retry.onclick = () => {
    if (!current(context)) return;
    if ($('text').value.trim() && !confirm('输入框有未发送的内容，是否替换为这条消息？')) return;
    $('text').value = message.role === 'user' ? message.content : '关于这段回复，我想进一步了解：\n' + message.content;
    $('text').oninput?.(); $('text').focus?.();
    status('已填入输入框，可以修改后再发送。');
  };
  actions.append(copy, delegate, retry); bubble.append(actions);
  if (messagePanels.has(messageKey)) bubble.append(messagePanels.get(messageKey));
}

function renderChatExamples() {
  const box = el('section', 'chat-examples');
  box.append(el('h2', null, '有什么需要帮忙的？'), el('p', 'muted', '直接告诉我就好。需要请同事帮忙时，会先由你确认。'));
  for (const text of ['帮我梳理今天的工作重点', '帮我检查这份文档', '把我的想法整理成待办事项']) {
    const button = el('button', 'example-prompt', text); button.type = 'button';
    button.onclick = () => {
      if ($('text').value.trim()) { status('输入框里还有没发送的内容，请先发送或清空，再选择示例。'); return; }
      $('text').value = text; $('text').oninput?.(); $('text').focus?.();
    };
    box.append(button);
  }
  return box;
}

function renderConversation(context, messages, proposals) {
  const separators = new Map();
  let previous = null;
  messages.forEach((message, index) => {
    const at = parseUtc(message.created_at);
    if (!at) return;
    if (!previous || at - previous >= 10 * 60 * 1000 || at.toDateString() !== previous.toDateString()) {
      separators.set(index, el('div', 'time-sep', formatWhen(at)));
    }
    previous = at;
  });
  const entries = messages.map(message => {
    const notice = message.role !== 'user' && /^\[(系统提示|MOCK)/.test(message.content);
    const bubble = el('article', 'msg ' + (message.role === 'user' ? 'msg-user' : notice ? 'msg-notice' : 'msg-assistant')
      + (message.status === 'failed' ? ' msg-failed' : ''));
    bubble.append(message.role === 'assistant' && !notice ? renderMarkdown(message.content) : el('div', 'msg-body', message.content));
    addMessageActions(bubble, context, message);
    if (message.status !== 'complete') bubble.append(el('div', 'msg-status', MESSAGE_STATUS[message.status] || message.status));
    return bubble;
  });
  // Place each proposal right after the exchange that produced it. A proposal's
  // summary is an exact excerpt of its source user message; unmatched proposals
  // (e.g. /delegate or test-double turns) stay at the end as before.
  // Each proposal claims the latest unclaimed matching user message, so the
  // result does not depend on the order the proposals API returns.
  const after = new Map();
  const claimed = new Set();
  const trailing = [];
  const oldProposals = proposals.filter(isOldProposal);
  for (const proposal of proposals.filter(proposal => !isOldProposal(proposal))) {
    let at = -1;
    for (let i = messages.length - 1; i >= 0; i--) {
      if (!claimed.has(i) && messages[i].role === 'user' && typeof proposal.summary === 'string' && proposal.summary
          && String(messages[i].content).includes(proposal.summary)) { at = i; break; }
    }
    if (at === -1) { trailing.push(proposal); continue; }
    claimed.add(at);
    const anchor = messages[at + 1] && messages[at + 1].role !== 'user' ? at + 1 : at;
    if (!after.has(anchor)) after.set(anchor, []);
    after.get(anchor).push(proposal);
  }
  // Unmatched proposals (e.g. /delegate, or a source message outside the loaded
  // window) are placed by their own timestamp; undated legacy ones predate the
  // window and go first. Nothing piles up at the bottom of the conversation.
  const leading = [];
  for (const proposal of trailing) {
    const at = parseUtc(proposal.created_at);
    let anchor = -1, anyDated = false;
    if (at) messages.forEach((message, index) => {
      const when = parseUtc(message.created_at);
      if (when) { anyDated = true; if (when <= at) anchor = index; }
    });
    if (anchor >= 0) {
      if (!after.has(anchor)) after.set(anchor, []);
      after.get(anchor).push(proposal);
    } else if (at && !anyDated && messages.length) {
      if (!after.has(messages.length - 1)) after.set(messages.length - 1, []);
      after.get(messages.length - 1).push(proposal);
    } else leading.push(proposal);
  }
  if (!messages.length && !proposals.length) entries.push(renderChatExamples());
  const ordered = leading.map(proposal => ({ proposal }));
  entries.forEach((bubble, index) => {
    if (separators.has(index)) ordered.push({ bubble: separators.get(index) });
    ordered.push({ bubble });
    for (const proposal of after.get(index) || []) ordered.push({ proposal });
  });
  entries.length = 0;
  for (const item of ordered) {
    if (item.bubble) { entries.push(item.bubble); continue; }
    const proposal = item.proposal;
    const element = el('article', 'card proposal' + (proposal.status === 'pending' ? '' : ' settled'));
    element.proposalId = proposal.id;
    const head = el('header', 'card-head');
    head.append(el('span', 'card-kicker', '委派提案'), badge(PROPOSAL_STATUS, proposal.status));
    element.append(head, el('div', 'field', '接收人：' + proposal.owner), el('blockquote', 'summary-quote', proposal.summary));
    if (proposal.task_id) element.append(el('div', 'meta', '已创建任务 #' + String(proposal.task_id).slice(0, 8)));
    if (proposal.status === 'pending') {
      element.append(el('p', 'note', '确认后才会创建任务，并将接收人、这份摘要和附带材料共享给对方。你的私聊不会发送。'));
      const label = el('label', null, '共享材料（可选）：会原样发给对方，并用于生成草稿。请勿填写密钥或私密信息。');
      const box = el('textarea');
      box.rows = 3;
      box.maxLength = MAX_DETAILS_BYTES;
      box.placeholder = '粘贴对方完成任务需要的内容，例如文档正文或需求说明…';
      const key = 'proposals:' + proposal.id;
      box.value = detailDrafts.get(key) ?? '';
      box.oninput = () => detailDrafts.set(key, box.value);
      attachMaterialFile(label, box, context, key);
      label.append(box);
      // Collapsed until asked for, unless the user already typed something.
      const reveal = el('button', 'text-btn add-material', '＋ 附带共享材料（可选）');
      reveal.type = 'button';
      if (!box.value) label.classList.add('hide');
      else reveal.classList.add('hide');
      reveal.onclick = () => { label.classList.remove('hide'); reveal.classList.add('hide'); box.focus?.(); };
      const actions = el('div', 'actions');
      actions.detailsBox = box;
      element.detailsBox = box;
      element.append(reveal, label, actions);
      addActions(actions, context, 'proposals', proposal, [['confirm', '确认共享并委派'], ['edit', '修改后确认'], ['reject', '拒绝，不共享']]);
    }
    const proposalEditor = actionEditors.get('proposals:' + proposal.id);
    if (proposalEditor) {
      element.append(proposalEditor.panel);
      if (proposal.revision !== proposalEditor.revision || proposal.status !== 'pending') { proposalEditor.stale = true; proposalEditor.submit.disabled = true; proposalEditor.note.textContent = '提案已变更，请关闭编辑框后重新核对。输入仍保留。'; }
    }
    entries.push(element);
  }
  if (oldProposals.length) {
    const group = el('details', 'old-proposals');
    group.open = !!document.querySelector?.('.old-proposals')?.open || oldProposals.some(proposal => actionEditors.has('proposals:' + proposal.id));
    group.append(el('summary', null, `${oldProposals.length} 个未处理的提案`));
    const reject = el('button', 'danger', '全部拒绝，不共享'); reject.type = 'button';
    reject.disabled = busy.has('proposals:bulk-reject');
    reject.onclick = async () => {
      if (!current(context) || reject.disabled || busy.has('proposals:bulk-reject')) return;
      busy.add('proposals:bulk-reject'); reject.disabled = true;
      let done = 0;
      try {
        for (const proposal of oldProposals) {
          await api(context, 'proposals/' + encodeURIComponent(proposal.id) + '/actions', 'POST', { action: 'reject', expected_revision: proposal.revision });
          done++;
        }
        if (current(context)) status(`已拒绝 ${done} 个提案，未共享。`);
      } catch (error) { if (current(context)) status(`已拒绝 ${done} 个提案；其余未完成：` + error.message); }
      finally { if (current(context)) { busy.delete('proposals:bulk-reject'); reject.disabled = false; await refresh(context).catch(error => status(error.message)); } }
    };
    group.append(reject);
    for (const proposal of oldProposals) {
      const row = el('article', 'old-proposal'); row.proposalId = proposal.id;
      row.append(el('p', null, proposal.owner + '：' + proposal.summary));
      addActions(row, context, 'proposals', proposal, [['confirm', '确认共享并委派'], ['edit', '修改后确认'], ['reject', '拒绝，不共享']]);
      const editor = actionEditors.get('proposals:' + proposal.id);
      if (editor) {
        row.append(editor.panel);
        if (proposal.revision !== editor.revision) { editor.stale = true; editor.submit.disabled = true; editor.note.textContent = '提案已变更，请关闭编辑框后重新核对。'; }
      }
      group.append(row);
    }
    entries.push(group);
  }
  return entries;
}
