/* Explicit, owner-private immutable repository context for one ordinary chat. */
let selectedRepository = null, repositoryGeneration = 0, repositoryRead = null;
const repositoryLocked = () => !!pendingTurn || busy.has('send') || busy.has('turn-check');
function closeRepositoryPicker() {
  repositoryGeneration++;
  repositoryRead?.abort(); repositoryRead = null;
  $('repository-picker').classList.add('hide');
  $('repository-options').replaceChildren();
}
function resetRepositoryUI() {
  selectedRepository = null; closeRepositoryPicker(); renderRepositorySelection();
}
function renderRepositorySelection() {
  const selection = pendingTurn?.selectedRepository || selectedRepository;
  $('repository-context').classList[selection ? 'remove' : 'add']('hide');
  $('repository-context-text').textContent = selection
    ? `${selection.repository} · ${selection.commit.slice(0, 12)} · ${selection.path || '搜索：' + selection.query}`
      + (pendingTurn ? ' · 本次发送已锁定' : ' · 仅附给下一条普通聊天') : '';
  $('clear-repository').disabled = repositoryLocked();
  $('choose-repository').disabled = repositoryLocked();
}
function repositoryChatMessage(text, selection) {
  if (!selection) return text;
  if (/^\/repo(?:-search)?\s/.test(text)) throw Error('已有仓库选择，请清除选择后再使用仓库命令，或直接输入问题。');
  return `${selection.path ? '/repo' : '/repo-search'} ${selection.repository} ${selection.commit} ${selection.path || selection.query}\n${text}`;
}
function repositoryReceiptMatches(expected, actual) {
  return !expected || (actual && actual.repository === expected.repository && actual.commit === expected.commit
    && actual.snapshot_digest === expected.snapshot_digest && actual.path === expected.path && actual.query === expected.query && actual.read_only === true);
}
async function openRepositoryPicker() {
  const context = session(); if (!current(context) || repositoryLocked()) return;
  setMenuOpen(false);
  closeRepositoryPicker(); const generation = repositoryGeneration;
  const valid = () => current(context) && generation === repositoryGeneration && !repositoryLocked();
  $('repository-picker').classList.remove('hide');
  const container = $('repository-options'); container.replaceChildren(el('p', 'note', '正在读取你可访问的仓库…'));
  async function read(path, method, body) {
    repositoryRead?.abort(); const controller = new AbortController(); repositoryRead = controller;
    const stop = () => controller.abort(); context.signal.addEventListener('abort', stop, { once: true });
    const timeout = setTimeout(stop, READ_TIMEOUT);
    try { return await api({ ...context, signal: controller.signal }, path, method, body); }
    finally { clearTimeout(timeout); context.signal.removeEventListener('abort', stop); if (repositoryRead === controller) repositoryRead = null; }
  }
  try {
    const repositories = await read('repositories'); if (!valid()) return;
    if (!Array.isArray(repositories) || repositories.some(repo => !/^[A-Za-z0-9_-]{1,64}$/.test(repo.id) || typeof repo.label !== 'string')) throw Error('仓库列表无效');
    if (!repositories.length) { container.replaceChildren(el('p', 'note', '还没有可用仓库。请管理员配置你的只读仓库权限并完成边界验证。')); return; }
    const repository = el('select'); repository.setAttribute('aria-label', '只读仓库');
    for (const repo of repositories) { const option = el('option', null, repo.label); option.value = repo.id; repository.append(option); }
    repository.value = repositories[0].id;
    const commit = el('input'); commit.placeholder = '完整提交 ID（40 或 64 位）'; commit.setAttribute('aria-label', '仓库提交 ID');
    const mode = el('select'); mode.setAttribute('aria-label', '仓库读取方式');
    for (const [value, text] of [['path', '读取单个文件'], ['query', '搜索文字（非正则）']]) { const option = el('option', null, text); option.value = value; mode.append(option); } mode.value = 'path';
    const target = el('input'); target.setAttribute('aria-label', '仓库文件路径或搜索文字'); target.placeholder = '例如 README.md';
    const note = el('p', 'note', '选择固定提交和一个文件或搜索词。预览后需明确确认，才会随下一条普通聊天发送给你的模型；不会附给委派提案。');
    const preview = el('pre', 'repository-preview hide');
    const load = el('button', null, '预览仓库内容'), confirmSelection = el('button', 'primary', '确认用于下一条聊天');
    load.type = confirmSelection.type = 'button'; confirmSelection.disabled = true;
    let captured = null, reading = false;
    const selection = () => ({ repository: repository.value, commit: commit.value.trim(), [mode.value]: target.value.trim() });
    const signature = () => JSON.stringify(selection());
    const invalidate = () => { captured = null; confirmSelection.disabled = true; preview.classList.add('hide'); };
    for (const field of [repository, commit, mode, target]) { field.oninput = invalidate; field.onchange = invalidate; }
    load.onclick = async () => {
      if (!valid() || reading) return;
      const choice = selection(), original = signature();
      if (!repositories.some(repo => repo.id === choice.repository) || !/^(?:[a-f0-9]{40}|[a-f0-9]{64})$/.test(choice.commit)
          || !target.value.trim() || /[\r\n]/.test(target.value) || new TextEncoder().encode(target.value.trim()).length > (mode.value === 'path' ? 1024 : 200)) {
        note.textContent = '请填写完整提交 ID，以及有效的单行文件路径或搜索文字。'; return;
      }
      reading = true; load.disabled = true; invalidate();
      try {
        const result = await read('repositories/context', 'POST', choice); if (!valid() || original !== signature()) return;
        if (result.repository !== choice.repository || result.commit !== choice.commit || result.read_only !== true
            || typeof result.text !== 'string' || typeof result.snapshot_digest !== 'string'
            || !/^[a-f0-9]{64}$/.test(result.snapshot_digest)
            || (result.path ?? null) !== (choice.path ?? null) || (result.query ?? null) !== (choice.query ?? null)) throw Error('仓库预览与选择不一致');
        captured = { repository: result.repository, commit: result.commit, path: result.path ?? null,
          query: result.query ?? null, snapshot_digest: result.snapshot_digest };
        preview.textContent = result.text; preview.classList.remove('hide'); confirmSelection.disabled = false;
        note.textContent = (result.truncated ? '预览仅包含部分内容。' : '请核对预览。')
          + (Number.isSafeInteger(result.skipped_files) && result.skipped_files > 0 ? `已跳过 ${result.skipped_files} 个不支持的文件。` : '')
          + '确认后只附给下一条普通聊天，不执行代码、不共享给其他用户。';
      } catch (error) { if (valid()) note.textContent = '无法读取仓库：' + error.message + '。原有选择未改变。'; }
      finally { if (valid()) { reading = false; load.disabled = false; } }
    };
    confirmSelection.onclick = () => {
      if (!valid() || !captured || confirmSelection.disabled) return;
      selectedRepository = { ...captured }; closeRepositoryPicker(); renderRepositorySelection(); $('text').focus?.();
      status('已选择仓库内容；尚未发送。输入问题后点击发送，只用于下一条普通聊天。');
    };
    const field = (name, input) => { const label = el('label', null, name); label.append(input); return label; };
    container.replaceChildren(note, field('仓库', repository), field('固定提交', commit), field('读取方式', mode), field('文件路径或搜索文字', target), preview, load, confirmSelection);
    commit.focus?.();
  } catch (error) { if (valid()) container.replaceChildren(el('p', 'note', '无法读取仓库列表：' + error.message + '。可关闭后重试。')); }
}
