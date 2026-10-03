import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {createHash} from 'node:crypto';

export async function verifySketchHistoryLayout({base,repository_id,surface_id,sketch_id,root,out,check,browser}) {
  const geometry=(primary,heading)=>[
    {id:'content-width',kind:'primary-content-width',selector:primary,minWidth:280},
    {id:'heading',kind:'readable-heading',selector:heading},
    {id:'surface-name',kind:'readable-canonical-identifier',selector:heading},
    {id:'word-wrapping',kind:'no-character-wrapping',selector:heading},
    {id:'page-width',kind:'document-horizontal-overflow',selector:'html'},
    {id:'content-arrival',kind:'initial-viewport-placement',selector:primary},
    {id:'content-clipping',kind:'clipping',selector:primary},
  ];
  const shapes=(id,selector)=>[{id,revision:'mockup-history-v2',conditionalDom:[selector],layoutEffect:'Real persisted mockups and selection records populate this review state.'}];
  const reviewInputs=['console/sketches.js','console/app.js','console/bootstrap.mjs','console/index.html','console/sketches.css','console/app.css','console/design-system.css','console/locales/en/sketches.json'].map(file=>({path:file,kind:file.endsWith('.css')?'style':'ui-code'}));
  // Bind the real served index and verify each declared UI input's bytes.
  const probe=await browser.newPage();let sourceBinding;
  try {
    const response=await probe.goto(base);assert.equal(response.status(),200);
    assert.ok((await response.body()).equals(await fs.readFile(path.join(root,'console/index.html'))));
    sourceBinding={expected:response.headers().etag,responseHeader:'etag',metaName:null};
    const observed=[];
    for(const file of reviewInputs){const response=await probe.request.get(new URL(file.path.slice('console/'.length),base).href);const bytes=await response.body();assert.equal(response.status(),200);assert.ok(bytes.equals(await fs.readFile(path.join(root,file.path))),file.path);observed.push({path:file.path,sha256:createHash('sha256').update(bytes).digest('hex')});}
    await fs.writeFile(path.join(out,'served-ui-inputs.json'),JSON.stringify({sourceBinding,files:observed},null,2));
  }finally{await probe.close();}
  for(const theme of ['light','dark']) await check(`Formal mockup history ${theme}`,async()=>{
    const directory=path.join(out,`formal-${theme}`);await fs.mkdir(directory,{recursive:true});
    const common={theme,reviewInputs,sourceBinding,journeys:[{id:'review-mockups',name:'Find and continue the selected mockup directions',frequencyPercent:100,risk:'normal'}],primaryJourney:'review-mockups',execution:{parallelSafe:true}};
    const region=selector=>[{selector,role:'primary-content',journey:'review-mockups'}];
    const continuation=anchor=>({kind:'in-page',anchor,maxScrollDelta:8});
    const targets=[{
      ...common,name:`gallery-${theme}`,url:`${base}#/sketches/${repository_id}`,
      regions:region('[data-collection]'),waitFor:{selector:'[data-review-set]',renderFrames:2},
      geometryAssertions:geometry('[data-collection]','.mockup-heading h1'),fixtureDataShapes:shapes('gallery-first-page-current-and-history','.mockup-surface'),
      states:[{name:'filters',actions:[{action:'click',selector:'[data-search] summary'}],continuation:{...continuation('.mockup-filters input'),focusWithin:'.mockup-filters'},waitFor:{selector:'.mockup-filters',renderFrames:2}}],
    },{
      ...common,name:`story-${theme}`,url:`${base}#/sketches/${repository_id}?surface=${surface_id}&sketch=${sketch_id}`,
      regions:region('.mockup-preview'),waitFor:{selector:'[data-main-mockup][data-loaded=true]',renderFrames:2},
      geometryAssertions:geometry('.mockup-preview','.mockup-heading h1'),fixtureDataShapes:shapes('selected-branches-with-agent-context','[data-option] input:checked'),
      states:[
        {name:'scope',actions:[{action:'click',selector:'.mockup-scope>summary'}],continuation:{...continuation('.mockup-scope>summary'),focusWithin:'.mockup-scope'},regions:region('.mockup-scope'),geometryAssertions:geometry('.mockup-scope','.mockup-scope>summary'),fixtureDataShapes:shapes('expanded-scope','.mockup-scope[open]')},
        {name:'context',actions:[{action:'click',selector:'.mockup-context>details:not(.mockup-initial-details)>summary'}],continuation:{...continuation('.mockup-context>details:not(.mockup-initial-details)>summary'),focusWithin:'.mockup-context'},regions:region('.mockup-context'),geometryAssertions:geometry('.mockup-context','.mockup-context h2'),fixtureDataShapes:shapes('context-revisions','[data-context-revisions] li')},
        {name:'comments',actions:[{action:'click',selector:'.mockup-comments>summary'}],continuation:{...continuation('.mockup-comments>summary'),focusWithin:'.mockup-comments'},regions:region('.mockup-comments'),geometryAssertions:geometry('.mockup-comments','.mockup-comments>summary'),fixtureDataShapes:shapes('comments-and-composer','[data-comment]')},
        {name:'editor',actions:[{action:'click',selector:'[data-edit-context]'}],continuation:{...continuation('dialog.mockup-editor[open]>form>header>h2'),focusWithin:'dialog.mockup-editor[open]'},regions:region('dialog.mockup-editor[open]'),geometryAssertions:geometry('dialog.mockup-editor[open]','dialog.mockup-editor[open]>form>header>h2'),fixtureDataShapes:shapes('context-editor','dialog.mockup-editor[open][open]'),waitFor:{selector:'dialog.mockup-editor[open]',renderFrames:2}},
      ],
    }];
    targets.push({...common,name:`empty-search-${theme}`,url:`${base}#/sketches/${repository_id}?q=no-such-mockup-result`,regions:region('[data-collection]'),waitFor:{selector:'[data-collection] .notice',renderFrames:2},geometryAssertions:geometry('[data-collection]','.mockup-heading h1'),fixtureDataShapes:shapes('empty-search-with-existing-archive','[data-collection] .notice'),states:[]});
    const viewports=[{name:'phone',width:390,height:844,colorScheme:theme},{name:'intermediate',width:927,height:1058,colorScheme:theme},{name:'desktop',width:1280,height:1058,colorScheme:theme},{name:'wide',width:1487,height:1058,colorScheme:theme}];
    const fixtureDataShapes=targets.flatMap(target=>[{name:'base',fixtureDataShapes:target.fixtureDataShapes},...target.states].flatMap(state=>(state.fixtureDataShapes||target.fixtureDataShapes).map(shape=>({...shape,id:target.name+'-'+state.name+'-'+shape.id,target:target.name,route:new URL(target.url).pathname+new URL(target.url).search,state:state.name}))));
    const config={fixtureDataShapes,repoRoot:root,playwrightModuleDir:path.join(root,'ci/playwright/node_modules'),targets,viewports,maxPageCount:32,execution:{maxConcurrency:1},requiredCoverage:targets.flatMap(target=>['base',...target.states.map(state=>state.name)].flatMap(state=>viewports.map(viewport=>({target:target.name,state,viewport:viewport.name,width:viewport.width})))),screenshotMasks:[{selector:'#who-email',reason:'Account identity is private'},{selector:'#workspace-root',reason:'Local repository path is private'}]};
    const input=path.join(directory,'config.json');await fs.writeFile(input,JSON.stringify(config,null,2));
    let result;
    try {result=await promisify(execFile)(process.execPath,[path.join(root,'skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs'),'--config',input,'--json-out',path.join(directory,'report.json'),'--markdown-out',path.join(directory,'report.md'),'--screenshot-dir',path.join(directory,'screenshots')],{cwd:root,maxBuffer:1024*1024});}
    catch(error){await fs.writeFile(path.join(directory,'receipt.json'),error.stdout||'');throw error;}
    await fs.writeFile(path.join(directory,'receipt.json'),result.stdout);
    const report=JSON.parse(await fs.readFile(path.join(directory,'report.json'),'utf8'));
    assert.equal(report.formal.result,'passed',JSON.stringify(report.formal));
  });
}
