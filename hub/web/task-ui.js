/* Human-reviewed task and proposal editing. */
function openActionEditor(context, kind, item, action, origin) {
  const key = kind + ':' + item.id;
  const sharedDetails = () => detailDrafts.get(key) ?? origin.detailsBox?.value ?? '';
  const existing = actionEditors.get(key);
  if (existing) { existing.panel.scrollIntoView?.({ block: 'nearest' }); return; }
  const panel = el('section', 'action-editor');
  const note = el('p', 'note', action === 'cancel'
    ? '取消原因会发给双方。已有交付记录保留；已发出的模型请求可能继续运行并产生费用。'
    : action === 'edit' ? '由你手动修改共享摘要和接收人。预览并确认后才会发送给对方。' : '请审阅交付内容。预览并确认后，对方可见。');
  const label = el('label', null, action === 'cancel' ? '取消原因（1–500 字）' : action === 'edit' ? '共享摘要' : '人工交付内容');
  const input = el('textarea'); input.rows = 6;
  input.setAttribute('aria-label', action === 'cancel' ? '取消原因' : action === 'edit' ? '共享摘要' : '人工交付内容');
  input.value = action === 'deliver' ? draftUI.reviewedText(item) : action === 'edit' ? item.summary : '';
  const sourceDraftId = action === 'deliver' ? draftUI.reviewedSource?.(item) : null;
  let owner;
  if (action === 'edit') {
    const ownerLabel = el('label', null, '接收人'); owner = el('input'); owner.value = item.owner; owner.setAttribute('aria-label', '接收人');
    ownerLabel.append(owner); panel.append(ownerLabel);
  }
  label.append(input);
  const preview = el('div', 'action-preview hide'), controls = el('div', 'actions');
  const review = el('button', null, '预览'), submit = el('button', 'primary', action === 'cancel' ? '确认取消任务' : action === 'edit' ? '确认修改并委派' : '确认交付');
  const close = el('button', null, '关闭编辑框');
  for (const button of [review, submit, close]) button.type = 'button';
  submit.disabled = true;
  let reviewed = null;
  const value = () => JSON.stringify([input.value, owner?.value, sharedDetails()]);
  input.oninput = () => { reviewed = null; submit.disabled = true; preview.classList.add('hide'); };
  if (owner) owner.oninput = input.oninput;
  review.onclick = () => {
    const text = input.value.trim();
    if (!text || (action === 'cancel' && Array.from(text).length > 500)) { note.textContent = '请填写有效内容' + (action === 'cancel' ? '（1–500 字）。' : '。'); return; }
    if (owner && !/^[A-Za-z0-9_-]+$/.test(owner.value.trim())) { note.textContent = '请填写有效接收人。'; return; }
    preview.replaceChildren(el('p', null, owner ? '接收人：' + owner.value.trim() : '确认后将共享以下内容：'), renderMarkdown(text));
    if (sharedDetails().trim()) preview.append(el('p', null, '附带共享材料：'), el('pre', null, sharedDetails()));
    preview.classList.remove('hide'); reviewed = value(); submit.disabled = false;
  };
  close.onclick = () => {
    if (busy.has(key)) return;
    actionEditors.delete(key); panel.remove?.(); origin.querySelector?.('button')?.focus();
  };
  panel.onkeydown = event => { if (event.key === 'Escape') { event.stopPropagation(); close.onclick(); } };
  submit.onclick = async () => {
    if (!current(context) || busy.has(key) || submit.disabled) return;
    if (actionEditors.get(key)?.stale) { submit.disabled = true; note.textContent = '内容已变更，请关闭编辑框后重新核对。'; return; }
    if (reviewed !== value()) { submit.disabled = true; note.textContent = '内容已修改，请重新预览。'; return; }
    const details = sharedDetails();
    if (details && new TextEncoder().encode(details).length > MAX_DETAILS_BYTES) { note.textContent = '共享材料最多 256 KiB。'; return; }
    busy.add(key); submit.disabled = true; review.disabled = true; close.disabled = true;
    try {
      await api(context, kind + '/' + encodeURIComponent(item.id) + '/actions', 'POST', {
        action: action === 'edit' ? 'confirm' : action, expected_revision: item.revision,
        ...(action === 'deliver' ? { result: input.value, ...(sourceDraftId ? { source_draft_id: sourceDraftId } : {}) } : action === 'cancel' ? { reason: input.value.trim() } : { owner: owner.value.trim(), summary: input.value.trim(), ...(details?.trim() ? { details } : {}) })
      });
      if (!current(context)) return;
      actionEditors.delete(key); panel.remove?.(); status(action === 'edit' ? '已确认共享摘要并创建任务。' : '已更新。', { transient: true });
    } catch (error) { if (current(context)) { note.textContent = error.message + '。输入已保留，请刷新核对状态后再操作。'; reviewed = null; } }
    finally {
      if (current(context)) { busy.delete(key); review.disabled = false; close.disabled = false; await refresh(context).catch(error => status(error.message)); }
    }
  };
  controls.append(review, submit, close); panel.append(note, label, preview, controls);
  actionEditors.set(key, { panel, submit, note, revision: item.revision });
  const card = origin.closest?.('article');
  (card?.querySelector?.('.card-panels') || card || origin).append(panel);
  const more = origin.closest?.('details.more'); if (more) more.open = false;
  input.focus?.();
}


function attachMaterialFile(label, box, context, key) {
  const picker = el('input'); picker.type = 'file'; picker.accept = '.txt,.md,.markdown,.pdf,text/plain,text/markdown,application/pdf';
  picker.setAttribute('aria-label', '选择文本、Markdown 或 PDF 文件');
  const note = el('p', 'note', '文本或 Markdown：UTF-8，最多 256 KiB；PDF：最多 2 MiB，会发送到当前 Hub 提取文字。预览正文并确认委派后才会共享给接收人。');
  const preview = el('details', 'material-preview hide'); preview.append(el('summary', null, '文件正文预览'));
  const content = el('pre'); preview.append(content);
  picker.onchange = async () => {
    const file = picker.files?.[0]; if (!file || !current(context)) return;
    picker.disabled = true;
    try {
      if (!/\.(txt|md|markdown|pdf)$/i.test(file.name)) throw Error('请选择文本、Markdown 或 PDF 文件。');
      const pdf = /\.pdf$/i.test(file.name), limit = pdf ? 2 * 1024 * 1024 : MAX_DETAILS_BYTES;
      if (file.size > limit) throw Error(pdf ? 'PDF 超过 2 MiB。' : '文件超过 256 KiB，请缩小后重试。');
      if (pdf && !confirm('将此 PDF 发送到当前 Hub 提取文字，不会共享给接收人。是否继续？')) return;
      const bytes = await file.arrayBuffer();
      if (!current(context)) return;
      if (bytes.byteLength > limit) throw Error('文件超过大小限制。');
      let text;
      if (pdf) {
        const data = new Uint8Array(bytes); let binary = '';
        for (let at = 0; at < data.length; at += 8192) binary += String.fromCharCode(...data.subarray(at, at + 8192));
        const extracted = await api(context, 'materials/extract', 'POST', { pdf_base64: btoa(binary) });
        if (!current(context)) return;
        if (typeof extracted.text !== 'string' || new TextEncoder().encode(extracted.text).length > MAX_DETAILS_BYTES) throw Error('提取正文无效或超过 256 KiB。');
        text = extracted.text;
      } else text = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
      if (!text.trim() || text.includes('\0')) throw Error('文件为空或包含二进制内容，请使用 UTF-8 文本。');
      if (box.value.trim() && !confirm('导入文件将替换当前共享材料，是否继续？')) return;
      box.value = text; detailDrafts.set(key, text); content.textContent = text; preview.classList.remove('hide');
      note.textContent = `已读取 ${file.name}（${file.size} 字节）。可修改下方正文；确认委派前不会发送。`;
      box.focus?.();
    } catch (error) { if (current(context)) note.textContent = '未导入：' + error.message; }
    finally { if (current(context)) { picker.disabled = false; picker.value = ''; } }
  };
  label.append(picker, note, preview);
}

let taskFilter = 'active';
function applyTaskFilter() {
  const context = session();
  let count = 0;
  for (const card of $('tasks').children) {
    const task = visibleTasks.get(card.taskId);
    if (!task) continue;
    const finished = ['completed', 'canceled'].includes(task.status);
    const show = taskFilter === 'completed' ? finished : !finished && (taskFilter === 'mine' ? task.requester === context.user
      : taskFilter === 'waiting' ? (task.owner === context.user && ['awaiting_owner', 'working'].includes(task.status))
        || (task.requester === context.user && task.status === 'ready_for_acceptance') : true);
    card.classList[show ? 'remove' : 'add']('hide'); if (show) count++;
  }
  $('task-filter-empty').textContent = count ? '' : taskFilter === 'completed' ? '还没有已完成或已取消的任务。' : '这个筛选下暂时没有任务。';
}

// Index navigation changes only the view, never task state or unsent text.
function revealTask(taskId) {
  const task = visibleTasks.get(taskId);
  const card = [...$('tasks').children].find(card => card.taskId === taskId);
  if (!task || !card) return;
  if (card.classList.contains('hide')) {
    taskFilter = ['completed', 'canceled'].includes(task.status) ? 'completed' : 'active';
    $('task-filter').value = taskFilter;
    applyTaskFilter();
  }
  card.tabIndex = -1; card.focus?.(); flash(card);
}

function renderTaskCards(context, tasks) {
  const taskCards = [];
  const slots = new Map();
  for (const task of tasks) {
    const element = el('article', 'card task task-' + task.status);
    element.taskId = task.id;
    const head = el('header', 'card-head');
    const mine = task.owner === context.user ? '我负责' : task.requester === context.user ? '我发起' : '';
    head.append(badge(TASK_STATUS, task.status));
    if (mine) head.append(el('span', 'badge badge-mine', mine));
    const id = el('span', 'card-id', '#' + String(task.id).slice(0, 8));
    id.title = '任务编号：' + task.id + ' · 版本：' + task.revision;
    head.append(id);
    element.append(head, el('h3', 'task-title', task.summary),
      el('div', 'meta', `${task.requester} → ${task.owner}`));
    if (task.details) {
      const material = el('details', 'material');
      material.append(el('summary', null, '共享材料'), el('pre', null, task.details));
      element.append(material);
    }
    if (task.result) {
      // Finished tasks keep the delivery one click away instead of filling the list.
      const finished = ['completed', 'canceled'].includes(task.status);
      const delivery = el(finished ? 'details' : 'div', 'delivery');
      delivery.append(el(finished ? 'summary' : 'span', 'delivery-label', '最新交付'), el('pre', null, task.result));
      element.append(delivery);
    }
    const draftNote = draftUI.status?.(task);
    if (draftNote) element.append(el('p', 'draft-status' + (task.my_draft?.status === 'pending' ? ' working' : ''), draftNote));
    // Overview only: accepting, drafting, delivering and accepting results happen
    // in the conversation, where the matching card sits at the time of its last step.
    const waitingOnMe = (task.owner === context.user && ['awaiting_owner', 'working'].includes(task.status))
      || (task.requester === context.user && task.status === 'ready_for_acceptance');
    const canCancel = (task.requester === context.user && ['awaiting_owner', 'working', 'ready_for_acceptance'].includes(task.status))
      || (task.owner === context.user && ['awaiting_owner', 'working'].includes(task.status));
    const foot = el('div', 'card-foot');
    const actionRow = el('div', 'actions');
    const tools = el('div', 'tools');
    const more = el('details', 'more');
    const menu = el('div', 'more-menu');
    more.append(el('summary', null, '更多'), menu);
    foot.append(actionRow, tools);
    const open = el('button', waitingOnMe ? 'primary' : null, waitingOnMe ? '在对话中处理' : '在对话中查看');
    open.type = 'button';
    open.onclick = () => showTaskInChat(task.id);
    actionRow.append(open);
    addDiscussion(tools, context, task);
    draftUI.attach(menu, context, task);
    timelineUI.attach(menu, context, task);
    workerUI.attach(menu, context, task);
    if (canCancel) addActions(menu, context, 'tasks', task, [['cancel', '取消任务']]);
    if (menu.children.length) tools.append(more);
    const slot = el('div', 'card-panels');
    slots.set(task.id, slot);
    element.append(foot, slot);
    const editor = actionEditors.get('tasks:' + task.id);
    if (editor) {
      slot.append(editor.panel);
      if (editor.revision !== task.revision) { editor.stale = true; editor.submit.disabled = true; editor.note.textContent = '任务已变更，请关闭编辑框后重新核对。输入仍保留。'; }
    }
    taskCards.push(element);
  }
  return { taskCards, slots };
}

function addActions(element, context, kind, item, actions) {
  const key = kind + ':' + item.id;
  const buttons = actions.map(([action, label]) => {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = label;
    if (ACTION_STYLE[action]) button.className = ACTION_STYLE[action];
    button.disabled = busy.has(key);
    button.onclick = async () => {
      if (!current(context) || busy.has(key)) return;
      if (['deliver', 'cancel', 'edit'].includes(action)) {
        openActionEditor(context, kind, item, action, element);
        return;
      }
      let result, reason, details;
      if (kind === 'proposals' && action === 'confirm') {
        const entered = element.detailsBox?.value ?? '';
        if (entered.trim()) {
          if (new TextEncoder().encode(entered).length > MAX_DETAILS_BYTES) {
            status('共享材料最多 256 KiB。尚未确认。'); return;
          }
          details = entered;
        }
      }
      busy.add(key);
      buttons.forEach(control => { control.disabled = true; });
      status('');
      try {
        await api(context, kind + '/' + encodeURIComponent(item.id) + '/actions', 'POST', {
          action, result, ...(reason === undefined ? {} : { reason }),
          ...(details === undefined ? {} : { details }), expected_revision: item.revision
        });
        if (!current(context)) return;
        status(kind === 'proposals'
          ? (action === 'confirm' ? (details ? '已确认共享摘要和共享材料，并创建任务。' : '已确认共享摘要并创建任务。') : '已拒绝提案，未创建任务。')
          : '已更新。', { transient: true });
      } catch (error) {
        if (current(context)) status(error.message + '。请刷新核对服务端状态后再操作。');
      } finally {
        if (current(context)) {
          busy.delete(key);
          buttons.forEach(control => { control.disabled = false; });
          await refresh(context).catch(error => { if (current(context)) status(error.message); });
        }
      }
    };
    element.append(button);
    return button;
  });
}

