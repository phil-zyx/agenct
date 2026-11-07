/* Pure presentation of the participant-filtered #11/#12 detail DTO.
 * This is NOT an authorization filter; the Hub enforces visibility. */
function prepareTaskTimeline(task, events, deliveries, audit) {
  const fail = () => { throw Error('任务时间线响应无效'); };
  const string = value => typeof value === 'string';
  const integer = value => Number.isSafeInteger(value) && value >= 0;
  const fields = (value, names) => value && typeof value === 'object' && !Array.isArray(value)
    && Object.keys(value).every(key => names.includes(key));
  const states = ['awaiting_owner', 'working', 'ready_for_acceptance', 'completed', 'canceled'];
  const labels = { created: '创建任务', accept: '接手', deliver: '提交交付', return: '退回', approve: '验收通过', cancel: '取消' };
  if (!task || !string(task.id) || !integer(task.revision) || !Array.isArray(events)
      || !Array.isArray(deliveries) || !audit || audit.schema_version !== 1
      || typeof audit.legacy_history_incomplete !== 'boolean'
      || (audit.legacy_notice !== null && !string(audit.legacy_notice))) fail();
  const submitted = new Map();
  for (const delivery of deliveries) {
    if (!fields(delivery, ['seq', 'author', 'content', 'created_at', 'task_revision'])
        || !integer(delivery.seq) || delivery.seq < 1 || submitted.has(delivery.seq)
        || !string(delivery.author) || !string(delivery.content) || !string(delivery.created_at)
        || !integer(delivery.task_revision) || delivery.task_revision > task.revision) fail();
    submitted.set(delivery.seq, delivery);
  }
  if ([...submitted.keys()].sort((a, b) => a - b).some((seq, index) => seq !== index + 1)) fail();
  const used = new Set(), rows = [];
  let previous = 0;
  const deliveryText = delivery => `交付版本 ${delivery.seq} · ${delivery.author}\n记录时间：${delivery.created_at}\n${delivery.content}`;
  for (const event of events) {
    if (!fields(event, ['seq', 'actor', 'action', 'at', 'from_status', 'to_status', 'revision', 'delivery_seq', 'provenance', 'reason'])
        || !integer(event.seq) || event.seq !== previous + 1 || !string(event.actor)
        || !Object.hasOwn(labels, event.action) || !string(event.at)
        || !['recorded', 'legacy'].includes(event.provenance)) fail();
    if (event.reason != null && (event.action !== 'cancel' || !string(event.reason)
        || !event.reason.trim() || Array.from(event.reason).length > 500)) fail();
    previous = event.seq;
    const legacy = event.provenance === 'legacy';
    if (legacy && !audit.legacy_history_incomplete) fail();
    if (event.from_status !== null && !states.includes(event.from_status)) fail();
    if (event.to_status !== null && !states.includes(event.to_status)) fail();
    if (event.revision !== null && (!integer(event.revision) || event.revision > task.revision)) fail();
    if (!legacy && (event.to_status === null || event.revision === null
        || (event.from_status === null && event.action !== 'created'))) fail();
    let text = `${labels[event.action]} · ${event.actor}\n记录时间：${event.at}`;
    if (legacy) text += '\n旧记录：历史状态或版本信息可能不完整。';
    else text += `\n${event.from_status ?? '未创建'} → ${event.to_status} · 任务版本 ${event.revision}`;
    if (event.delivery_seq !== null) {
      const delivery = submitted.get(event.delivery_seq);
      if (!delivery || used.has(delivery.seq) || event.action !== 'deliver'
          || (!legacy && (delivery.task_revision !== event.revision || delivery.author !== event.actor))) fail();
      used.add(delivery.seq);
      text += '\n' + deliveryText(delivery);
    } else if (!legacy && event.action === 'deliver') fail();
    if (event.action === 'cancel') text += event.reason == null
      ? '\n旧记录未保存取消原因。' : '\n取消原因（双方可见）：' + event.reason;
    rows.push(text);
  }
  // Retained legacy results may lack a trustworthy corresponding event. Keep
  // them separate rather than inventing their place in the transition history.
  const unlinked = [...submitted.values()].filter(delivery => !used.has(delivery.seq));
  if (unlinked.length && !audit.legacy_history_incomplete) fail();
  const legacyDeliveries = unlinked.sort((a, b) => a.seq - b.seq)
    .map(delivery => '保留的旧交付，无法核实对应历史事件。\n' + deliveryText(delivery));
  return {
    notice: audit.legacy_history_incomplete
      ? '历史记录不完整：旧版本中已覆盖或清空的交付无法恢复。以下仅展示现存证据。'
      : '任务共享时间线，仅含已提交交付和任务状态变更。',
    rows, legacyDeliveries
  };
}
if (typeof module !== 'undefined') module.exports = { prepareTaskTimeline };
