// The mockup story is the projection of retained Coordinator records.
// Browsing, review decisions, and the explicit continuation set stay distinct.
'use strict';
window.DevCoordinatorSketches = (() => {
  const drafts = new Map();
  const route = (repo, surface, sketch, extra = {}) => {
    const query = new URLSearchParams(extra);
    if (surface) query.set('surface', surface);
    if (sketch) query.set('sketch', sketch);
    return `#/sketches/${encodeURIComponent(repo)}${query.size ? `?${query}` : ''}`;
  };
  const i18n = () => window.DevCoordinatorI18n;
  const h = (key, args) => i18n().markup(`sketches.${key}`, args);
  const text = (key) => i18n().t(`sketches.${key}`);
  const common = (key, args) => i18n().markup(`common.${key}`, args);
  const evidence = (key, args) => i18n().markup(`evidence.${key}`, args);
  const date = (value) => Number.isFinite(Date.parse(value)) ? new Intl.DateTimeFormat(i18n().locale, { dateStyle:'medium', timeStyle:'short' }).format(new Date(value)) : '—';
  const byBatch = (nodes) => [...nodes.reduce((groups,node) => {
    if (!groups.has(node.batch_id)) groups.set(node.batch_id,{id:node.batch_id,title:node.sketch_set,nodes:[]});
    groups.get(node.batch_id).nodes.push(node);return groups;
  },new Map()).values()];

  async function mount(main, {repositoryId:repo,api,esc:e,signal,imageUrl,openAnnotations}) {
    const query = new URLSearchParams(location.hash.split('?')[1] || '');
    const surface = query.get('surface');
    let draftKey;
    let generation=0;
    const current = () => !signal.aborted;
    const read = (operation,params) => api(`design.sketch.${operation}`,{repository_id:repo,...params});
    const showError = (error, container=main) => {
      if (!current() || error.code==='stale') return;
      container.innerHTML=`<div class="notice" role="alert">${e(error.message)} <button class="btn" data-retry>${common('retry_942087')}</button></div>`;
      container.querySelector('[data-retry]')?.addEventListener('click',()=>window.render());
    };
    const images = (root,nodes) => {
      const lookup=new Map(nodes.map(node=>[node.sketch_id,node]));
      for (const img of root.querySelectorAll('img[data-mockup]')) {
        if(img.dataset.loaded==='true'||img.closest('details:not([open])'))continue;
        const node=lookup.get(img.dataset.mockup);if(!node)continue;
        imageUrl(repo,node).then(url=>{if(current()&&img.isConnected){img.onload=()=>{if(img.isConnected)img.dataset.loaded='true';};img.src=url;}}).catch(error=>{if(current()&&img.isConnected){img.alt=text('option_unavailable_d03e4e');img.closest('figure,button,a')?.classList.add('unavailable');}});
      }
    };
    const status = (node) => node.legacy?h('legacy'):node.current?h('currentSelection'):node.decision==='reject'?h('rejected_aea4a0'):node.decision==='keep'?`${common('status_keep')} · ${h('history')}`:h('undecided_00cc36');
    const thumb = (node) => `<img data-mockup="${e(node.sketch_id)}" alt="${e(node.title)}" decoding="async">`;
    const scopeHref = (node) => node.legacy ? route(repo,null,node.sketch_id) : route(repo,node.surface_id,node.sketch_id);

    if (!surface) {
      let nodes=[],offset=0,hasMore=false;
      const params={include_legacy:query.get('legacy')!=='0',current_only:query.get('current')==='1',state:query.get('state')||undefined,theme:query.get('theme')||undefined};
      const search=query.get('q')||'';
      main.innerHTML=`<section class="mockup-gallery" data-ui-region="sketches-primary"><header class="mockup-heading"><h1>${h('sketches_a56d78')}</h1></header><form class="mockup-search" data-search><label><input aria-label="${e(text('search'))}" type="search" name="q" value="${e(search)}"></label><button class="btn" type="submit">${h('search')}</button><details><summary data-ui-continuation-anchor>${evidence('details_45989d')}</summary><div class="mockup-filters" role="dialog" aria-label="${e(text('search'))}" data-ui-contextual-overlay="Search filters"><label><input type="checkbox" name="current"${params.current_only?' checked':''}>${h('currentSelection')}</label><label><input type="checkbox" name="legacy"${params.include_legacy?' checked':''}>${h('legacy')}</label><label>${evidence('state_a3b50c')}<input name="state" value="${e(params.state||'')}"></label><label>${h('theme')}<input name="theme" value="${e(params.theme||'')}"></label></div></details></form><div data-collection aria-live="polite">${common('loading_more_results_d16d8b')}</div><button class="btn mockup-more" data-more hidden>${h('earlier')}</button></section>`;
      const collection=main.querySelector('[data-collection]');const more=main.querySelector('[data-more]');
      const filters=main.querySelector('[data-search] details');filters.addEventListener('toggle',()=>{if(filters.open)filters.querySelector('input').focus();});filters.addEventListener('keydown',event=>{if(event.key==='Escape'){filters.open=false;filters.querySelector('summary').focus();}});
      main.querySelector('[data-search]').addEventListener('submit',event=>{event.preventDefault();const data=new FormData(event.currentTarget);location.hash=route(repo,null,null,{q:String(data.get('q')||''),state:String(data.get('state')||''),theme:String(data.get('theme')||''),current:data.has('current')?'1':'0',legacy:data.has('legacy')?'1':'0'});});
      const load=async()=>{
        more.disabled=true;
        try {
          const [result,heads]=await Promise.all([read(search?'search':'list',{...params,...(search?{query:search}:{}),offset,limit:24}),!search&&!params.current_only&&offset===0?read('list',{...params,current_only:true,include_legacy:false,offset:0,limit:64}):Promise.resolve({sketches:[]})]);if(!current())return;
          nodes=[...new Map([...heads.sketches,...nodes,...result.sketches].map(node=>[node.sketch_id,node])).values()];offset+=result.sketches.length;hasMore=result.has_more;
          const groups=new Map();for(const node of nodes){const key=node.surface_id||`legacy:${node.batch_id}`;if(!groups.has(key))groups.set(key,{title:node.surface_title||node.sketch_set,nodes:[]});groups.get(key).nodes.push(node);}
          collection.innerHTML=groups.size?[...groups.values()].map(group=>`<section class="mockup-surface"><header><h2>${e(group.title)}</h2></header><div class="mockup-gallery-grid">${group.nodes.map(node=>`<article class="mockup-card"><a class="mockup-card-image" href="${e(scopeHref(node))}">${thumb(node)}</a><div><h3><span class="mockup-order">${e(node.display_order||'')}</span> ${e(node.title)}</h3><p class="muted">${e(node.sketch_set)}</p><span class="mockup-status ${node.current?'current':node.decision==='reject'?'rejected':''}">${status(node)}</span><p>${e(node.description||'')}</p><a class="btn btn-small" href="${e(scopeHref(node))}" data-review-set>${h('review_set_ca242c')}</a></div></article>`).join('')}</div></section>`).join(''):`<p class="notice">${search||params.current_only||params.state||params.theme||!params.include_legacy?`${h('sketches_a56d78')} · 0`:common('no_sketches_have_been_published_for_this_project_8bde12')}</p>`;
          images(collection,nodes);more.hidden=!hasMore;
        }catch(error){showError(error,collection);}finally{if(current())more.disabled=false;}
      };
      more.addEventListener('click',load);await load();return;
    }

    let story, resolution, detail, batchNodes=[],selected=new Set(),draftDirty=false;
    let historyNodes=[],historyOffset=0,historyMore=false,activationOffset=0;
    const requestedSketch=query.get('sketch');
    const load=async()=>{
      const ticket=++generation;
      const [nextStory,nextResolution]=await Promise.all([read('story',{surface_id:surface,limit:24}),read('resolve',{surface_id:surface})]);
      if(!current()||ticket!==generation)return;
      story=nextStory;resolution=nextResolution;
      if(story.revision!==resolution.revision){story=await read('story',{surface_id:surface,limit:24});if(story.revision!==resolution.revision)throw new Error(i18n().t('common.error_conflict'));}
      const activeId=requestedSketch||resolution.current[0]?.sketch_id||story.nodes[0]?.sketch_id;
      if(!activeId){main.innerHTML=`<p class="notice">${h('no_selection_211f6b')}</p>`;return;}
      detail=await read('get',{sketch_id:activeId});
      if(detail.sketch.surface_id!==surface)throw new Error(text('option_unavailable_d03e4e'));
      draftKey=`${repo}:${surface}:${detail.sketch.batch_id}`;
      batchNodes=[];let offset=0;
      for(;;){const part=await read('list',{batch_id:detail.sketch.batch_id,include_legacy:false,offset,limit:64});batchNodes.push(...part.sketches);offset+=part.sketches.length;if(!part.has_more)break;if(!part.sketches.length)throw new Error(text('option_unavailable_d03e4e'));}
      if(!current()||ticket!==generation)return;
      if(query.has('annotate'))return openAnnotations(batchNodes,activeId);
      const draft=drafts.get(draftKey);
      selected=new Set(draft?.ids||resolution.current.filter(node=>node.batch_id===detail.sketch.batch_id).map(node=>node.sketch_id));draftDirty=Boolean(draft);
      if(draft)resolution.revision=draft.revision;
      historyNodes=story.nodes;historyOffset=story.next_offset??story.nodes.length;activationOffset=story.next_activation_offset??story.activations.length;historyMore=story.has_more||story.next_activation_offset!=null;
      renderStory();
    };
    function selectionSummary(){return evidence('selectionSummary',{selected:selected.size,total:batchNodes.length});}
    function updateSelection(){
      main.querySelector('[data-selection-count]').innerHTML=selectionSummary();
      const button=main.querySelector('[data-continue]');button.disabled=!draftDirty;
      for(const card of main.querySelectorAll('[data-option]')){const on=selected.has(card.dataset.option);card.classList.toggle('selected',on);card.querySelector('input').checked=on;}
    }
    function renderHistory(){
      const container=main.querySelector('[data-history]');
      container.innerHTML=byBatch(historyNodes).map((batch,index)=>`<details class="mockup-round"><summary data-ui-continuation-anchor><strong>${e(batch.title)}</strong><span>${batch.nodes.length}</span></summary><ol>${batch.nodes.map(node=>{
        const parents=story.lineage.filter(edge=>edge.child_sketch_id===node.sketch_id);
        return `<li class="mockup-history-node"><a href="${e(route(repo,surface,node.sketch_id))}">${thumb(node)}<span><strong>${e(node.display_order||'')} · ${e(node.title)}</strong><small>${status(node)}</small></span></a>${parents.length?`<p>${h('basedOn')} ${parents.map(parent=>`<a href="${e(route(repo,surface,parent.parent_sketch_id))}">${e(historyNodes.find(n=>n.sketch_id===parent.parent_sketch_id)?.title||text('open_sketch_3ccc5c'))}</a>`).join(', ')}</p><p>${e(parents[0].rationale)}</p>`:''}</li>`;
      }).join('')}</ol></details>`).join('');
      const activity=main.querySelector('[data-selection-history]');
      activity.innerHTML=story.activations.map(event=>`<li><strong>${h('selected_57fd7a')}</strong> <time>${e(date(event.created_at))}</time><p>${e(event.rationale)}</p><div>${event.sketch_ids.map(id=>`<a href="${e(route(repo,surface,id))}">${e(historyNodes.find(n=>n.sketch_id===id)?.title||text('open_sketch_3ccc5c'))}</a>`).join(' · ')}</div></li>`).join('');
      images(container,historyNodes);
      for(const group of container.querySelectorAll('.mockup-round'))group.addEventListener('toggle',()=>{if(group.open)images(group,historyNodes);});
      main.querySelector('[data-history-more]').hidden=!historyMore;
    }
    function renderContext(){
      const node=detail.sketch;
      const initial=detail.initial_context?.description||'';
      main.querySelector('[data-initial-context]').textContent=initial.length>420?initial.slice(0,420)+'…':initial;
      main.querySelector('[data-initial-complete]').textContent=initial.length>420?initial:'';
      const latest=main.querySelector('[data-current-context]');latest.textContent=node.description||'';
      main.querySelector('[data-context-revisions]').innerHTML=detail.description_history.map(rev=>`<li><time>${e(date(rev.created_at))}</time><p>${e(rev.rationale)}</p><p>${e(rev.description)}</p><details><summary data-ui-continuation-anchor>${evidence('details_45989d')}</summary>${contextFields(rev)}</details></li>`).join('');
      main.querySelector('[data-context-more]').hidden=!detail.context_history_has_more;
    }
    function contextFields(context){return `<dl class="mockup-context-facts">${['journey','decisions','instructions','constraints'].map(field=>`<dt>${field==='decisions'?evidence('decision_640ae4'):h(field)}</dt><dd>${e(context[field]||'')}</dd>`).join('')}</dl>`;}
    function renderStory(){
      const node=detail.sketch;
      main.innerHTML=`<section class="mockup-story" data-sketch-story data-history-revision="${resolution.revision}" data-ui-region="sketches-primary"><header class="mockup-heading"><div><a href="${e(route(repo))}">${h('sketches_a56d78')}</a><h1>${e(story.surface_title||surface)}</h1></div><div class="actions"><a class="btn" href="${e(route(repo,surface,node.sketch_id,{annotate:'1'}))}">${h('open_full_review_canvas_26fc95')}</a></div></header><div class="mockup-story-layout"><section class="mockup-preview"><div class="mockup-preview-title"><h2>${e(node.title)}</h2><span class="mockup-status ${node.current?'current':''}">${node.current?h('currentSelection'):status(node)}</span></div><figure data-ui-theme-exception="Retained mockup preserves the source image colors"><img data-mockup="${e(node.sketch_id)}" alt="${e(node.title)}" data-main-mockup></figure></section><aside class="mockup-story-inspector"><details class="mockup-scope"><summary data-ui-continuation-anchor><span class="ti ti-chevron-down" aria-hidden="true"></span>${evidence('details_45989d')} ${resolution.current.length?`<a data-current-source href="${e(route(repo,surface))}">${h('currentSelection')} ${resolution.current.length}</a>`:`<span>${h('currentSelection')} 0</span>`}</summary><dl><dt>${h('surface')}</dt><dd>${e(node.surface_title)}</dd><dt>${h('elements')}</dt><dd>${e(node.element_ids.join(', '))}</dd><dt>${evidence('state_a3b50c')}</dt><dd>${e(node.state)}</dd><dt>${h('theme')}</dt><dd>${e(node.theme)}</dd><dt>${evidence('viewport_91e53b')}</dt><dd>${e(node.viewport)}</dd><dt>${h('currentSelection')}</dt><dd>${resolution.current.map(head=>`<a href="${e(route(repo,surface,head.sketch_id))}">${e(head.title)}</a>`).join(' · ')}</dd></dl></details><section class="mockup-options"><header><h2>${e(node.sketch_set)}</h2><span data-selection-count>${selectionSummary()}</span></header><div class="mockup-option-grid">${batchNodes.map(option=>`<article class="mockup-option ${selected.has(option.sketch_id)?'selected':''} ${option.decision==='reject'?'rejected':''}" data-option="${e(option.sketch_id)}"><label><input type="checkbox" value="${e(option.sketch_id)}"${selected.has(option.sketch_id)?' checked':''}>${e(option.display_order)} · ${e(option.title)}</label><a href="${e(route(repo,surface,option.sketch_id))}">${thumb(option)}</a><small>${status(option)}</small></article>`).join('')}</div><form data-continuation><details><summary data-ui-continuation-anchor>${h('selectionNote')}</summary><textarea name="rationale" rows="2" maxlength="2000" aria-label="${e(text('selectionNote'))}"></textarea></details><button class="btn btn-primary" data-continue type="submit" disabled>${h('continueSelected')}</button><p class="mockup-error" data-selection-error role="alert" hidden></p></form></section><section class="mockup-context"><header><h2>${h('initialContext')}</h2><button type="button" class="btn btn-small" data-edit-context>${evidence('edit_464c4f')}</button></header><p data-initial-context></p><details class="mockup-initial-details"><summary data-ui-continuation-anchor>${evidence('details_45989d')}</summary><p data-initial-complete></p>${contextFields(detail.initial_context||node)}</details><details><summary data-ui-continuation-anchor>${h('latestContext')}</summary><p data-current-context></p>${contextFields(node)}<ol data-context-revisions></ol><button type="button" class="btn btn-small" data-context-more hidden>${h('earlier')}</button></details></section><section class="mockup-history"><h2>${h('history')}</h2><div data-history></div><button class="btn btn-small" type="button" data-history-more hidden>${h('earlier')}</button><details><summary data-ui-continuation-anchor>${h('currentSelection')}</summary><ol class="mockup-activity" data-selection-history></ol></details></section><details class="mockup-comments"><summary data-ui-continuation-anchor>${h('comments_355f79')}</summary><div data-comments>${detail.annotations.map(note=>`<article><time>${e(date(note.created_at))}</time><p>${e(note.body)}</p></article>`).join('')}</div><form data-comment><label><textarea aria-label="${e(i18n().t('evidence.commentLabel'))}" name="body" minlength="3" maxlength="2000" rows="2" required></textarea></label><button class="btn btn-small" type="submit">${evidence('postComment')}</button><p role="alert" class="mockup-error" hidden></p></form></details></aside></div></section>`;
      images(main,[node,...batchNodes]);renderHistory();renderContext();
      if(drafts.has(draftKey))main.querySelector('[data-continuation] textarea').value=drafts.get(draftKey).note||'';
      const saveDraft=()=>{drafts.set(draftKey,{ids:[...selected],revision:resolution.revision,note:main.querySelector('[data-continuation] textarea').value});while(drafts.size>20)drafts.delete(drafts.keys().next().value);};
      main.querySelector('[data-continuation] textarea').addEventListener('input',()=>{draftDirty=true;saveDraft();updateSelection();});
      for(const input of main.querySelectorAll('[data-option] input'))input.addEventListener('change',()=>{if(input.checked)selected.add(input.value);else selected.delete(input.value);draftDirty=true;saveDraft();updateSelection();});
      updateSelection();
      main.querySelector('[data-continuation]').addEventListener('submit',async event=>{
        event.preventDefault();const form=event.currentTarget;const button=form.querySelector('button');const error=form.querySelector('[role=alert]');button.disabled=true;error.hidden=true;
        try {
          const rationale=String(new FormData(form).get('rationale')||'').trim()||text('selected_57fd7a');
          await read('activate',{surface_id:surface,sketch_ids:[...selected],expected_revision:resolution.revision,action:'select',rationale});
          drafts.delete(draftKey);
          await load();
        }catch(problem){if(current()){error.textContent=problem.message;error.hidden=false;button.disabled=false;}}
      });
      main.querySelector('[data-comment]').addEventListener('submit',async event=>{
        event.preventDefault();const form=event.currentTarget;const button=form.querySelector('button');const error=form.querySelector('[role=alert]');button.disabled=true;error.hidden=true;
        try{await read('annotation.create',{sketch_id:node.sketch_id,body:String(new FormData(form).get('body')||''),marks:[]});detail=await read('get',{sketch_id:node.sketch_id});resolution=await read('resolve',{surface_id:surface});if(current())renderStory();}
        catch(problem){if(current()){error.textContent=problem.message;error.hidden=false;button.disabled=false;}}
      });
      main.querySelector('[data-edit-context]').addEventListener('click',openContextEditor);
      main.querySelector('[data-history-more]').addEventListener('click',async event=>{
        event.currentTarget.disabled=true;try{const next=await read('story',{surface_id:surface,offset:historyOffset,activation_offset:activationOffset,limit:24});if(!current())return;historyNodes.push(...next.nodes);story.lineage.push(...next.lineage);story.activations.push(...next.activations.filter(a=>!story.activations.some(b=>b.revision===a.revision)));historyMore=next.has_more||next.next_activation_offset!=null;historyOffset=next.next_offset??historyOffset+next.nodes.length;activationOffset=next.next_activation_offset??activationOffset+next.activations.length;renderHistory();}catch(error){showError(error);}finally{if(current())main.querySelector('[data-history-more]').disabled=false;}
      });
      main.querySelector('[data-context-more]').addEventListener('click',async event=>{
        const before=detail.description_history.at(-1)?.revision;event.currentTarget.disabled=true;
        try{const older=await read('get',{sketch_id:node.sketch_id,context_before_revision:before});detail.description_history.push(...older.description_history);detail.context_history_has_more=older.context_history_has_more;if(current())renderContext();}catch(error){showError(error);}finally{if(current())main.querySelector('[data-context-more]').disabled=false;}
      });
    }
    function openContextEditor(event){
      const opener=event.currentTarget;const node=detail.sketch;const dialog=document.createElement('dialog');dialog.className='mockup-editor';
      dialog.innerHTML=`<form><header><h2>${h('latestContext')}</h2><button type="button" class="btn" data-cancel>${evidence('cancel_19766e')}</button></header><label>${h('description')}<textarea name="description" minlength="20" maxlength="8000" rows="5" required>${e(node.description)}</textarea></label>${['journey','decisions','instructions','constraints'].map(field=>`<label>${field==='decisions'?evidence('decision_640ae4'):h(field)}<textarea name="${field}" maxlength="4000" rows="2" required>${e(node[field])}</textarea></label>`).join('')}<label>${h('whyChanged')}<input name="rationale" maxlength="2000" required></label><p class="mockup-error" role="alert" hidden></p><footer><button class="btn btn-primary" type="submit">${evidence('save_1509f5')}</button></footer></form>`;
      document.body.appendChild(dialog);dialog.showModal();dialog.querySelector('textarea').focus();
      const close=()=>{dialog.close();dialog.remove();if(opener.isConnected)opener.focus();};
      dialog.querySelector('[data-cancel]').addEventListener('click',close);dialog.addEventListener('cancel',event=>{event.preventDefault();close();});signal.addEventListener('abort',()=>dialog.remove(),{once:true});
      dialog.querySelector('form').addEventListener('submit',async event=>{
        event.preventDefault();const data=Object.fromEntries(new FormData(event.currentTarget));const button=dialog.querySelector('[type=submit]');const error=dialog.querySelector('[role=alert]');button.disabled=true;error.hidden=true;
        try{await read('description',{sketch_id:node.sketch_id,expected_revision:node.description_revision,...data});await load();close();main.querySelector('[data-edit-context]')?.focus();}
        catch(problem){if(current()){error.textContent=problem.message;error.hidden=false;button.disabled=false;}}
      });
    }
    main.innerHTML=`<p class="notice">${common('loading_more_results_d16d8b')}</p>`;
    try{await load();}catch(error){showError(error);}
  }
  return {route,mount};
})();
