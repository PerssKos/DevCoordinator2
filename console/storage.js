/* Storage actions share the native inventory, policy and cleanup engine. */
(() => {
  'use strict';
  const terminal = new Set(['completed', 'partial', 'failed', 'cancelled']);
  function create({api, esc, bytes, icon}) {
    const s = { rows: [], total: 0, next: null, inventory: null, repositories: [], selected: new Map(), detail: null, plan: null, policy: null, job: null, error: null, loading: false, read: 0, epoch: 0, repository: '', filesystem: '', kind: '', safety: '', query: '', dialog: null, signal: null, root: null };
    const i18n = () => window.DevCoordinatorI18n;
    const t = (key, args = {}) => i18n().t(`storage.${key}`, args);
    const h = (key, args = {}) => esc(t(key, args));
    const date = value => value == null ? '—' : i18n().date(new Date(value).toISOString());
    const key = () => crypto.randomUUID();
    const compact = (value, limit = 18) => value.length > limit ? `${value.slice(0, limit - 1)}…` : value;
    const reason = code => { try { return t(`reason_${code}`); } catch { return t('reason_unknown'); } };
    const selectedRows = () => s.rows.filter(row => s.selected.get(row.artifact_id) === row.revision && row.deletable);
    const current = () => s.rows.find(row => row.artifact_id === s.detail);
    const safetyIcon = row => row.safety === 'safe' ? 'circle-check' : ['protected', 'in_use'].includes(row.safety) ? 'lock' : 'alert-triangle';
    function safety(row) { return `<span class="storage-safety ${esc(row.safety)}">${icon(safetyIcon(row))}<span>${h(`safety_${row.safety}`)}</span></span>`; }
    function size(rows) {
      const seen = new Set(); let total = 0, known = false;
      for (const row of rows) {
        const id = row.accounting_id || row.artifact_id;
        if (seen.has(id) || row.allocated_bytes == null) continue;
        seen.add(id); total += row.allocated_bytes; known = true;
      }
      return known ? bytes(total) : '—';
    }
    function fault(error) {
      if (error?.code === 'stale' || s.signal?.aborted) return;
      s.error = error?.code === 'storage_conflict' ? t('changed') : error?.code === 'permission_denied' ? t('denied') : t('failed');
      draw();
    }
    function markup() {
      const row = current();
      const projects = new Map(s.repositories.map(r => [r.repository_id, r.display_name]));
      s.rows.forEach(r => { if (r.repository_id) projects.set(r.repository_id, r.repository_name || r.repository_id); });
      const active = s.job && !terminal.has(s.job.state);
      return `<div class="storage-layout${row ? ' has-detail' : ''}">
        <section class="storage-collection" aria-label="${h('artifacts')}">
          <header class="storage-heading"><h1>${h('title')}</h1><div class="actions"><button class="btn" id="storage-scan"${active ? ' disabled' : ''}>${icon('refresh')}<span>${h('scan')}</span></button><button class="btn" id="storage-policy">${icon('settings')}<span>${h('policies')}</span></button></div></header>
          <div class="storage-filters"><label class="storage-search"><span class="sr-only">${h('search')}</span>${icon('search')}<input id="storage-query" type="search" value="${esc(s.query)}" placeholder="${h('search')}"></label>
            <label><span class="sr-only">${h('project')}</span><select id="storage-project"><option value="">${h('all_projects')}</option>${[...projects].sort((a,b) => a[1].localeCompare(b[1])).map(([id,name]) => `<option value="${esc(id)}" title="${esc(name)}"${s.repository === id ? ' selected' : ''}>${esc(compact(name, 13))}</option>`).join('')}</select></label>
            <label><span class="sr-only">${h('disk')}</span><select id="storage-disk"><option value="">${h('all_disks')}</option>${(s.inventory?.filesystems || []).map(f => `<option value="${esc(f.filesystem_id)}"${s.filesystem === f.filesystem_id ? ' selected' : ''}>${esc(f.label)}</option>`).join('')}</select></label>
            <label><span class="sr-only">${h('deletion_safety')}</span><select id="storage-safety" data-ui-continuation-anchor="safe-filter"><option value="">${h('all_artifacts')}</option>${[['safe','safe_short'],['in_use','in_use_short'],['protected','protected_short'],['needs_review','needs_review_short'],['observing','observing_short']].map(([value,label]) => `<option value="${value}"${s.safety === value ? ' selected' : ''}>${h(label)}</option>`).join('')}</select></label>
          </div>
          <div class="storage-tools"><label><span class="sr-only">${h('type')}</span><select id="storage-kind"><option value="">${h('types_short')}</option>${[['volume','volume_short'],['container','container_short'],['image','image_short'],['build_cache','cache_short'],['network','network_short'],['build_output','output_short'],['dependency_cache','deps_short'],['worktree','worktree_short'],['backup','backup_short'],['evidence','evidence_short'],['backing_directory','backing_short'],['mount','mount_short'],['unknown','other_short']].map(([kind,label]) => `<option value="${kind}"${s.kind === kind ? ' selected' : ''}>${h(label)}</option>`).join('')}</select></label><button class="btn btn-small btn-primary" id="storage-clean"${s.rows.some(r => r.automatic_eligible && r.deletable) ? '' : ' disabled'}>${h('clean_eligible')}</button><button class="btn btn-small" id="storage-eligible"${s.rows.some(r => r.automatic_eligible) ? '' : ' disabled'}>${h('select_eligible')}</button><button class="btn btn-small" id="storage-history">${h('history')}</button></div>
          ${s.error ? `<div class="notice bad" role="alert">${esc(s.error)} <button class="btn btn-small" id="storage-retry">${h('retry')}</button></div>` : ''}
          ${s.inventory?.coverage_gaps.length ? `<div class="storage-coverage" role="status">${icon('info-circle')}<span>${h('coverage_gap')}</span></div>` : ''}
          ${s.job ? jobMarkup() : ''}
          <div class="storage-list" aria-busy="${s.loading}">${s.rows.length ? table() : `<div class="storage-empty">${s.loading ? h('loading') : !s.inventory?.last_scan_at_ms ? h('not_scanned') : h('empty')}<button class="btn" id="storage-empty-scan">${h('scan')}</button></div>`}</div>
          <footer class="storage-footer"><span>${h('showing', {shown:s.rows.length,total:s.total})}</span>${s.next != null ? `<button class="btn btn-small" id="storage-more">${h('show_more')}</button>` : ''}<span>${s.inventory?.last_scan_at_ms ? h('checked', {date:date(s.inventory.last_scan_at_ms)}) : ''}</span></footer>
        </section>
        ${row ? inspector(row) : `<aside class="storage-idle"><span>${icon('shield')}${h('inspect_prompt')}</span></aside>`}
      </div>`;
    }
    function table() {
      const groups = new Map();
      s.rows.forEach(row => { const id = row.group_id || row.repository_id || 'shared'; if (!groups.has(id)) groups.set(id, []); groups.get(id).push(row); });
      return `<table class="storage-table"><thead><tr><th class="storage-check"><input type="checkbox" id="storage-select-all" aria-label="${h('select_visible')}"${s.rows.filter(r => r.deletable).every(r => s.selected.has(r.artifact_id)) && s.rows.some(r => r.deletable) ? ' checked' : ''}${s.rows.some(r => r.deletable) ? '' : ' disabled'}></th><th>${h('artifact')}</th><th class="storage-type">${h('type')}</th><th>${h('size')}</th><th class="storage-used">${h('last_used')}</th><th>${h('deletion_safety')}</th></tr></thead><tbody>${[...groups.values()].map(rows => {
        const first = rows[0];
        return `<tr class="storage-group"><th colspan="6">${icon('folder')}<strong>${esc(first.repository_name || t('shared'))}</strong>${first.group_name ? `<span>${esc(first.group_name)}</span>` : ''}</th></tr>${rows.map((row, index) => `<tr class="storage-row${s.detail === row.artifact_id ? ' selected' : ''}" data-storage-row="${esc(row.artifact_id)}">
          <td class="storage-check">${row.deletable ? `<input type="checkbox" data-storage-select="${esc(row.artifact_id)}" aria-label="${esc(t('select_named', {name:row.name}))}"${s.selected.has(row.artifact_id) ? ' checked' : ''}>` : ''}</td>
          <td><button type="button" class="storage-name" data-storage-inspect="${esc(row.artifact_id)}"${row.artifact_id === s.rows[0]?.artifact_id ? ' data-storage-first-inspect="true"' : ''} aria-expanded="${s.detail === row.artifact_id}">${icon(row.kind === 'volume' ? 'database' : 'folder')}<span>${esc(row.name)}<small class="storage-mobile-type">${h('kind_' + row.kind)}</small></span></button></td><td class="storage-type">${h('kind_' + row.kind)}</td><td class="storage-size">${esc(bytes(row.allocated_bytes))}</td><td class="storage-used">${esc(date(row.last_used_at_ms))}</td><td>${safety(row)}</td></tr>`).join('')}`;
      }).join('')}</tbody></table>`;
    }
    function inspector(row) {
      const selection = selectedRows();
      const items = s.plan?.items || [];
      const permanent = [...selection, ...items].some(i => ['permanent_data','recovery_copy','source_worktree','runtime_resource'].includes(i.effect));
      const active = s.job && !terminal.has(s.job.state);
      const blockers = [...new Set(items.flatMap(item => item.blockers))];
      return `<aside class="storage-inspector" role="dialog" data-ui-contextual-overlay="artifact details overlay" aria-label="${h('details')}" tabindex="-1"><header>${icon(row.kind === 'volume' ? 'database' : 'folder')}<div><h2>${esc(row.name)}</h2><p>${esc(row.repository_name || t('shared'))}</p></div><button class="btn storage-icon" id="storage-close" aria-label="${h('close')}">${icon('x')}</button></header>
        <dl class="storage-facts"><div><dt>${h('size')}</dt><dd>${esc(bytes(row.allocated_bytes))}</dd></div><div><dt>${h('type')}</dt><dd>${h('kind_' + row.kind)}</dd></div><div><dt>${h('ownership')}</dt><dd>${esc(row.ownership || t('unknown_ownership'))}</dd></div><div><dt>${h('last_used')}</dt><dd>${esc(date(row.last_used_at_ms))}</dd></div><div><dt>${h('last_checked')}</dt><dd>${esc(date(row.verified_at_ms))}</dd></div><div><dt>${h('scheduled_deletion')}</dt><dd>${esc(row.eligible_at_ms ? date(row.eligible_at_ms) : '—')}</dd></div></dl>
        <section><h3>${row.deletable ? h('why_safe') : h('why_keep')}</h3>${safety(row)}<ul class="storage-reasons">${row.reasons.map(code => `<li>${esc(reason(code))}</li>`).join('')}</ul>${row.eligible_at_ms && !row.automatic_eligible && row.deletable ? `<p class="muted">${h('automatic_after', {date:date(row.eligible_at_ms)})}</p>` : ''}</section>
        ${row.dependencies?.length ? `<section><h3>${h('dependencies')}</h3><ul class="storage-dependencies">${row.dependencies.map(name => `<li>${esc(name)}</li>`).join('')}</ul></section>` : ''}
        ${selection.length && row.deletable && s.selected.has(row.artifact_id) ? `<section><h3>${h('selected', {count:selection.length})}<span>${esc(size(selection))}</span></h3>${s.plan ? `${dependencies(items)}${blockers.length ? `<ul class="storage-reasons storage-blockers">${blockers.map(code => `<li>${esc(reason(code))}</li>`).join('')}</ul>` : ''}` : `<p class="muted">${h('checking_selection')}</p>`}</section>
          <section class="storage-delete"><h3>${permanent ? h('permanent_title') : h('rebuildable_title')}</h3><p>${permanent ? h('permanent_body') : h('rebuildable_body')}</p><button class="btn btn-danger" id="storage-delete"${s.plan?.ready && !active ? '' : ' disabled'}>${icon('trash')}<span>${h('delete_selected')}</span><strong>${esc(size(selection))}</strong></button></section>` : ''}
        <div class="storage-inspector-actions"><button class="btn" id="storage-protect"${active ? ' disabled' : ''}>${icon('shield')}<span>${row.protected ? h('unprotect') : h('protect')}</span></button>${row.safety === 'needs_review' && row.reasons.every(r => ['ownership_unknown','disposal_not_authorized','owner_state_unverified'].includes(r)) ? `<button class="btn" id="storage-review">${h('review_ownership')}</button>` : ''}</div>
        ${s.policy ? `<section class="storage-policy-summary"><h3>${h('policy_title')}</h3><dl><div><dt>${h('caches')}</dt><dd>${h('days', {count:s.policy.cache_idle_seconds/86400})}</dd></div><div><dt>${h('other_artifacts')}</dt><dd>${h('days', {count:s.policy.data_idle_seconds/86400})}</dd></div></dl><span>${s.policy.automatic ? h('automatic_on') : h('automatic_off')}</span></section>` : ''}
      </aside>`;
    }
    function dependencies(items) {
      const groups=new Map();items.forEach(item=>{if(!groups.has(item.kind))groups.set(item.kind,[]);groups.get(item.kind).push(item);});
      return [...groups].map(([kind,rows])=>`<details class="storage-dependency-group"><summary><span>${h('plural_'+kind)}</span><strong>${rows.length}</strong></summary><ul class="storage-dependencies">${rows.map(item=>`<li><span>${esc(item.name)}</span>${item.blockers.length?`<span class="storage-safety needs_review">${h('safety_needs_review')}</span>`:''}</li>`).join('')}</ul></details>`).join('');
    }
    function jobMarkup() {
      return `<div class="storage-job" role="status"><strong>${s.job.state==='running'?h('running_'+s.job.kind):h('job_' + s.job.state)}</strong><span>${h('job_items', {count:s.job.receipts.length})}</span>${!terminal.has(s.job.state) ? `<button class="btn btn-small" id="storage-cancel-job">${h('cancel_remaining')}</button>` : ''}${s.job.receipts.some(r => r.code) ? `<details><summary>${h('details')}</summary><ul>${s.job.receipts.filter(r => r.code).map(r => `<li>${esc(reason(r.code))}</li>`).join('')}</ul></details>` : ''}</div>`;
    }
    function draw() {
      if (!s.root || s.signal?.aborted) return;
      const active = document.activeElement;
      const focus = active?.id, start = active?.selectionStart, end = active?.selectionEnd;
      s.root.innerHTML = markup();
      bind();
      const replacement = focus ? s.root.querySelector(`#${CSS.escape(focus)}`) : null;
      if (replacement) { replacement.focus({preventScroll:true}); if (typeof start === 'number' && replacement.setSelectionRange) replacement.setSelectionRange(start, end); }
    }
    function bind() {
      const $ = selector => s.root.querySelector(selector);
      $('#storage-scan').onclick = scan;
      $('#storage-empty-scan')?.addEventListener('click', scan);
      $('#storage-policy').onclick = policies;
      $('#storage-clean').onclick = cleanEligible;
      $('#storage-history').onclick = history;
      $('#storage-retry')?.addEventListener('click', () => load());
      $('#storage-more')?.addEventListener('click', () => load(true));
      $('#storage-close')?.addEventListener('click', () => { const id=s.detail;s.detail=null;draw();s.root.querySelector(`[data-storage-inspect="${CSS.escape(id)}"]`)?.focus(); });
      $('#storage-query').oninput = event => { s.query=event.target.value; clearTimeout(s.searchTimer);s.searchTimer=setTimeout(() => load(),200); };
      for (const [id,prop] of [['project','repository'],['disk','filesystem'],['kind','kind'],['safety','safety']]) $(`#storage-${id}`).onchange = event => {s[prop]=event.target.value;s.selected.clear();s.plan=null;load();};
      $('#storage-select-all')?.addEventListener('change', event => {s.rows.filter(r => r.deletable).forEach(r => event.target.checked ? s.selected.set(r.artifact_id,r.revision) : s.selected.delete(r.artifact_id));prepare();});
      $('#storage-eligible').onclick = () => {s.rows.filter(r => r.automatic_eligible).forEach(r => s.selected.set(r.artifact_id,r.revision));prepare();};
      s.root.querySelectorAll('[data-storage-select]').forEach(input => input.onchange = () => {const row=s.rows.find(r => r.artifact_id===input.dataset.storageSelect);if(input.checked&&row?.deletable)s.selected.set(row.artifact_id,row.revision);else s.selected.delete(input.dataset.storageSelect);prepare();});
      s.root.querySelectorAll('[data-storage-inspect]').forEach(button => button.onclick = () => {s.detail=button.dataset.storageInspect;draw();s.root.querySelector('.storage-inspector')?.focus({preventScroll:true});});
      $('#storage-delete')?.addEventListener('click', remove);
      $('#storage-protect')?.addEventListener('click', async () => {const row=current();try{const updated=await api('storage.protection.set',{artifact_id:row.artifact_id,expected_revision:row.revision,protected:!row.protected},false);s.selected.delete(row.artifact_id);s.plan=null;s.safety='';const index=s.rows.findIndex(item=>item.artifact_id===updated.artifact_id);if(index<0)s.rows.unshift(updated);else s.rows[index]=updated;s.detail=updated.artifact_id;s.total=Math.max(s.total,s.rows.length);draw();}catch(error){fault(error);}});
      $('#storage-review')?.addEventListener('click', review);
      $('#storage-cancel-job')?.addEventListener('click', async () => {try{s.job=await api('storage.job.cancel',{job_id:s.job.job_id},false);draw();}catch(error){fault(error);}});
    }
    async function load(more=false) {
      const read=++s.read, epoch=s.epoch;
      s.loading=true;s.error=null;draw();
      try {
        const [inventoryRead, policyRead] = await Promise.allSettled([
          api('storage.inventory',{repository_id:s.repository||null,filesystem_id:s.filesystem||null,kind:s.kind||null,safety:s.safety||null,query:s.query||null,offset:more?s.next:0,limit:50}),
          api('storage.policy.get',{repository_id:s.repository||null}),
        ]);
        if (inventoryRead.status === 'rejected') throw inventoryRead.reason;
        const result=inventoryRead.value;
        if(read!==s.read||epoch!==s.epoch||s.signal.aborted)return;
        s.inventory=result;s.rows=more?[...s.rows,...result.artifacts]:result.artifacts;s.total=result.total;s.next=result.next_offset;
        for(const [id,revision] of s.selected){const row=s.rows.find(r=>r.artifact_id===id);if(!row?.deletable||row.revision!==revision)s.selected.delete(id);}
        if(!current())s.detail=null;
        if(policyRead.status === 'fulfilled') s.policy=policyRead.value;
        s.loading=false;draw();
      }catch(error){s.loading=false;fault(error);}
    }
    async function prepare() {
      const ids=selectedRows().map(r=>r.artifact_id);
      const ticket=++s.planRead;
      s.plan=null;s.error=null;if(ids.length&&!current())s.detail=ids[0];draw();
      if(!ids.length)return;
      if(ids.length>200){s.error=t('selection_limit');draw();return;}
      try {const plan=await api('storage.cleanup.plan',{artifact_ids:ids,include_persistent_data:true});if(ticket===s.planRead){s.plan=plan;s.planSelection=ids.join(',');draw();}}catch(error){fault(error);}
    }
    async function watch(job) {
      const epoch=s.epoch;let cursor=null;s.job=job;draw();
      try {
        while(epoch===s.epoch&&!s.signal.aborted&&!terminal.has(s.job.state)){
          const response=await api('event.wait',{cursor,filters:[{filter_id:'storage',categories:['other'],kinds:['storage.job.started','storage.job.progress','storage.job.finished','storage.job.failed'],deadline_at:new Date(Date.now()+30000).toISOString()}]});
          cursor=response.cursor;
          if(epoch!==s.epoch||s.signal.aborted)return;
          s.job=await api('storage.job.status',{job_id:job.job_id});draw();
        }
        if(epoch===s.epoch&&!s.signal.aborted){s.selected.clear();s.plan=null;await load();}
      }catch(error){fault(error);}
    }
    async function scan() {try {await watch(await api('storage.scan',{repository_id:s.repository||null,idempotency_key:key()},false));}catch(error){fault(error);}}
    async function cleanEligible() {
      const rows = s.rows.filter(row => row.automatic_eligible && row.deletable);
      if (!rows.length) { s.error = t('nothing_eligible'); draw(); return; }
      s.selected.clear(); rows.forEach(row => s.selected.set(row.artifact_id, row.revision));
      s.detail = rows[0].artifact_id;
      try {
        const plan = await api('storage.cleanup.plan', { artifact_ids: rows.map(row => row.artifact_id), automatic: true, include_persistent_data: false });
        s.plan = plan; s.planSelection = rows.map(row => row.artifact_id).join(','); draw();
        if (!plan.ready) return;
        s.plan = null; draw();
        await watch(await api('storage.cleanup.start', { plan_id: plan.plan_id, idempotency_key: key() }, false));
      } catch (error) { fault(error); }
    }
    async function remove() {
      if(!s.plan?.ready||selectedRows().map(r=>r.artifact_id).join(',')!==s.planSelection)return;
      const plan=s.plan;s.plan=null;draw();
      try {await watch(await api('storage.cleanup.start',{plan_id:plan.plan_id,idempotency_key:key()},false));}catch(error){await load();fault(error);}
    }
    function dialog(title,content) {
      s.dialog?.close();s.dialog?.remove();
      const trigger=document.activeElement;
      const element=document.createElement('dialog');element.className='storage-dialog';element.setAttribute('aria-label',title);
      element.innerHTML=`<header><h2>${esc(title)}</h2><button class="btn storage-icon" data-close aria-label="${h('close')}">${icon('x')}</button></header>${content}`;
      element.querySelector('[data-close]').onclick=()=>element.close();
      element.addEventListener('close',()=>{element.remove();if(s.dialog===element)s.dialog=null;trigger?.isConnected&&trigger.focus();},{once:true});
      document.body.append(element);s.dialog=element;element.showModal();return element;
    }
    async function policies() {
      try {
        const p=await api('storage.policy.get',{repository_id:s.repository||null});
        const d=dialog(t('policies'),`<form><p>${s.repository?esc(s.repositories.find(r=>r.repository_id===s.repository)?.display_name||current()?.repository_name||t('project')):h('all_projects')}</p><label class="storage-toggle"><input name="automatic" type="checkbox"${p.automatic?' checked':''}>${h('automatic_label')}</label><label>${h('cache_days')}<input name="cache" type="number" min="1" max="3650" required value="${p.cache_idle_seconds/86400}"></label><label>${h('other_days')}<input name="data" type="number" min="1" max="3650" required value="${p.data_idle_seconds/86400}"></label><label>${h('backup_floor')}<input name="backups" type="number" min="2" max="1000" required value="${p.minimum_verified_backups}"></label><p class="muted">${h('policy_protection')}</p><p class="storage-form-error" role="alert"></p><footer><button type="button" class="btn" data-cancel>${h('cancel')}</button><button type="submit" class="btn btn-primary">${h('save')}</button></footer></form>`);
        d.querySelector('[data-cancel]').onclick=()=>d.close();d.querySelector('form').onsubmit=async event=>{event.preventDefault();const f=event.target;const button=f.querySelector('[type=submit]');button.disabled=true;try{s.policy=await api('storage.policy.set',{repository_id:s.repository||null,expected_revision:p.revision,automatic:f.elements.automatic.checked,cache_idle_seconds:Number(f.elements.cache.value)*86400,data_idle_seconds:Number(f.elements.data.value)*86400,minimum_verified_backups:Number(f.elements.backups.value)},false);d.close();await load();}catch(error){f.querySelector('[role=alert]').textContent=error.code==='storage_conflict'?t('changed'):t('failed');button.disabled=false;}};
      }catch(error){fault(error);}
    }
    function review() {
      const row=current();if(!row)return;
      const d=dialog(t('review_ownership'),`<form><p><strong>${esc(row.name)}</strong></p><p>${h('review_body')}</p><label>${h('data_effect')}<select name="effect"><option value="permanent_data">${h('effect_permanent_data')}</option><option value="rebuildable">${h('effect_rebuildable')}</option></select></label><label>${h('review_reason')}<textarea name="reason" required maxlength="2000" rows="3"></textarea></label><p class="storage-form-error" role="alert"></p><footer><button type="button" class="btn" data-cancel>${h('cancel')}</button><button class="btn btn-primary" type="submit">${h('allow_cleanup')}</button></footer></form>`);
      d.querySelector('[data-cancel]').onclick=()=>d.close();d.querySelector('form').onsubmit=async event=>{event.preventDefault();const f=event.target;const button=f.querySelector('[type=submit]');button.disabled=true;try{if(row.group_id&&row.reasons.includes('disposal_not_authorized')){const job=await api('storage.legacy.register',{deployment_id:row.group_id,expected_inventory_revision:s.inventory.revision,reason:f.elements.reason.value},false);d.close();await watch(job);}else{await api('storage.register',{artifact_id:row.artifact_id,expected_revision:row.revision,repository_id:row.repository_id,effect:f.elements.effect.value,reason:f.elements.reason.value},false);d.close();await load();}}catch(error){f.querySelector('[role=alert]').textContent=t('failed');button.disabled=false;}};
    }
    async function history() {
      try {const result=await api('storage.history',{artifact_id:s.detail,limit:20});const d=dialog(t('history'),`<ol class="storage-history">${result.jobs.map(job=>`<li><button class="storage-name" data-job="${esc(job.job_id)}"><span>${h('job_'+job.state)}<small>${esc(date(job.created_at_ms))}</small></span><span>${h('job_items',{count:job.receipts.length})}</span></button></li>`).join('')}</ol>${!result.jobs.length?`<p>${h('no_history')}</p>`:''}`);d.querySelectorAll('[data-job]').forEach(button=>button.onclick=async()=>{try{s.job=await api('storage.job.status',{job_id:button.dataset.job});d.close();draw();if(!terminal.has(s.job.state))watch(s.job);}catch(error){fault(error);}});}catch(error){fault(error);}
    }
    async function show(root,administrator,signal) {
      s.epoch++;s.root=root;s.signal=signal;s.planRead=0;s.loading=true;s.error=null;
      await i18n().ensure('storage');
      if(!administrator){root.innerHTML=`<h1>${h('title')}</h1><p>${h('denied')}</p>`;return;}
      signal.addEventListener('abort',()=>{clearTimeout(s.searchTimer);s.dialog?.close();},{once:true});
      s.repository=new URLSearchParams(location.hash.split('?')[1]||'').get('repository')||s.repository;
      draw();await load();
      api('repository.list',{}).then(result=>{if(!signal.aborted){s.repositories=result.repositories||[];draw();}}).catch(()=>{});
      document.addEventListener('dc2:localechange',()=>draw(),{signal});
    }
    return {show};
  }
  window.DevCoordinatorStorage={create};
})();
