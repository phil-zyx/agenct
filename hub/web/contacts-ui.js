/* Shared conversations are separate from the owner's private assistant history. */
let peerState = null, peerGeneration = 0, peerReadFlight = null;
let directoryState = null, peerIndexFlight = null;
async function contactApi(context, path, method = 'GET', body) {
  const request = new AbortController();
  const abort = () => request.abort();
  context.signal.addEventListener('abort', abort, { once: true });
  const timeout = setTimeout(abort, READ_TIMEOUT);
  try { return await api({ ...context, signal: request.signal }, path, method, body); }
  finally { clearTimeout(timeout); context.signal.removeEventListener('abort', abort); }
}
const peerDrafts = new Map();
function peerName(id) { return directoryState?.people.get(id)?.display_name || id; }
function clearPeerSession() {
  closePeerConversation(); peerDrafts.clear(); directoryState = null; peerIndexFlight = null;
  $('contact-sidebar').replaceChildren(); $('contact-sidebar').classList.add('hide');
  $('room').classList.remove('contact-mode', 'contact-list-open');
  $('contacts-badge').textContent = '';
}
function closePeerConversation() {
  peerGeneration++; peerState = null; peerReadFlight = null;
  $('peer-view').classList.add('hide'); $('peer-messages').replaceChildren();
  $('peer-message-input').value = ''; $('peer-send-status').textContent = '';
  $('room').classList.remove('peer-active', 'contact-mode', 'contact-list-open');
  $('contact-sidebar').classList.add('hide');
}
function hideCollaborationSettings() { collaborationProfileUI.close(); }
function restorePrivateChat() {
  if (typeof closeProjects === 'function') closeProjects();
  closeContacts(); closePeerConversation(); profileUI.reset(); hideCollaborationSettings();
  setTasksOpen(false); $('open-chat').classList.add('active');
  $('open-chat').setAttribute('aria-current', 'page'); $('text').focus?.();
}
async function openContactDirectory() {
  if (typeof closeProjects === 'function') closeProjects();
  const context = session(); if (!current(context)) return;
  const owner = context.epoch + ':' + context.user;
  if (contactDraftOwner !== owner) { clearContactDrafts(); contactDraftOwner = owner; }
  profileUI.reset(); hideCollaborationSettings(); closeContacts(); setTasksOpen(false);
  $('room').classList.add('contact-mode', 'contact-list-open');
  $('contact-sidebar').classList.remove('hide'); $('open-contacts').classList.add('active');
  $('open-contacts').setAttribute('aria-expanded', 'true');
  const host = $('contact-sidebar');
  const heading = el('header', 'contact-directory-heading');
  const back = el('button', 'text-btn', '返回我的助手'); back.type = 'button'; back.onclick = restorePrivateChat;
  heading.append(el('h2', null, '联系人'), back);
  const search = el('input'); search.type = 'search'; search.placeholder = '搜索姓名或账号'; search.setAttribute('aria-label', '搜索联系人');
  const list = el('div', 'contacts-list', '正在读取联系人…');
  host.replaceChildren(heading, search, list);
  const version = ++peerGeneration;
  try {
    const [contacts, people, conversations] = await Promise.all([
      contactApi(context, 'contacts'), contactApi(context, 'people'), contactApi(context, 'conversations')
    ]);
    if (!current(context) || version !== peerGeneration) return;
    if (!Array.isArray(contacts) || !Array.isArray(people) || !Array.isArray(conversations)) throw Error('联系人列表格式无效');
    directoryState = { contacts, people: new Map(people.map(p => [p.id, p])), conversations, search, list };
    search.oninput = renderContactDirectory; renderContactDirectory(); search.focus?.();
  } catch (error) {
    if (!current(context) || version !== peerGeneration) return;
    list.replaceChildren(el('p', 'note', '无法读取联系人：' + error.message));
    const retry = el('button', null, '重新读取'); retry.type = 'button'; retry.onclick = openContactDirectory; list.append(retry);
  }
}
function renderContactDirectory() {
  if (!directoryState) return;
  const { contacts, people, conversations, search, list } = directoryState;
  const recents = new Map(conversations.map(c => [c.contact_id, c]));
  const term = search.value.trim().toLowerCase();
  const matches = contacts.filter(id => (id + ' ' + (people.get(id)?.display_name || '')).toLowerCase().includes(term));
  matches.sort((a, b) => (recents.get(b)?.last_message?.id || 0) - (recents.get(a)?.last_message?.id || 0));
  list.replaceChildren();
  if (!matches.length) list.append(el('p', 'note', contacts.length ? '没有匹配的联系人。' : '同事登录加入后，就可以在这里联系他。'));
  for (const id of matches) {
    const person = people.get(id), recent = recents.get(id);
    const row = el('button', 'contact-row'); row.type = 'button'; row.dataset.contact = id;
    if (peerState?.id === id) row.setAttribute('aria-current', 'true');
    const copy = el('span', 'contact-row-copy'); copy.append(el('span', 'contact-name', person?.display_name || id));
    const last = recent?.last_message;
    copy.append(el('span', 'contact-preview', last ? (last.author_kind === 'assistant' ? '助手：' : '') + last.body : '查看资料，开始沟通'));
    row.append(el('span', 'contact-avatar avatar-' + (person?.color || 'green'), (person?.display_name || id).slice(0, 1).toUpperCase()), copy);
    if (recent?.unread_count) row.append(el('span', 'contact-unread', String(recent.unread_count)));
    row.onclick = () => openPeerConversation(id); list.append(row);
  }
}
async function openPeerConversation(id) {
  if (typeof closeProjects === 'function') closeProjects();
  const context = session(); if (!current(context)) return;
  profileUI.reset(); hideCollaborationSettings(); closeContacts(); setTasksOpen(false);
  const generation = ++peerGeneration;
  const draftKey = context.epoch + ':' + context.user + ':' + id;
  const draft = peerDrafts.get(draftKey) || { value: '', requestId: null, requestValue: null, pending: null, confirmed: [] };
  peerDrafts.set(draftKey, draft);
  peerState = { id, context, generation, draft, messages: [], snapshot: '', loading: true };
  $('room').classList.add('peer-active', 'contact-mode'); $('room').classList.remove('contact-list-open');
  $('contact-sidebar').classList.remove('hide'); $('peer-view').classList.remove('hide');
  $('open-chat').classList.remove('active'); $('open-chat').removeAttribute('aria-current');
  $('open-contacts').classList.add('active'); $('open-contacts').setAttribute('aria-expanded', 'true');
  $('peer-name').textContent = peerName(id);
  $('peer-visibility').textContent = '与 ' + peerName(id) + ' 共享 · 助手代答会单独标明';
  $('peer-input-label').textContent = '发给 ' + peerName(id);
  $('peer-message-input').value = draft.value;
  $('peer-send-status').textContent = '';
  renderPeerMessages(); renderContactDirectory();
  if (globalThis.innerWidth > 1100) void showPeerDetails();
  await refreshPeerMessages();
  if (peerState?.generation === generation) $('peer-message-input').focus?.();
}
function renderPeerMessages() {
  const state = peerState; if (!state) return;
  const host = $('peer-messages'), stick = host.scrollHeight - host.scrollTop - host.clientHeight < 140;
  const entries = [];
  if (!state.messages.length) entries.push(el('p', 'peer-empty note', state.loading ? '正在读取消息…' : '从一个问题开始。需要明确交付结果时，可以请对方帮忙。'));
  for (const message of state.messages) {
    const own = message.sender === state.context.user;
    const row = el('article', 'peer-message' + (own ? ' own' : '') + (message.author_kind === 'assistant' ? ' assistant' : ''));
    const label = (own ? '你' : peerName(message.sender)) + (message.author_kind === 'assistant' ? '的助手 · 自动回复' : ' · 本人');
    row.append(el('p', 'peer-message-author', label), el('p', 'peer-message-body', message.body));
    const sentAt = parseUtc(message.created_at);
    const timestamp = el('time', 'peer-message-time', sentAt ? formatWhen(sentAt) : '');
    if (sentAt) timestamp.setAttribute('datetime', sentAt.toISOString()); row.append(timestamp); entries.push(row);
  }
  if (state.draft.pending) {
    const pending = state.draft.pending;
    const row = el('article', 'peer-message own pending');
    row.append(el('p', 'peer-message-author', '你 · 本人'), el('p', 'peer-message-body', pending.body));
    const label = el('p', 'note', pending.rejected ? '消息未发送，请修改后再试。' : pending.error ? '发送结果未确认，重试不会重复发送。' : '正在发送…'); label.setAttribute('role', 'status'); row.append(label);
    if (pending.error) {
      const retry = el('button', null, pending.rejected ? '取回并编辑' : '重试发送'); retry.type = 'button';
      retry.onclick = pending.rejected ? () => {
        state.draft.value = pending.body + (state.draft.value ? '\n' + state.draft.value : '');
        state.draft.pending = null; state.draft.requestId = null; state.draft.requestValue = null;
        $('peer-message-input').value = state.draft.value; renderPeerMessages(); $('peer-message-input').focus?.();
      } : sendPeerMessage; row.append(retry);
    }
    entries.push(row);
  }
  host.replaceChildren(...entries);
  $('peer-send').disabled = !!state.draft.pending;
  if (stick || state.draft.pending) host.scrollTop = host.scrollHeight;
}
async function refreshPeerMessages() {
  const state = peerState; if (!state || !current(state.context)) return;
  if (peerReadFlight?.state === state) return peerReadFlight.promise;
  const flight = { state };
  peerReadFlight = flight;
  flight.promise = (async () => {
    try {
      const messages = await contactApi(state.context, 'contacts/' + encodeURIComponent(state.id) + '/messages');
      if (peerState !== state || !current(state.context)) return;
      if (!Array.isArray(messages)) throw Error('消息格式无效');
      // A GET started before a successful POST must not erase the confirmed bubble.
      const merged = new Map(messages.map(message => [message.id, message]));
      for (const message of state.draft.confirmed) merged.set(message.id, message);
      const visible = [...merged.values()].sort((a, b) => a.id - b.id).slice(-100);
      const snapshot = JSON.stringify(visible);
      state.loading = false;
      if (state.snapshot !== snapshot) { state.messages = visible; state.snapshot = snapshot; renderPeerMessages(); }
      if (!state.draft.pending) $('peer-send-status').textContent = '';
      else renderPeerMessages();
      const latest = visible.at(-1);
      if (latest && !document.hidden && !$('room').classList.contains('contact-list-open') && !document.querySelector('#peer-view')?.inert && state.readThrough !== latest.id) {
        await contactApi(state.context, 'contacts/' + encodeURIComponent(state.id) + '/read', 'POST', { through_id: latest.id });
        if (peerState === state) state.readThrough = latest.id;
      }
    } catch (error) {
      if (peerState === state && current(state.context)) {
        state.loading = false; renderPeerMessages(); $('peer-send-status').textContent = '消息同步失败：' + error.message + '。稍后自动重试。';
      }
    }
  })().finally(() => { if (peerReadFlight === flight) peerReadFlight = null; });
  return flight.promise;
}
async function refreshPeerContacts(context) {
  if (peerIndexFlight?.epoch === context.epoch) return peerIndexFlight.promise;
  const flight = { epoch: context.epoch }; peerIndexFlight = flight;
  flight.promise = (async () => {
    try {
      const conversations = await contactApi(context, 'conversations');
      if (!current(context) || !Array.isArray(conversations)) return;
      const count = conversations.reduce((total, c) => total + (c.unread_count || 0), 0);
      $('contacts-badge').textContent = count ? String(count) : '';
      if (directoryState) { directoryState.conversations = conversations; renderContactDirectory(); }
    } catch { /* Independent reads must not prevent private chat recovery. */ }
    await refreshPeerMessages();
  })().finally(() => { if (peerIndexFlight === flight) peerIndexFlight = null; });
  return flight.promise;
}
async function sendPeerMessage(event) {
  event?.preventDefault();
  const state = peerState; if (!state || !current(state.context)) return;
  const draft = state.draft;
  if (draft.pending && !draft.pending.error) return;
  const body = draft.pending?.body || $('peer-message-input').value.trim();
  if (!body) return;
  if (Array.from(body).length > 4000) { $('peer-send-status').textContent = '消息过长，请分成几条发送。'; return; }
  if (draft.requestValue !== body) { draft.requestId = crypto.randomUUID(); draft.requestValue = body; }
  draft.pending = { body, error: false }; draft.value = ''; $('peer-message-input').value = '';
  $('peer-send-status').textContent = ''; renderPeerMessages();
  try {
    const receipt = await contactApi(state.context, 'contacts/' + encodeURIComponent(state.id) + '/messages', 'POST', { body, request_id: draft.requestId });
    if (!receipt?.message || receipt.message.sender !== state.context.user || receipt.message.body !== body || receipt.message.recipient !== state.id) throw Error('发送回执无效，请核对后重试');
    if (!current(state.context)) return;
    draft.pending = null; draft.requestId = null; draft.requestValue = null;
    draft.confirmed = [...draft.confirmed, receipt.message, ...(receipt.reply ? [receipt.reply] : [])].slice(-100);
    if (peerState?.draft !== draft) return;
    const merged = new Map([...peerState.messages, ...draft.confirmed].map(message => [message.id, message]));
    peerState.messages = [...merged.values()].sort((a, b) => a.id - b.id).slice(-100);
    peerState.loading = false; renderPeerMessages(); await refreshPeerMessages();
    if (peerState?.draft === draft) $('peer-send-status').textContent = '已发送，尚不代表对方本人已读或接手。';
  } catch (error) {
    if (!current(state.context)) return;
    draft.pending.error = true;
    draft.pending.rejected = [400, 403, 404, 413, 422].includes(error.httpStatus);
    if (peerState?.draft === draft) { renderPeerMessages(); $('peer-send-status').textContent = error.message; }
  }
}
$('peer-form').onsubmit = sendPeerMessage;
$('peer-message-input').oninput = () => { if (peerState) peerState.draft.value = $('peer-message-input').value; };
$('peer-message-input').onkeydown = event => {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229) { event.preventDefault(); void sendPeerMessage(); }
};
$('peer-back').onclick = openContactDirectory;
$('peer-details').onclick = () => showPeerDetails();
async function showPeerDetails() {
  const state = peerState; if (!state || !current(state.context)) return;
  closeContacts(); profileUI.reset(); hideCollaborationSettings();
  const context = state.context, id = state.id, version = contactsGeneration;
  const valid = () => peerState === state && current(context) && version === contactsGeneration;
  const host = $('contacts-view'); host.classList.remove('hide'); $('room').classList.add('contacts-open');
  const header = el('header', 'profile-header');
  const close = el('button', 'text-btn', '关闭资料'); close.type = 'button'; close.onclick = () => { closeContacts(); $('peer-details').focus?.(); };
  header.append(el('h2', null, peerName(id)), close);
  const body = el('div', 'contacts-body', '正在读取协作资料…'); host.replaceChildren(header, body);
  if (globalThis.innerWidth <= 900) close.focus?.();
  try {
    const profile = await contactApi(context, 'contacts/' + encodeURIComponent(id) + '/profile'); if (!valid()) return;
    body.replaceChildren(el('p', 'note', '对方公开的协作资料'));
    const details = el('dl', 'contact-public-profile');
    for (const [field, label] of [['team','团队'],['responsibilities','负责的工作'],['help_with','可以找我帮忙'],['request_requirements','发请求时请提供']]) {
      details.append(el('dt', null, label), el('dd', null, profile.public?.[field] || '尚未填写'));
    }
    body.append(details);
    const actions = el('div', 'actions');
    const chat = el('button', null, '发消息'); chat.type = 'button'; chat.onclick = () => { closeContacts(); $('peer-message-input').focus?.(); };
    const delegate = el('button', 'primary', '请他帮忙'); delegate.type = 'button';
    const editor = el('div', 'contact-editor');
    delegate.onclick = () => renderContactProposal(context, id, editor, valid);
    actions.append(chat, delegate); body.append(actions, editor);
  } catch (error) { if (valid()) { body.replaceChildren(el('p', 'note', '无法读取资料：' + error.message)); const retry = el('button', null, '重新读取'); retry.type = 'button'; retry.onclick = showPeerDetails; body.append(retry); } }
}
function renderContactProposal(context, id, editor, valid) {
  const saved = contactDrafts.get(id) || { value: '', requestId: crypto.randomUUID(), requestValue: null };
  contactDrafts.set(id, saved);
  editor.replaceChildren(el('h3', null, '请 ' + peerName(id) + ' 帮忙'));
  const summary = el('textarea'); summary.rows = 5; summary.setAttribute('aria-label', '委派摘要');
  summary.placeholder = '希望完成什么？需要哪些材料，什么时候给结果？'; summary.value = saved.value;
  summary.oninput = () => { if (valid()) saved.value = summary.value; };
  const note = el('p', 'note', '写清目标和完成标准。下一步可以检查共享内容，确认后才会发给对方。');
  const create = el('button', 'primary', '生成委派提案'); create.type = 'button';
  create.onclick = async () => {
    if (!valid() || create.disabled) return;
    const text = summary.value.trim();
    if (!text || new TextEncoder().encode(text).length > 4000) { note.textContent = '请填写任务摘要；内容过长时，请删减后再试。'; return; }
    if (saved.requestValue !== text) { saved.requestValue = text; saved.requestId = crypto.randomUUID(); }
    create.disabled = true;
    try {
      await contactApi(context, 'proposals', 'POST', { owner: id, summary: text, request_id: saved.requestId });
      if (current(context) && contactDrafts.get(id) === saved && saved.value.trim() === text) contactDrafts.delete(id);
      if (!valid()) return;
      restorePrivateChat(); status('提案已准备好，请检查共享内容并确认发送。'); await refresh(context);
    } catch (error) { if (valid()) note.textContent = error.message + '。摘要已保留，可以重试。'; }
    finally { if (valid()) create.disabled = false; }
  };
  editor.append(summary, note, create); summary.focus?.();
}
