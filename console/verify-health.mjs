// Extends the real Console/SQLite acceptance fixture used by Glossary.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';

// Deterministic resource samples isolate the incident acceptance from host
// load. Incident reads/writes/permissions and SQLite are never mocked here.
export function healthReadFixture(operation) {
  const host={cpu_percent:28.6,ncpu:32,memory_used:156*2**30,memory_total:252*2**30,fs_used:1.8*2**40,fs_free:.1*2**40,fs_size:1.9*2**40,load_1:8.54,reconciliation:{managed_cpu_percent:20,other_cpu_percent:8.6,managed_memory:80*2**30,other_memory:76*2**30}};
  if(operation==='health.summary')return {host,alerts:[],unhealthy_deployments:[],active_tests:[],container_counts:{}};
  if(operation==='health.history')return {points:[0,1,2,3,4,5].map(i=>({minute:`2026-09-23T17:0${i}:00Z`,avg:20+i,min:19+i,max:22+i}))};
  if(operation==='health.repositories')return {host,repositories:[
    {repository_id:'r1111111111111111',display_name:'Kaizen',root_path:'/fixture/Kaizen',cpu_percent:1,memory_bytes:2**30,storage_bytes:20*2**30,deployments:[{deployment_id:'d1111111111111111',name:'web',source:'worktree',state:'failed'}]},
    ...[0,1].map(i=>({repository_id:`hdlripper-${i}`,display_name:i?'0.1.7-generated-workspace':'hdlripper',root_path:i?'/fixture/hdlripper/.local/workspaces/release':'/fixture/hdlripper',repository_source:{key:'verified-git-hdlripper',name:'hdlripper'},cpu_percent:i+1,memory_bytes:2**30,storage_bytes:(i?2:20)*2**30,deployments:Array.from({length:30},(_,n)=>({deployment_id:`fixture-${i}-${n}`,name:'web-build',source:'worktree',state:'completed'}))}))
  ]};
  return null;
}

export async function verifyHealth({ page, browser, base, call, request, check, cookieFor, out, root }) {
  const list = view => call('health.incidents', {view, limit:50});
  const response = {agent_response:'Restarted once; the health check still failed.',escalation_reason:'The agent needs your decision on whether to roll back.',next_step:'Review the previous working version.'};
  let web; let disk;
  await check('Automatic observations are not owner escalations', async()=>{
    const a=await list('attention'); assert.equal(a.attention_count,0); assert.equal(a.incidents.length,0);
    const all=await list('all'); assert.equal(all.incidents.length,4);
    web=all.incidents.find(i=>i.deployment_id); disk=all.incidents.find(i=>i.alert_key==='host/disk');
    assert.equal(web.status,'unreviewed');
  },page);
  if(!web||!disk)throw Error('Incident acceptance prerequisites failed');
  await check('Escalation requires a recorded response, reason and next step',async()=>{
    const bad=await request('health.incident.update',{incident_id:web.incident_id,expected_revision:0,status:'escalated'});
    assert.equal(bad.ok,false);assert.equal(bad.error.code,'params_invalid');assert.equal((await list('attention')).attention_count,0);
  },page);
  await check('Development and agent-handled conditions remain outside the owner queue',async()=>{
    const all=await list('all');const test=all.incidents.find(i=>i.category==='development');const cpu=all.incidents.find(i=>i.alert_key==='host/cpu');
    assert.ok(test); const bad=await request('health.incident.update',{incident_id:test.incident_id,expected_revision:0,status:'escalated',...response});assert.equal(bad.ok,false);
    await call('health.incident.update',{incident_id:test.incident_id,expected_revision:0,status:'suppressed',agent_response:'Expected temporary scratch space during the test run.'});
    await call('health.incident.update',{incident_id:cpu.incident_id,expected_revision:0,status:'handling',agent_response:'The agent is reducing the active build load.'});
    assert.equal((await list('attention')).attention_count,0);
  },page);
  await check('Agent escalation persists an explanation with exact occurrence identity',async()=>{
    web=await call('health.incident.update',{incident_id:web.incident_id,expected_revision:0,status:'escalated',summary:'Preview is unavailable',what_happened:'The web process exited. The preview does not respond.',...response});
    disk=await call('health.incident.update',{incident_id:disk.incident_id,expected_revision:0,status:'escalated',summary:'Host storage is nearly full',what_happened:'Available root storage is below 10%.',agent_response:'Inspected the measured storage categories. No files were removed.',escalation_reason:'The largest files are retained project data; their owner must decide what to keep.',next_step:'Review storage by repository before choosing files to remove.'});
    assert.equal((await list('attention')).attention_count,2);assert.equal(web.revision,1);
    const stale=await request('health.incident.update',{incident_id:web.incident_id,expected_revision:0,status:'dismissed'});assert.equal(stale.error.code,'configuration_conflict');
  },page);

  // All incident state below travels through the real authorized control
  // plane. Unrelated host metrics use the declared samples above.
  for(const theme of ['light','dark']) for(const width of [1440,760,390]) {
    await check(`Health ${theme} ${width}: collapsed arrival and selected explanation`,async()=>{
      await page.setViewportSize({width,height:width===390?844:1024});
      await page.goto(`${base}#/health`);
      await page.evaluate(theme=>{document.documentElement.dataset.theme=theme;localStorage.setItem('dc2-theme',theme);},theme);
      await page.locator('#hi-incidents-toggle').waitFor();
      if(await page.locator('#hi-incidents-toggle').getAttribute('aria-expanded')==='true')await page.locator('#hi-incidents-toggle').click();
      assert.equal(await page.locator('#hi-incident-panel').isVisible(),false);
      assert.equal(await page.locator('.hi-count').innerText(),'2');
      await page.screenshot({path:path.join(out,`health-${theme}-${width}-collapsed.png`),fullPage:true});
      await page.locator('#hi-incidents-toggle').click();
      await page.locator(`[data-hi-select="${web.incident_id}"]`).click();
      if(!await page.locator('#hi-selected-detail').isVisible())await page.locator(`[data-hi-select="${web.incident_id}"]`).click();
      const detail=page.locator('#hi-selected-detail');
      assert.equal(await detail.locator('.hi-answer').count(),4);
      for(const text of Object.values(response))assert.ok((await detail.innerText()).includes(text));
      assert.equal(await detail.getByRole('link',{name:'Open deployment',exact:false}).getAttribute('href'),'#/deployments/d1111111111111111');
      const layout=await page.evaluate(()=>{
        const d=document.querySelector('#hi-selected-detail');const row=d.closest('[data-hi-row]');
        return{inline:!!row,detailWidth:d.getBoundingClientRect().width,overflow:document.documentElement.scrollWidth-innerWidth,tiny:[...d.querySelectorAll('button,.btn')].some(e=>e.getBoundingClientRect().height<43)};
      });
      assert.equal(layout.inline,width<=760);assert.equal(layout.overflow,0);assert.equal(layout.tiny,false);assert.ok(layout.detailWidth>270);
      assert.ok(!await page.locator('.hi-incidents').innerText().then(t=>/test scratch|reducing the active build/.test(t)));
      await page.screenshot({path:path.join(out,`health-${theme}-${width}-expanded.png`),fullPage:true});
      await detail.locator('summary').click();assert.ok((await detail.innerText()).includes('Response recorded'));
    },page);
  }
  await check('Changing width preserves the same detail and keyboard focus',async()=>{
    const button=page.locator('[data-hi-disposition]');await button.focus();
    await page.setViewportSize({width:1440,height:1024});
    assert.equal(await page.locator('[data-hi-disposition]').evaluate(e=>e===document.activeElement),true);
    assert.equal(await page.locator('.hi-detail-host #hi-selected-detail').count(),1);
    await page.setViewportSize({width:390,height:844});
    assert.equal(await page.locator('[data-hi-row] #hi-selected-detail').count(),1);
  },page);
  await check('Search filters the inbox without replacing the focused control',async()=>{
    const field=page.locator('[data-hi-search]');await field.fill('unmatched condition');assert.equal(await page.locator('[data-hi-row]:visible').count(),0);assert.ok(await page.getByText('No incidents match your search.').isVisible());
    assert.equal(await field.evaluate(e=>e===document.activeElement),true);await field.fill('Kaizen');assert.equal(await page.locator('[data-hi-row]:visible').count(),1);await field.fill('');
  },page);
  await check('Dismiss is saved in SQLite and visible in a new browser context',async()=>{
    await page.locator(`[data-hi-select="${web.incident_id}"]`).click();
    if(!await page.locator('#hi-selected-detail').isVisible())await page.locator(`[data-hi-select="${web.incident_id}"]`).click();
    await page.locator('[data-hi-disposition]').click();await page.waitForFunction(()=>document.querySelector('.hi-count')?.textContent==='1');
    assert.equal((await list('attention')).attention_count,1);assert.equal((await list('dismissed')).incidents[0].incident_id,web.incident_id);
    const ctx=await browser.newContext({viewport:{width:390,height:844}});try{await ctx.addCookies([cookieFor('owner@example.test')]);const p=await ctx.newPage();await p.goto(`${base}#/health`);await p.locator('#hi-incidents-toggle').waitFor();assert.equal(await p.locator('.hi-count').innerText(),'1');await p.locator('[data-hi-dismissed]').click();await p.locator(`[data-hi-select="${web.incident_id}"]`).click();await p.locator('[data-hi-disposition="escalated"]').click();await p.waitForFunction(()=>document.querySelector('.hi-count')?.textContent==='2');}finally{await ctx.close();}
    assert.equal((await list('attention')).attention_count,2);
  },page);
  await check('Failure to persist never hides the incident or reports success',async()=>{
    await page.reload();await page.locator('#hi-incidents-toggle').click();await page.locator(`[data-hi-select="${web.incident_id}"]`).click();
    await page.route('**/api/v2/health.incident.update',route=>route.fulfill({status:409,contentType:'application/json',body:JSON.stringify({ok:false,error:{code:'configuration_conflict',message:'Incident changed; refresh'}})}));
    await page.locator('[data-hi-disposition]').click();await page.getByText('Could not save: Incident changed; refresh').waitFor();
    assert.equal((await list('attention')).attention_count,2);assert.equal(await page.locator('[data-hi-disposition]').isEnabled(),true);await page.unroute('**/api/v2/health.incident.update');
  },page);
  await check('Paging, recovery and recurrence keep exact incident identities',async()=>{
    const first=await call('health.incidents',{view:'attention',limit:1});assert.equal(first.incidents.length,1);assert.ok(first.next_before);const second=await call('health.incidents',{view:'attention',limit:1,before:first.next_before});assert.notEqual(first.incidents[0].incident_id,second.incidents[0].incident_id);
    await call('health.incident.update',{incident_id:disk.incident_id,expected_revision:disk.revision,status:'dismissed'});
    await call('fixture.incident.recur');const all=await list('all');const next=all.incidents.find(i=>i.alert_key==='host/disk'&&i.condition_active);assert.notEqual(next.incident_id,disk.incident_id);assert.equal(next.status,'unreviewed');assert.equal(all.incidents.find(i=>i.incident_id===disk.incident_id).status,'dismissed');
  },page);
  await check('Viewer cannot read or alter host incident dispositions',async()=>{
    const read=await request('health.incidents',{},'viewer@example.test');assert.equal(read.error.code,'permission_denied');const write=await request('health.incident.update',{incident_id:web.incident_id,expected_revision:3,status:'dismissed'},'viewer@example.test');assert.equal(write.error.code,'permission_denied');
  },page);
  await check('Host incident action leads to the real repository resource view',async()=>{
    const next=(await list('all')).incidents.find(i=>i.alert_key==='host/disk'&&i.condition_active);
    disk=await call('health.incident.update',{incident_id:next.incident_id,expected_revision:0,status:'escalated',...response});
    await page.reload();await page.locator('#hi-incidents-toggle').click();await page.locator(`[data-hi-select="${disk.incident_id}"]`).click();await page.locator('[data-hi-resources]').click();assert.equal(await page.locator('[data-hi-sort="storage_bytes"]').getAttribute('aria-pressed'),'true');assert.equal(await page.locator('#hi-repositories-heading').evaluate(e=>e===document.activeElement),true);
  },page);
  await check('Repository families contain generated checkouts and summarize repeat deployments',async()=>{
    const row=page.locator('.hi-repo-row').filter({hasText:'hdlripper'});assert.equal(await row.count(),1);assert.ok((await row.innerText()).includes('60 completed'));
    assert.equal(await page.locator('.hi-repo-name').filter({hasText:'0.1.7'}).count(),0);
    await row.locator('summary').click();assert.equal(await row.locator('.hi-checkout').count(),2);assert.equal(await row.locator('.hi-checkout li').count(),2);
    const field=page.locator('[data-hi-repo-search]');await field.fill('0.1.7');assert.equal(await page.locator('.hi-repo-row').count(),1);await field.fill('');
    await page.locator('.hi-attribution summary').click();assert.equal(await page.locator('.hi-storage-breakdown>div').count(),5);
  },page);
  if(process.env.HEALTH_FORMAL === '1') {
    for(const theme of ['light','dark']) await check(`Formal ${theme} incident journeys`,async()=>{
      const formalOut=path.join(out,`formal-${theme}`);await fs.mkdir(formalOut,{recursive:true});
      const target={name:`health-${theme}`,url:`${base}#/health`,theme,
        journeys:[{id:'health',name:'Inspect current health',frequencyPercent:80,risk:'normal'},{id:'incident',name:'Understand and act on an escalation',frequencyPercent:20,risk:'normal'}],primaryJourney:'health',
        regions:[{selector:'.hi-host',role:'primary-content',journey:'health'},{selector:'.hi-incidents',role:'supporting'}],
        reviewInputs:[{path:'console/health.js',kind:'ui-code'},{path:'console/health.css',kind:'style'},{path:'console/index.html',kind:'ui-code'},{path:'console/design-system.css',kind:'tokens'}],
        waitFor:{selector:'#hi-history svg',renderFrames:2},
        states:[{name:'expanded-incident',actions:[{action:'click',selector:'#hi-incidents-toggle'},{action:'click',selector:`[data-hi-select="${web.incident_id}"]`}],primaryJourney:'incident',priorityOverrideReason:'The owner selected an incident',regions:[{selector:'#hi-selected-detail',role:'primary-content',journey:'incident'}],continuation:{kind:'in-page',anchor:'#hi-selected-detail .hi-answer h3',focusWithin:'#hi-selected-detail',maxScrollDelta:8},waitFor:{selector:'#hi-selected-detail .hi-answer',renderFrames:2}}]};
      const viewports=[{name:'phone',width:390,height:844,colorScheme:theme},{name:'inline-boundary',width:760,height:1024,colorScheme:theme},{name:'pane-boundary',width:761,height:1024,colorScheme:theme},{name:'desktop',width:1440,height:1024,colorScheme:theme}];
      const config={repoRoot:root,playwrightModuleDir:'/home/DevCoordinator2/ci/playwright/node_modules',cookies:[`dc2_session=${cookieFor('owner@example.test').value}`],targets:[target],viewports,requiredCoverage:['base','expanded-incident'].flatMap(state=>viewports.map(v=>({target:target.name,state,viewport:v.name,width:v.width}))),maxPageCount:8};
      const input=path.join(formalOut,'input.private.json');await fs.writeFile(input,JSON.stringify(config),{mode:0o600});
      try {const result=await promisify(execFile)(process.execPath,[path.join(root,'skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs'),'--config',input,'--json-out',path.join(formalOut,'report.json'),'--markdown-out',path.join(formalOut,'report.md'),'--screenshot-dir',path.join(formalOut,'screenshots')],{cwd:root,maxBuffer:1024*1024});await fs.writeFile(path.join(formalOut,'receipt.json'),result.stdout);}finally{await fs.rm(input);}
    },page);
  }
  await fs.writeFile(path.join(out,'health-fixture-identities.json'),JSON.stringify({web:web.incident_id,disk:disk.incident_id}));
}
