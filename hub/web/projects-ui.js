/* Project coordination stays beside private chat. Nothing reads or publishes chat history. */
let projectView = null, projectGeneration = 0, projectRead = null, projectDraft = null, projectBusy = false;
let projectDirectory = [], projectInvitations = [];
const projectPath = id => 'projects/' + encodeURIComponent(id);
const projectHost = () => $('project-panel');
function projectButton(text, action, className = '') {
  const button = el('button', className, text); button.type = 'button'; button.onclick = action; return button;
}
function projectField(form, label, type = 'text', value = '', options) {
  const wrap = el('label', 'project-field', label);
  const input = el(type === 'textarea' ? 'textarea' : options ? 'select' : 'input');
  if (options) for (const [key, name] of options) { const option = el('option', '', name); option.value = key; input.append(option); }
  else if (type !== 'textarea') input.type = type;
  // Explicit naming prevents a wrapped select's option text from entering its label.
  input.setAttribute('aria-label', label);
  input.value = value; input.required = true;
  if (type === 'textarea') input.rows = 4;
  wrap.append(input); form.append(wrap); return input;
}
function projectNotice(message) { const notice = $('project-notice'); if (notice) notice.textContent = message; }
function closeProjects() {
  $('room').classList.remove('projects-open'); projectHost().classList.add('hide');
  $('open-projects').classList.remove('active'); $('open-projects').setAttribute('aria-expanded', 'false');
}
function resetProjects() {
  projectGeneration++; projectView = null; projectRead = null; projectDraft = null; projectBusy = false;
  projectDirectory = []; projectInvitations = []; closeProjects(); projectHost().replaceChildren();
}
function projectShell() {
  const head = el('header', 'project-heading');
  head.append(el('h2', '', '项目'), projectButton('回到私聊', () => { closeProjects(); $('text').focus(); }));
  const notice = el('p', 'project-notice note'); notice.id = 'project-notice'; notice.setAttribute('role', 'status');
  projectHost().replaceChildren(head, el('p', 'note', '与助手讨论仍仅自己可见。只有预览并确认发布的内容会成为项目材料。'), notice);
}
async function openProjects() {
  if (!current(session())) return;
  closeContacts(); closePeerConversation(); profileUI.reset(); hideCollaborationSettings(); setTasksOpen(false);
  $('room').classList.add('projects-open'); projectHost().classList.remove('hide');
  $('open-projects').classList.add('active'); $('open-projects').setAttribute('aria-expanded', 'true');
  if (projectDraft) { void refreshProjects(); return; }
  if (projectView) await loadProject(projectView.project.id); else await loadProjectDirectory();
}
function projectCanNavigate() {
  if (!projectDraft && !projectBusy) return true;
  projectNotice('请先完成或取消编辑。关闭面板会保留本页未发布内容，退出账号或刷新会清除。'); return false;
}
async function loadProjectDirectory() {
  if (!projectCanNavigate()) return;
  const context = session(), generation = ++projectGeneration; projectView = null; projectRead = null;
  projectShell(); projectNotice('正在读取项目…');
  try {
    const [projects, invitations] = await Promise.all([contactApi(context, 'projects'), contactApi(context, 'project-invitations')]);
    if (!current(context) || generation !== projectGeneration) return;
    projectDirectory = projects; projectInvitations = invitations; renderProjectDirectory();
  } catch (error) {
    if (!current(context) || generation !== projectGeneration) return;
    projectNotice('无法读取项目：' + error.message);
    projectHost().append(projectButton('重新读取项目', loadProjectDirectory));
  }
}
function renderProjectDirectory() {
  projectShell();
  projectHost().append(projectButton('创建项目', createProjectEditor, 'primary'));
  if (projectInvitations.length) projectHost().append(el('h3', '', '待接受的邀请'));
  for (const invitation of projectInvitations) {
    const card = el('article', 'project-card');
    card.append(el('h3', '', invitation.name), el('p', 'project-copy', invitation.goal), el('p', 'note', '邀请人：' + invitation.creator + ' · 加入后才可读取共同状态。'));
    card.append(projectButton('加入项目', () => membershipEditor(invitation, 'accept')), projectButton('婉拒邀请', () => membershipEditor(invitation, 'decline')));
    projectHost().append(card);
  }
  projectHost().append(el('h3', '', '我的项目'));
  if (!projectDirectory.length) projectHost().append(el('p', 'note', '还没有项目。可以围绕一次临时协作或一项持续工作创建。'));
  for (const project of projectDirectory) {
    const button = projectButton(project.name, () => loadProject(project.id), 'project-list-item');
    button.append(el('span', 'note', (project.kind === 'ongoing' ? '持续项目' : '临时项目') + (project.status === 'archived' ? ' · 已归档' : '')));
    projectHost().append(button);
  }
  projectHost().append(projectButton('刷新项目列表', loadProjectDirectory));
}
async function fetchProject(context, id) {
  // A concurrent write between these reads must never produce a mixed-version confirmation.
  for (let attempt = 0; attempt < 2; attempt++) {
    const detail = await contactApi(context, projectPath(id));
    const state = await contactApi(context, projectPath(id) + '/state');
    if (detail.project.revision === state.project_revision) return { ...detail, items: state.items };
  }
  throw Error('项目正在更新，请重新读取');
}
async function loadProject(id) {
  if (!projectCanNavigate()) return;
  const context = session(), generation = ++projectGeneration; projectRead = null; projectView = null;
  projectShell(); projectNotice('正在读取共同状态…');
  try {
    const view = await fetchProject(context, id);
    if (!current(context) || generation !== projectGeneration) return;
    projectView = view; renderProject();
  } catch (error) {
    if (!current(context) || generation !== projectGeneration) return;
    projectNotice('无法读取项目：' + error.message);
    projectHost().append(projectButton('重新读取项目', () => loadProject(id)), projectButton('项目列表', loadProjectDirectory));
  }
}
function projectCard(item, historical = false) {
  const card = el('article', 'project-state-card'); card.dataset.item = item.id;
  card.append(el('p', 'project-item-status', historical ? '历史版本 ' + item.version + ' · ' + (item.status === 'confirmed' ? '当时由 ' + item.confirmed_by + ' 确认' : ProjectStateModel.status(item)) : ProjectStateModel.status(item) + ' · 版本 ' + item.version));
  card.append(el('p', 'project-copy', item.content));
  if (item.assignee) card.append(el('p', 'note', '负责人：' + item.assignee));
  card.append(el('p', 'note', '来源类型：' + (item.source_kind === 'model_suggestion' ? '模型建议（不是本人承诺）' : '人工陈述')),
    el('p', 'project-copy note', '来源说明（发布人填写）：' + item.source_note),
    el('p', 'note', '发布人：' + item.created_by + ' · 本次变更：' + item.changed_by + ' · ' + item.created_at));
  if (!historical && item.status === 'proposed') card.append(el('p', 'note', '等待确认：' + ProjectStateModel.confirmer(projectView.project, item) + '。其他成员不能代替本人确认。'));
  if (historical) return card;
  const actions = el('div', 'actions');
  const permissions = ProjectStateModel.permissions(projectView.project, item, user);
  if (permissions.confirm) actions.append(projectButton(ProjectStateModel.confirmations[item.kind], () => stateActionEditor(item, 'confirm'), 'primary'));
  if (permissions.revise) actions.append(projectButton('修订', () => stateEditor(item)));
  if (permissions.withdraw) actions.append(projectButton('撤回', () => stateActionEditor(item, 'withdraw')));
  actions.append(projectButton('版本记录', () => stateHistory(item)));
  card.append(actions); return card;
}
function renderProject() {
  if (!projectView || projectDraft) return;
  projectShell(); const { project, members, items, tasks } = projectView;
  projectHost().append(projectButton('项目列表', loadProjectDirectory));
  projectHost().append(el('h3', 'project-title', project.name), el('p', 'note', (project.kind === 'ongoing' ? '持续项目' : '临时项目') + ' · ' + (project.status === 'archived' ? '已归档，只读' : '进行中') + ' · 项目版本 ' + project.revision));
  const description = el('details', 'project-description'); description.append(el('summary', '', '项目说明（创建人提供，不代表全员确认）'), el('p', 'project-copy', project.goal)); projectHost().append(description);
  projectHost().append(el('p', 'project-members note', '成员：' + members.filter(m => ['active', 'invited'].includes(m.status)).map(m => m.owner + (m.status === 'invited' ? '（待加入）' : '')).join('、')));
  const actions = el('div', 'actions');
  actions.append(projectButton('和助手讨论', discussProject));
  if (project.status === 'active') {
    actions.append(projectButton('发布共同状态', () => stateEditor(), 'primary'));
    if (project.creator === user) actions.append(projectButton('邀请成员', inviteProjectEditor));
  }
  actions.append(projectButton('刷新共同状态', () => loadProject(project.id))); projectHost().append(actions);
  for (const [kind, name] of Object.entries(ProjectStateModel.kinds)) {
    const section = el('section', 'project-state-group'); section.setAttribute('aria-label', name);
    section.append(el('h3', '', name));
    const group = items.filter(item => item.kind === kind && item.status !== 'withdrawn');
    if (!group.length) section.append(el('p', 'note', '尚未发布' + name + '。'));
    for (const item of group) section.append(projectCard(item));
    projectHost().append(section);
  }
  const withdrawn = items.filter(item => item.status === 'withdrawn');
  if (withdrawn.length) { const history = el('details'); history.append(el('summary', '', '已撤回（' + withdrawn.length + '）')); for (const item of withdrawn) history.append(projectCard(item)); projectHost().append(history); }
  if (tasks.length) {
    const links = el('section', 'project-state-group'); links.append(el('h3', '', '关联任务'), el('p', 'note', '这里只展示经任务双方同意公开的标签和进度。任务正文与私密草稿仍仅双方可见。'));
    for (const task of tasks) {
      const card = el('article', 'project-card');
      card.append(el('p', 'project-copy', task.label), el('p', 'note', task.status === 'pending' ? '等待另一方同意公开' : (TASK_STATUS[task.task_status]?.[0] || task.task_status)));
      if (task.can_open_task) card.append(projectButton('打开我的任务', () => { closeProjects(); showTaskInChat(task.task_id); }));
      links.append(card);
    }
    projectHost().append(links);
  }
}
function discussProject() {
  // This action prepares only the user's own input. It never reads private chat or sends.
  if ($('text').value.trim() || pendingSend || pendingTurn || selectedTask || selectedRepository) {
    projectNotice('请先处理当前输入或已选择的聊天上下文，再讨论项目。'); return;
  }
  $('text').value = '我想讨论项目「' + projectView.project.name + '」。请帮我整理下一步，任何共同状态都等我预览后再手动发布。';
  $('text').dispatchEvent(new Event('input')); closeProjects(); $('text').focus();
}
function beginProjectEditor(title) {
  if (!projectCanNavigate()) return null;
  const editor = el('section', 'project-editor'); editor.setAttribute('aria-label', title + '面板');
  editor.append(el('h3', '', title));
  const context = session();
  projectDraft = { editor, context, generation: projectGeneration, projectId: projectView?.project.id, revision: projectView?.project.revision, invalid: false };
  projectHost().append(editor); editor.scrollIntoView?.({ block: 'nearest' }); return editor;
}
function cancelProjectEditor() {
  if (projectBusy) return;
  projectDraft?.editor.remove(); projectDraft = null;
  if (projectView) void loadProject(projectView.project.id); else void loadProjectDirectory();
}
function editorCancel() { return projectButton('取消编辑', cancelProjectEditor); }
function projectPreview(form, title, lines, label, path, body) {
  const draft = projectDraft; if (!draft || draft.invalid || projectBusy) return;
  draft.idempotent = !!body.request_id;
  const preview = el('div', 'project-preview'); preview.append(el('h4', '', title));
  for (const text of lines) preview.append(el('p', 'project-copy', text));
  preview.append(el('p', 'note', '只提交以上内容，不附带私聊、私密草稿或其他来源正文。'));
  const confirm = projectButton(label, () => commitProject(draft, path, body, confirm), 'primary');
  preview.append(confirm);
  if (form.tagName === 'FORM') preview.append(projectButton('返回修改', () => {
    if (projectBusy || draft.uncertain) return;
    preview.replaceWith(form);
  }));
  preview.append(editorCancel());
  form.replaceWith(preview); confirm.focus();
}
async function commitProject(draft, path, body, button) {
  if (projectBusy || projectDraft !== draft || draft.invalid || !current(draft.context)) return;
  // Discard every read started before this write, including a poll that sees its commit
  // before the POST response arrives. Such a read cannot invalidate recovery.
  projectGeneration++;
  projectBusy = true; draft.uncertain = false; button.disabled = true;
  draft.editor.querySelectorAll('button').forEach(b => { b.disabled = true; }); projectNotice('正在提交…');
  let succeeded = false;
  try {
    const result = await contactApi(draft.context, path, 'POST', body); succeeded = true;
    if (!current(draft.context) || projectDraft !== draft) return;
    projectBusy = false; projectDraft = null; draft.editor.remove();
    const id = draft.projectId || (path === 'projects' ? result.id : null);
    if (id) await loadProject(id); else await loadProjectDirectory();
    projectNotice('操作已保存。发布不代表其他成员已确认。');
  } catch (error) {
    if (!current(draft.context) || projectDraft !== draft) return;
    if (succeeded) { projectNotice('操作已保存，读取失败。请刷新项目，不要重复提交。'); return; }
    projectBusy = false;
    draft.editor.querySelectorAll('button').forEach(b => { b.disabled = false; });
    if ([401, 403, 404].includes(error.httpStatus)) {
      projectDraft = null; projectView = null; projectGeneration++; projectShell(); projectNotice('无法访问项目，已清除此页内容。请重新读取项目列表。'); projectHost().append(projectButton('项目列表', loadProjectDirectory)); return;
    }
    if (error.httpStatus === 409) {
      draft.invalid = true; button.disabled = true; projectNotice('项目已更新，请取消编辑后重新核对。未自动重试或覆盖。'); return;
    }
    // Only creation has a request ID. Replay exactly that body, never a newly inferred intent.
    draft.uncertain = !error.httpStatus || error.httpStatus >= 500;
    if (draft.uncertain) {
      button.disabled = !body.request_id;
      if (body.request_id) button.textContent = '核对并重试原提交';
      projectNotice('提交结果未知。' + (body.request_id ? '可用同一编号重试以上原内容，不会重复创建。' : '请取消编辑并刷新核对，系统不会自动重发。'));
    } else projectNotice('未提交：' + error.message);
  } finally { if (current(draft.context) && (projectDraft === draft || !projectDraft)) projectBusy = false; }
}
function createProjectEditor() {
  const editor = beginProjectEditor('创建项目'); if (!editor) return;
  const form = el('form'); const name = projectField(form, '项目名称'); name.maxLength = 120;
  const goal = projectField(form, '项目说明', 'textarea'); goal.maxLength = 4000;
  const kind = projectField(form, '项目类型', 'text', 'temporary', [['temporary', '临时项目'], ['ongoing', '持续项目']]);
  form.append(el('p', 'note', '创建后最初只有你自己。邀请者会看到名称和说明，接受邀请才加入；说明不是已确认的共同目标。'));
  const preview = el('button', 'primary', '预览创建'); preview.type = 'submit'; form.append(preview, editorCancel());
  form.onsubmit = event => {
    event.preventDefault(); if (!form.reportValidity()) return;
    const body = { name: name.value.trim(), goal: goal.value.trim(), kind: kind.value, request_id: crypto.randomUUID() };
    projectPreview(form, '创建前核对', [body.name, body.goal, kind.selectedOptions[0].textContent], '确认创建项目', 'projects', body);
  };
  editor.append(form); name.focus();
}
function stateEditor(item) {
  const editor = beginProjectEditor(item ? '修订共同状态' : '发布共同状态'); if (!editor) return;
  const project = projectView.project; const form = el('form');
  const kind = projectField(form, '类型', 'text', item?.kind || 'goal', Object.entries(ProjectStateModel.kinds)); kind.disabled = !!item;
  const content = projectField(form, '内容', 'textarea', item?.content || ''); content.maxLength = 4000;
  const sourceKind = projectField(form, '来源类型', 'text', item?.source_kind || 'human_statement', [['human_statement', '人工陈述'], ['model_suggestion', '模型建议']]);
  const note = projectField(form, '来源说明', 'textarea', item?.source_note || ''); note.maxLength = 1000;
  const members = projectView.members.filter(member => member.status === 'active');
  const assignee = projectField(form, '负责人', 'text', item?.assignee || '', [['', '选择需本人确认的成员'], ...members.map(m => [m.owner, m.owner])]);
  const updateKind = () => { assignee.parentElement.classList.toggle('hide', kind.value !== 'assignment'); assignee.required = kind.value === 'assignment'; }; kind.onchange = updateKind; updateKind();
  form.append(el('p', 'note', '请只填准备向全体项目成员公开的内容和来源说明。模型建议保持建议标记；分工需要负责人本人确认。修订会清除原有确认。'));
  const preview = el('button', 'primary', '预览发布'); preview.type = 'submit'; form.append(preview, editorCancel());
  form.onsubmit = event => {
    event.preventDefault(); if (!form.reportValidity()) return;
    const value = { kind: kind.value, content: content.value.trim(), source_kind: sourceKind.value, source_note: note.value.trim(), assignee: kind.value === 'assignment' ? assignee.value : null };
    const body = item ? { action: 'revise', expected_revision: project.revision, expected_version: item.version, content: value } : { request_id: crypto.randomUUID(), expected_revision: project.revision, content: value };
    const path = projectPath(project.id) + '/state' + (item ? '/' + encodeURIComponent(item.id) + '/actions' : '');
    const lines = ['项目：' + project.name, '可见范围：所有已加入的项目成员（包括之后加入的成员）', ProjectStateModel.kinds[value.kind] + '：' + value.content, '来源类型：' + sourceKind.selectedOptions[0].textContent, '来源说明：' + value.source_note, ...(value.assignee ? ['负责人：' + value.assignee] : []), '发布后状态：待本人确认' + (item ? '。原确认作废，生成新版本。' : '')];
    projectPreview(form, '发布前核对', lines, '确认发布给项目成员', path, body);
  };
  editor.append(form); content.focus();
}
function stateActionEditor(item, action) {
  const editor = beginProjectEditor(action === 'confirm' ? '核对当前版本' : '撤回共同状态'); if (!editor) return;
  const project = projectView.project, form = el('div'); editor.append(form);
  projectPreview(form, action === 'confirm' ? '由 ' + user + ' 确认版本 ' + item.version : '撤回版本 ' + item.version,
    [ProjectStateModel.kinds[item.kind] + '：' + item.content, ...(item.assignee ? ['负责人：' + item.assignee] : []), '来源类型：' + (item.source_kind === 'model_suggestion' ? '模型建议' : '人工陈述'), '来源说明：' + item.source_note, action === 'confirm' ? '仅确认这一版内容，不代表全体成员达成一致，也不会接手或交付关联任务。' : '撤回后保留发布和确认历史，不再作为当前共同状态。'],
    action === 'confirm' ? '确认当前版本' : '确认撤回', projectPath(project.id) + '/state/' + encodeURIComponent(item.id) + '/actions',
    { action, expected_revision: project.revision, expected_version: item.version });
}
async function stateHistory(item) {
  const editor = beginProjectEditor('版本记录'); if (!editor) return;
  const draft = projectDraft; editor.append(el('p', 'note', '历史确认只说明当时的表态，不代表当前承诺。'), editorCancel());
  try {
    const result = await contactApi(draft.context, projectPath(draft.projectId) + '/state/' + encodeURIComponent(item.id));
    if (!current(draft.context) || projectDraft !== draft) return;
    for (const version of result.versions) editor.append(projectCard(version, true));
  } catch (error) { if (current(draft.context) && projectDraft === draft) projectNotice('无法读取版本记录：' + error.message); }
}
function membershipEditor(project, action) {
  const editor = beginProjectEditor(action === 'accept' ? '接受项目邀请' : '婉拒项目邀请'); if (!editor) return;
  const form = el('div'); editor.append(form);
  projectPreview(form, project.name, [project.goal, action === 'accept' ? '加入后，成员可见你的账号和你明确发布的项目状态。私聊、原有任务正文与草稿不会共享。' : '不加入这个项目。'],
    action === 'accept' ? '确认加入项目' : '确认婉拒', projectPath(project.id) + '/membership', { action, expected_revision: project.revision });
}
async function inviteProjectEditor() {
  const editor = beginProjectEditor('邀请成员'); if (!editor) return;
  const draft = projectDraft, project = projectView.project;
  editor.append(el('p', 'note', '邀请会向所选成员显示项目名称与说明。对方接受后才能读取共同状态；私聊和原有任务不因此公开。'), editorCancel());
  try {
    const contacts = await contactApi(draft.context, 'contacts');
    if (!current(draft.context) || projectDraft !== draft) return;
    const existing = new Set(projectView.members.filter(m => ['active', 'invited'].includes(m.status)).map(m => m.owner));
    const available = contacts.filter(id => !existing.has(id));
    if (!available.length) { editor.append(el('p', 'note', '没有可邀请的新成员。')); return; }
    const form = el('form'); const owner = projectField(form, '邀请成员', 'text', '', [['', '选择成员'], ...available.map(id => [id, id])]);
    const preview = el('button', 'primary', '预览邀请'); preview.type = 'submit'; form.append(preview);
    form.onsubmit = event => { event.preventDefault(); if (!form.reportValidity()) return;
      projectPreview(form, '核对邀请', ['项目：' + project.name, '项目说明：' + project.goal, '接收人：' + owner.value], '确认邀请', projectPath(project.id) + '/members', { owner: owner.value, expected_revision: project.revision }); };
    editor.append(form); owner.focus();
  } catch (error) { if (current(draft.context) && projectDraft === draft) projectNotice('无法读取可邀请成员：' + error.message); }
}
async function refreshProjects() {
  if (!$('room').classList.contains('projects-open') || !projectView || projectBusy || projectRead) return;
  const context = session(), generation = projectGeneration, id = projectView.project.id;
  const read = {}; projectRead = read;
  try {
    const view = await fetchProject(context, id);
    if (!current(context) || generation !== projectGeneration) return;
    if (projectDraft) {
      if (!(projectDraft.uncertain && projectDraft.idempotent) && (view.project.revision !== projectDraft.revision || JSON.stringify(view.members) !== JSON.stringify(projectView.members))) {
        projectDraft.invalid = true;
        projectDraft.editor.querySelectorAll('button.primary').forEach(button => { button.disabled = true; });
        projectNotice('项目已更新，请取消编辑后重新核对。未自动重试或覆盖。');
      }
    } else if (JSON.stringify(view) !== JSON.stringify(projectView)) { projectView = view; renderProject(); }
  } catch (error) {
    if (!current(context) || generation !== projectGeneration) return;
    if ([401, 403, 404].includes(error.httpStatus)) {
      projectDraft = null; projectView = null; projectGeneration++; projectShell(); projectHost().append(projectButton('项目列表', loadProjectDirectory));
      projectNotice('项目访问权限已失效，已清除此页内容。');
    } else projectNotice('共同状态暂未同步：' + error.message);
  } finally { if (projectRead === read) projectRead = null; }
}
$('open-projects').onclick = openProjects;
