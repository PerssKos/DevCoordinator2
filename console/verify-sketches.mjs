import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import path from 'node:path';
import { verifySketchHistoryLayout } from './verify-sketches-formal.mjs';

export async function verifySketches({ page, browser, call, request, check, base, temporary, restart, cookieFor, out, root }) {
  const repository_id = 'r1111111111111111';
  const surface_id = 'controller-review';
  page.setDefaultTimeout(8000);
  page.on('console',message=>{if(message.type()==='error')fs.appendFile(path.join(out,'browser-errors.log'),message.text()+'\n');});
  const clickSave = async (locator, operation) => {
    const response = page.waitForResponse(r=>r.url().endsWith('/api/v2/design.sketch.'+operation)&&r.request().method()==='POST');
    await locator.click();const body=await (await response).json();assert.equal(body.ok,true,body.error?.message);return body.data;
  };
  const imagePaths=[];const imageViewports=new Map();
  // Test-only captures supply distinct, genuine PNGs without a generator dependency.
  for(let index=0;index<9;index++){
    await page.setContent(`<main style="font:22px system-ui;background:${index%2?'#e1f2ef':'#eff2f3'};color:#202a30;padding:40px;width:820px;min-height:500px"><h1>Controller review — fixture ${index+1}</h1><p>One window with a diagram and review context.</p><section style="border:2px solid #08766e;padding:24px;margin-top:50px"><h2>Diagram ${index+1}</h2><p>Power input → regulator → telemetry</p></section><p>Generation ${index<3?'A':'B'} · Option ${index%3+1}</p></main>`);
    const file=path.join(temporary,`mockup-${index}.png`);const png=await page.locator('main').screenshot({path:file});imagePaths.push(file);imageViewports.set(file,`${png.readUInt32BE(16)}x${png.readUInt32BE(20)}`);
  }
  const record=path.join(temporary,'generation.json');await fs.writeFile(record,JSON.stringify({prompt:'One controller review window. Preserve the diagram, context, and single-window scope.'}));
  const manifest=(description,parents=[])=>({surface_id,surface_title:'Controller review',element_ids:['diagram','context'],state:'populated',theme:'light',viewport:'1440x1024',window_count:1,description,journey:'Choose a design direction while preserving the review context.',decisions:'Keep context beside the diagram.',instructions:'Show one window with its diagram and review context.',constraints:'Do not introduce additional windows or combine themes.',parent_relations:parents.map(parent_sketch_id=>({parent_sketch_id,relation:'adjusted_from'})),transition_note:parents.length?'Combine the selected parents and keep the annotation accent green.':''});
  const input=(key,start,parents=[])=>({repository_id,sketch_set:start?'Refined choices':'First exploration',source_skill:'acceptance-fixture',manifest_version:2,generation_record_path:record,idempotency_key:key,images:[0,1,2].map(index=>({title:`${start?'Refined':'Initial'} direction ${index+1}`,display_order:index+1,path:imagePaths[start+index],manifest:{...manifest(`Agent-authored ${start?'refined':'initial'} description for option ${index+1}: keep the diagram and its adjacent review context.`,parents),viewport:imageViewports.get(imagePaths[start+index])}}))});
  const initialInput=input('initial',0);
  const initial=await call('design.sketch.publish',initialInput,null);
  const first=initial.sketches.map(node=>node.sketch_id);
  await check('Retained publication preserves display order and idempotency',async()=>{
    assert.deepEqual(initial.sketches.map(node=>node.display_order),[1,2,3]);
    assert.equal((await call('design.sketch.publish',initialInput,null)).batch_id,initial.batch_id);
    const changed=structuredClone(initialInput);changed.images[0].title='Different intent';
    assert.equal((await request('design.sketch.publish',changed,null)).ok,false);
  });
  await check('Incomplete, shared, or cross-surface manifests cannot publish',async()=>{
    for(const adjust of [x=>delete x.images[0].manifest,x=>x.images[0].manifest.description='',x=>x.images[0].manifest.window_count=2,x=>x.images[1].path=x.images[0].path,x=>x.images[1].display_order=1]){
      const invalid=input(crypto.randomUUID(),6);adjust(invalid);
      assert.equal((await request('design.sketch.publish',invalid,null)).ok,false);
    }
    const reused=structuredClone(initialInput);reused.idempotency_key=crypto.randomUUID();reused.images[0].manifest.surface_id='other-window';
    assert.equal((await request('design.sketch.publish',reused,null)).ok,false);
    assert.equal((await call('design.sketch.list',{repository_id,include_legacy:true})).sketches.filter(node=>!node.legacy).length,3);
  });
  await call('design.sketch.decision',{repository_id,sketch_id:first[2],expected_revision:0,decision:'reject',rationale:'Reject this alternative but retain it in the story.'});
  const go=async(id=first[0])=>{await page.goto(`${base}#/sketches/${repository_id}?surface=${surface_id}&sketch=${id}`);await page.locator(`[data-main-mockup][data-mockup="${id}"][data-loaded=true]`).waitFor({timeout:8000});};
  await check('Gallery opens the real history route and all three options',async()=>{
    await page.goto(`${base}#/sketches/${repository_id}`);
    await page.locator('[data-review-set]').first().click();
    await page.locator('[data-main-mockup][data-loaded=true]').waitFor({timeout:8000});
    assert.equal(await page.locator('[data-option]').count(),3);
    assert.equal(await page.locator('.mockup-scope').getAttribute('open'),null);
    assert.match(await page.locator('[data-initial-context]').innerText(),/Agent-authored initial description/);
    assert.match(await page.locator(`[data-option="${first[2]}"]`).innerText(),/Rejected/);
  },page);
  if (process.env.SKETCH_SMOKE_ONLY) return;
  await check('Multiple choices and their comment persist in the real database and after reload',async()=>{
    await go();await page.locator(`[data-option="${first[0]}"] input`).check();await page.locator(`[data-option="${first[1]}"] input`).check();
    await page.locator('[data-continuation] summary').click();await page.locator('[data-continuation] textarea').fill('Keep options 1 and 2; combine their layout and context.');
    const active=await clickSave(page.locator('[data-continue]'),'activate');await page.waitForFunction(rev=>Number(document.querySelector('[data-sketch-story]')?.dataset.historyRevision)>=rev,active.revision);
    const saved=await call('design.sketch.resolve',{repository_id,surface_id});assert.equal(saved.status,'resolved');assert.deepEqual(saved.current.map(n=>n.sketch_id),first.slice(0,2));
    await page.reload();await page.locator('[data-option] input:checked').first().waitFor();assert.equal(await page.locator('[data-option] input:checked').count(),2);
    assert.equal((await call('design.sketch.get',{repository_id,sketch_id:first[2]})).sketch.decision,'reject');
  },page);
  await check('Clearing and restoring the explicit selection never falls back to an older Keep flag',async()=>{
    for(const id of first.slice(0,2))await page.locator(`[data-option="${id}"] input`).uncheck();
    const cleared=await clickSave(page.locator('[data-continue]'),'activate');await page.waitForFunction(rev=>Number(document.querySelector('[data-sketch-story]')?.dataset.historyRevision)>=rev,cleared.revision);
    assert.equal((await call('design.sketch.resolve',{repository_id,surface_id})).status,'unavailable');
    for(const id of first.slice(0,2))await page.locator(`[data-option="${id}"] input`).check();
    const restored=await clickSave(page.locator('[data-continue]'),'activate');await page.waitForFunction(rev=>Number(document.querySelector('[data-sketch-story]')?.dataset.historyRevision)>=rev,restored.revision);
    assert.deepEqual((await call('design.sketch.resolve',{repository_id,surface_id})).current.map(node=>node.sketch_id),first.slice(0,2));
  },page);
  const refined=await call('design.sketch.publish',input('refined',3,first.slice(0,2)),null);const next=refined.sketches.map(node=>node.sketch_id);
  await check('New generations link every selected parent and never automatically replace a choice',async()=>{
    const resolved=await call('design.sketch.resolve',{repository_id,surface_id});assert.deepEqual(resolved.current.map(n=>n.sketch_id),first.slice(0,2));
    const detail=await call('design.sketch.get',{repository_id,sketch_id:next[0]});assert.deepEqual(detail.lineage.map(edge=>edge.parent_sketch_id),first.slice(0,2));
    const invalid=input('foreign-parent',6,[next[0]]);invalid.images.forEach(image=>image.manifest.surface_id='foreign-surface');
    assert.equal((await request('design.sketch.publish',invalid,null)).ok,false);
  });
  await check('The newest explicit multi-option selection supersedes older Keep flags',async()=>{
    await go(next[0]);await page.locator(`[data-option="${next[0]}"] input`).check();
    await go(first[0]);assert.equal(await page.locator('[data-option] input:checked').count(),2,'another generation must show its own current choices, not hidden draft choices');
    await go(next[0]);assert.equal(await page.locator('[data-option] input:checked').count(),1,'returning to the batch preserves its pending choice');
    await go(next[0]);await page.locator(`[data-option="${next[0]}"] input`).check();await page.locator(`[data-option="${next[2]}"] input`).check();
    await page.locator(`[data-option="${next[2]}"] a`).click();await page.locator(`[data-main-mockup][data-mockup="${next[2]}"][data-loaded=true]`).waitFor();assert.equal(await page.locator('[data-option] input:checked').count(),2);
    const active=await clickSave(page.locator('[data-continue]'),'activate');await page.waitForFunction(rev=>Number(document.querySelector('[data-sketch-story]')?.dataset.historyRevision)>=rev,active.revision);
    const saved=await call('design.sketch.resolve',{repository_id,surface_id});assert.deepEqual(saved.current.map(n=>n.sketch_id),[next[0],next[2]]);
    assert.equal((await call('design.sketch.get',{repository_id,sketch_id:first[0]})).sketch.decision,'keep');
    const story=await call('design.sketch.story',{repository_id,surface_id,limit:1});assert.equal(story.has_more,true);assert.equal(story.current.length,2);assert.equal(story.nodes.length,1);
  },page);
  await check('Rejecting an active option updates currentness through the annotation workspace',async()=>{
    await go(next[0]);await page.locator('a[href*="annotate=1"]').click();
    page.once('dialog',dialog=>dialog.accept('Reject this active direction but retain the other selected option.'));
    await clickSave(page.locator('[data-sketch-decision="reject"]'),'decision');
    const rejected=await call('design.sketch.resolve',{repository_id,surface_id});
    assert.deepEqual(rejected.current.map(node=>node.sketch_id),[next[2]]);
    await go(next[0]);await page.locator('[data-option="'+next[0]+'"] input').check();
    const active=await clickSave(page.locator('[data-continue]'),'activate');
    await page.waitForFunction(rev=>Number(document.querySelector('[data-sketch-story]')?.dataset.historyRevision)>=rev,active.revision);
    assert.deepEqual(new Set((await call('design.sketch.resolve',{repository_id,surface_id})).current.map(node=>node.sketch_id)),new Set([next[0],next[2]]));
    assert.ok((await call('design.sketch.get',{repository_id,sketch_id:next[0]})).history.some(event=>event.decision==='reject'));
  },page);
  await check('Legacy images remain viewable and searchable but cannot become current',async()=>{
    const legacy=await call('design.sketch.list',{repository_id,include_legacy:true});
    assert.ok(legacy.sketches.some(node=>node.legacy));
    assert.ok((await call('design.sketch.search',{repository_id,query:'Historical',include_legacy:true})).sketches.some(node=>node.legacy));
    assert.equal((await call('design.sketch.resolve',{repository_id,surface_id:'legacy:k0000000000000001'})).status,'legacy_only');
    const rejected=await request('design.sketch.activate',{repository_id,surface_id,sketch_ids:['s0000000000000001'],expected_revision:(await call('design.sketch.resolve',{repository_id,surface_id})).revision,action:'restore',rationale:'Legacy adoption is forbidden'});
    assert.equal(rejected.ok,false);
    await page.goto(base+'#/sketches/'+repository_id+'?sketch=s0000000000000001');
    await page.locator('#evidence-canvas').waitFor();
    assert.equal(await page.locator('[data-continue]').count(),0);
  },page);
  const original=await call('design.sketch.get',{repository_id,sketch_id:next[0]});
  await check('Context edits preserve the agent description and image bytes',async()=>{
    await go(next[0]);await page.locator('[data-edit-context]').click();const dialog=page.locator('dialog.mockup-editor');
    await dialog.locator('[name=description]').fill('Follow this direction and make the annotation accent green. Keep the existing single-window layout.');
    await dialog.locator('[name=instructions]').fill('Follow this direction and paint the annotation accent green.');
    await dialog.locator('[name=rationale]').fill('Owner refinement: green accent, same layout.');await dialog.locator('[type=submit]').click();await dialog.waitFor({state:'detached'});
    const saved=await call('design.sketch.get',{repository_id,sketch_id:next[0]});assert.equal(saved.initial_context.description,original.initial_context.description);assert.match(saved.sketch.description,/accent green/);assert.equal(saved.sketch.sha256,original.sketch.sha256);
    await page.reload();await page.locator('[data-edit-context]').waitFor();assert.equal(await page.locator('[data-initial-context]').innerText(),original.initial_context.description);
    await page.locator('.mockup-context>details:not(.mockup-initial-details)>summary').click();assert.match(await page.locator('[data-current-context]').innerText(),/accent green/);
  },page);
  await check('Cancel and failed context saves preserve saved data and the draft',async()=>{
    await page.locator('[data-edit-context]').click();let dialog=page.locator('.mockup-editor');await dialog.locator('[name=description]').fill('Discard this draft without changing the saved direction.');await dialog.locator('[data-cancel]').click();
    assert.match((await call('design.sketch.get',{repository_id,sketch_id:next[0]})).sketch.description,/accent green/);
    await page.locator('[data-edit-context]').click();dialog=page.locator('.mockup-editor');const draft='Keep the green accent and provide a compact review legend.';await dialog.locator('[name=description]').fill(draft);await dialog.locator('[name=rationale]').fill('Add a compact legend.');
    await page.route('**/api/v2/design.sketch.description',route=>route.fulfill({status:503,json:{ok:false,error:{code:'daemon_unavailable',message:'Fixture save failure'}}}));
    await dialog.locator('[type=submit]').click();await dialog.locator('[role=alert]:not([hidden])').waitFor();assert.equal(await dialog.locator('[name=description]').inputValue(),draft);
    await page.unroute('**/api/v2/design.sketch.description');await dialog.locator('[type=submit]').click();await dialog.waitFor({state:'detached'});
    assert.equal((await call('design.sketch.get',{repository_id,sketch_id:next[0]})).sketch.description,draft);
  },page);
  await check('Comments and all prior context remain searchable',async()=>{
    await page.locator('.mockup-comments>summary').click();await page.locator('[data-comment] textarea').fill('Retain a searchable heliotrope note for the next generation.');await clickSave(page.locator('[data-comment] button'),'annotation.create');await page.getByText('Retain a searchable heliotrope note for the next generation.',{exact:true}).waitFor({state:'attached'});await page.locator('.mockup-comments>summary').click();await page.getByText('Retain a searchable heliotrope note for the next generation.',{exact:true}).waitFor();
    for(const query of ['heliotrope','paint the annotation','Agent-authored refined','Owner refinement']){const found=await call('design.sketch.search',{repository_id,query,include_legacy:true});assert.ok(found.sketches.some(node=>node.sketch_id===next[0]),query);}
    await page.goto(`${base}#/sketches/${repository_id}`);await page.locator('[data-search] [name=q]').fill('heliotrope');await clickSave(page.locator('[data-search] [type=submit]'),'search');await page.waitForFunction(()=>document.querySelectorAll('[data-review-set]').length===1);assert.equal(await page.locator('[data-review-set]').count(),1);
    await page.locator('[data-search] [name=q]').fill('no-such-mockup-result');await clickSave(page.locator('[data-search] [type=submit]'),'search');await page.waitForFunction(()=>document.querySelector('[data-collection] .notice')?.textContent.includes('0'));
    assert.equal(await page.locator('[data-review-set]').count(),0);assert.doesNotMatch(await page.locator('[data-collection]').innerText(),/No sketches have been published/);
  },page);
  await check('Stale writes and unauthorized readers cannot change another source',async()=>{
    const saved=await call('design.sketch.resolve',{repository_id,surface_id});
    assert.equal((await request('design.sketch.activate',{repository_id,surface_id,sketch_ids:[first[0]],expected_revision:0,action:'restore',rationale:'Stale client'})).error.code,'configuration_conflict');
    assert.equal((await request('design.sketch.list',{repository_id},'viewer@example.test')).error.code,'permission_denied');
    assert.equal((await request('design.sketch.annotation.create',{repository_id:'r2222222222222222',sketch_id:next[0],body:'Wrong repository',marks:[]})).ok,false);
    assert.deepEqual((await call('design.sketch.resolve',{repository_id,surface_id})).current,saved.current);
  });
  await check('Real backend restart preserves selections and context history',async()=>{
    const before=await call('design.sketch.resolve',{repository_id,surface_id});await restart();const after=await call('design.sketch.resolve',{repository_id,surface_id});assert.deepEqual(after,before);
    await go(next[0]);assert.equal(await page.locator('[data-option] input:checked').count(),2);
    assert.match((await call('design.sketch.get',{repository_id,sketch_id:next[0]})).initial_context.description,/Agent-authored refined/);
  },page);
  await check('Historical inspection returns directly to the explicit current source',async()=>{
    await go(first[0]);await page.locator('[data-current-source]').click();await page.waitForFunction(ids=>ids.includes(document.querySelector('[data-main-mockup][data-loaded=true]')?.dataset.mockup),[next[0],next[2]]);
    assert.ok(next.filter((_,index)=>index!==1).includes(await page.locator('[data-main-mockup]').getAttribute('data-mockup')));
  },page);
  await check('Long histories and older context pages load from the real API',async()=>{
    const drawing=await browser.newPage({viewport:{width:960,height:700}});
    try {for(let round=0;round<7;round++){
      const generated=input('history-page-'+round,0,[next[0]]);generated.sketch_set='Additional generation '+(round+1);
      for(const [index,image]of generated.images.entries()){
        await drawing.setContent(`<main style="font:20px system-ui;padding:32px;width:820px;height:540px;background:#e5f0ed;color:#162429"><h1>Controller review</h1><p>Additional generation ${round+1}, option ${index+1}</p></main>`);
        const filename=path.join(temporary,`history-${round}-${index}.png`);const png=await drawing.locator('main').screenshot({path:filename});image.path=filename;image.manifest.viewport=`${png.readUInt32BE(16)}x${png.readUInt32BE(20)}`;image.title=`Additional ${round+1}.${index+1}`;
      }
      await call('design.sketch.publish',generated,null);
    }
    }finally{await drawing.close();}
    for(let revision=0;revision<4;revision++){
      const current=(await call('design.sketch.get',{repository_id,sketch_id:next[0]})).sketch;
      await call('design.sketch.description',{repository_id,sketch_id:next[0],expected_revision:current.description_revision,description:current.description+' Retained refinement '+(revision+1)+'.',journey:current.journey,decisions:current.decisions,instructions:current.instructions,constraints:current.constraints,rationale:'Context paging fixture revision '+(revision+1)});
    }
    await go(next[0]);const before=await page.locator('.mockup-history-node').count();await page.locator('[data-history-more]').click();await page.waitForFunction(count=>document.querySelectorAll('.mockup-history-node').length>count,before);
    await page.locator('.mockup-context>details:not(.mockup-initial-details)>summary').click();const contexts=await page.locator('[data-context-revisions]>li').count();await page.locator('[data-context-more]').click();await page.waitForFunction(count=>document.querySelectorAll('[data-context-revisions]>li').length>count,contexts);
    await page.goto(`${base}#/sketches/${repository_id}`);await page.locator('[data-more]:not([hidden])').waitFor();
    assert.ok([next[0],next[2]].includes(await page.locator('.mockup-card img').first().getAttribute('data-mockup')),'explicit current heads lead the gallery even after later generations');
    const cards=await page.locator('[data-review-set]').count();await page.locator('[data-more]').click();await page.waitForFunction(count=>document.querySelectorAll('[data-review-set]').length>count,cards);
    assert.deepEqual(new Set((await call('design.sketch.resolve',{repository_id,surface_id})).current.map(node=>node.sketch_id)),new Set([next[0],next[2]]));
  },page);
  await check('Collapsed history loads old images only when their generation is opened',async()=>{
    const fresh=await browser.newPage({viewport:{width:1440,height:1058}});const reads=[];
    fresh.on('request',request=>{if(request.url().endsWith('/api/v2/design.sketch.image'))reads.push(request.postDataJSON().sketch_id);});
    try {
      await fresh.goto(`${base}#/sketches/${repository_id}?surface=${surface_id}&sketch=${next[0]}`);
      await fresh.waitForFunction(()=>document.querySelectorAll('[data-option] img[data-loaded=true]').length===3);
      assert.ok(reads.length>0);assert.ok(reads.every(id=>next.includes(id)),JSON.stringify(reads));
      await fresh.locator('.mockup-round>summary').first().click();await fresh.locator('.mockup-round[open] img[data-loaded=true]').first().waitFor();
      assert.ok(reads.some(id=>!next.includes(id)),'opening an older generation retrieves its retained images');
    }finally{await fresh.close();}
  });
  for(const width of [1440,1101,1100,1099,761,760,759,390])for(const theme of ['light','dark']){
    await check(`Mockup history ${width}px ${theme} keeps all choices and controls reachable`,async()=>{
      await page.setViewportSize({width,height:1058});await page.emulateMedia({colorScheme:theme});await page.evaluate(theme=>{localStorage.setItem('dc2-theme',theme);document.documentElement.dataset.theme=theme;},theme);await go(next[0]);
      assert.equal(await page.locator('[data-option]').count(),3);assert.equal(await page.locator('.mockup-scope').getAttribute('open'),null);assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);
      assert.equal(await page.locator('.mockup-round[open]').count(),0);
      await page.locator('[data-edit-context]').click();assert.equal(await page.locator('.mockup-editor [name=description]').evaluate(el=>el===document.activeElement),true);await page.keyboard.press('Escape');
      assert.equal(await page.locator('.mockup-editor').count(),0);
    },page);
  }
  await fs.writeFile(path.join(out,'fixture-identities.json'),JSON.stringify({repository_id,surface_id,initial:first,refined:next,source_root:root},null,2));
  if(process.env.SKETCH_FORMAL==='1') await verifySketchHistoryLayout({base,repository_id,surface_id,sketch_id:next[0],root,out,check,browser});
}
