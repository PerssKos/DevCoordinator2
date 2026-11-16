// Native, read-only post-install verification; execute through run-local.
// Default mode preserves the ticket journey. Explicit postgres-v3 mode requires
// CONSOLE_VERIFY_ACCEPTANCE + its pre-reviewed ACCEPTANCE_SHA256, CONFIG_SHA256,
// RUN_ID and TOOLING, in addition to the existing Console/source/output inputs.
// The schema-1 input and normalized receipt examples are exercised by
// verify-live-tickets.test.mjs. Refs are {path,sha256}; a reviewed sanitized copy
// also supplies original_sha256/original_bytes. Root-owned 0440 copies in a
// caller-group 0750 directory are supported; original private files stay intact.
// retirement_targets is the reviewed exact four-plus-eight list, never a scan.
// Retained summaries contain hashes/counts and allowlisted metadata only.
import fs from 'node:fs/promises';
import path from 'node:path';
import https from 'node:https';
import {createHash} from 'node:crypto';
import {createRequire} from 'node:module';
import {execFileSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {constants} from 'node:fs';

const mode=process.env.CONSOLE_VERIFY_MODE||'tickets';
// Complete formal reports can include the 60-cell journey evidence and are
// larger than the ordinary receipt bound. Keep the cap finite while allowing
// the real successful artifacts; queues stay smaller but need room for every
// changed cell's screenshot metadata.
const MAX_FORMAL_REPORT_BYTES=32*1024*1024;
const MAX_FORMAL_QUEUE_BYTES=8*1024*1024;
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
const digest=value=>/^[a-f0-9]{64}$/.test(value||'');
const fail=stage=>{throw Object.assign(new Error('Native acceptance proof refused'),{stage});};

// V3 consumes actual accepted receipts. It never runs a cleanup, fabricates run
// metadata, or treats a source fixture as live functional evidence.
async function verifyV3({root,out,source,base,request,check}){
  const accepted=[];
  const sealed=async(filename,label,max)=>{
    let file;
    try{
      file=await fs.open(filename,constants.O_RDONLY|constants.O_NOFOLLOW|constants.O_NONBLOCK);
      const stat=await file.stat();
      if(!stat.isFile()||stat.size>max||(stat.mode&0o007)!==0||(stat.mode&0o022)!==0)fail(label);
      const bytes=Buffer.alloc(max+1);let size=0;
      while(size<=max){const next=await file.read(bytes,size,bytes.length-size,null);if(!next.bytesRead)break;size+=next.bytesRead;}
      if(size>max||size!==stat.size)fail(label);
      return bytes.subarray(0,size);
    }catch{fail(label);}finally{await file?.close();}
  };
  const read=async(ref,label,max=2097152)=>{
    if(!ref||!path.isAbsolute(ref.path||'')||!digest(ref.sha256))fail(label);
    if(ref.original_sha256!==undefined&&(!digest(ref.original_sha256)||!Number.isSafeInteger(ref.original_bytes)||ref.original_bytes<1))fail(label);
    const bytes=await sealed(ref.path,label,max);
    try{
      if(hash(bytes)!==ref.sha256)fail(label);
      const value=JSON.parse(bytes);
      accepted.push({ref,label,bytes:bytes.length,max});
      return value;
    }catch{fail(label);}
  };
  const filename=process.env.CONSOLE_VERIFY_ACCEPTANCE;
  const acceptedSha=process.env.CONSOLE_VERIFY_ACCEPTANCE_SHA256;
  if(!path.isAbsolute(filename||'')||!digest(acceptedSha))fail('acceptance-input');
  const inputBytes=await sealed(filename,'acceptance-input',131072);
  if(hash(inputBytes)!==acceptedSha)fail('acceptance-input');
  let input;try{input=JSON.parse(inputBytes);}catch{fail('acceptance-input');}
  const configuration=process.env.CONSOLE_VERIFY_CONFIG_SHA256,run=process.env.DEVCOORDINATOR_RUN_ID;
  if(process.env.CONSOLE_VERIFY_RUN_ID!==undefined&&process.env.CONSOLE_VERIFY_RUN_ID!==run)fail('acceptance-binding');
  if(input.schema!==1||input.target!=='postgres-v3-refactor'||input.source_sha256!==source||!digest(configuration)||input.config_sha256!==configuration||input.run_id!==run||!/^[a-zA-Z0-9_-]{1,160}$/.test(run||'')||!/^[a-zA-Z0-9_-]{1,100}$/.test(input.repository_id||'')||!/^[a-f0-9]{40}$/.test(input.source_commit||'')||input.schema_version!==35)fail('acceptance-binding');
  const tooling=process.env.CONSOLE_VERIFY_TOOLING;
  if(!path.isAbsolute(tooling||''))fail('tooling-identity');
  try{if(execFileSync(tooling,['--source-commit'],{encoding:'utf8',timeout:10000,maxBuffer:65536,stdio:['ignore','pipe','pipe']}).trim()!==input.source_commit)fail('tooling-identity');}catch{fail('tooling-identity');}
  const original=name=>input.receipts[name].original_sha256||input.receipts[name].sha256;
  const summaries={};
  const receipt=async name=>read(input.receipts?.[name],name);
  const live=await receipt('live');
  const gates=['browser','edge-source','critical-journeys','critical-api','mcp-reconnect'];
  const fresh=value=>Number.isSafeInteger(value)&&value>0&&value<=Date.now();
  const binaryNames=['devcoordinator2','devcoordinator2-tooling','devcoordinator2-executor','devcoordinator2-migrate-authority'];
  if(live.schema!==1||live.result!=='passed'||live.source_commit!==input.source_commit||live.edge_source_commit!==input.source_commit||!fresh(live.checked_at_ms)||gates.some(g=>!live.checks?.includes(g))||binaryNames.some(name=>!digest(live.binaries?.[name]))||!Array.isArray(live.evidence)||!live.evidence.length)fail('live');
  summaries.live={schema:1,result:'passed',source_commit:live.source_commit,edge_source_commit:live.edge_source_commit,binaries:Object.fromEntries(binaryNames.map(name=>[name,live.binaries[name]])),checked_at_ms:live.checked_at_ms,checks:gates};
  if(!Array.isArray(input.formal)||!input.formal.length||input.formal.length>8)fail('formal');
  summaries.formal={schema:1,result:'passed',batches:[]};
  for(const [i,batch]of input.formal.entries()){
    const report=await read(batch.report,`formal-report-${i}`,MAX_FORMAL_REPORT_BYTES);
    await read(batch.queue,`formal-queue-${i}`,MAX_FORMAL_QUEUE_BYTES);
    await read(batch.review,`formal-review-${i}`,4194304);
    if(!Array.isArray(report.pages)||!report.pages.length||report.pages.some(page=>page.sourceBinding?.status!=='matched'||page.sourceBinding.observed!==`git:${input.source_commit}`||page.sourceBinding.expected!==`git:${input.source_commit}`))fail('formal-source');
    let reviewed;
    try{reviewed=JSON.parse(execFileSync(tooling,['formal-ui','review','--report',batch.report.path,'--queue',batch.queue.path,'--review',batch.review.path],{encoding:'utf8',timeout:30000,maxBuffer:65536,stdio:['ignore','pipe','pipe']}));}catch{fail('formal-manual');}
    if(reviewed.ok!==true||reviewed.manual?.result!=='passed'||reviewed.blockingCount!==0||reviewed.manual.requiredCells!==reviewed.manual.reviewedCells||reviewed.manual.requiredCells<1)fail('formal-manual');
    summaries.formal.batches.push({formal_run_id:reviewed.reviewedRunId,formal_ui_source_sha256:reviewed.manual.sourceSha256,formal_manifest_sha256:reviewed.manual.formalManifestSha256,candidate_id:reviewed.manual.candidateId,required_cells:reviewed.manual.requiredCells,reviewed_cells:reviewed.manual.reviewedCells,report_sha256:batch.report.sha256,manual_sha256:batch.review.sha256});
  }
  for(const name of ['critical-api','mcp']){
    const proof=await receipt(name);
    if(proof.schema!==1||proof.result!=='passed'||proof.source_commit!==input.source_commit||proof.protocol_version!==3||!fresh(proof.checked_at_ms)||proof.checked_at_ms>live.checked_at_ms||proof.unexpected_errors!==0)fail(name);
    if(name==='critical-api'){
      if(!Number.isSafeInteger(proof.checks)||proof.checks<1||!Number.isSafeInteger(proof.mutation_readbacks)||proof.mutation_readbacks<1)fail(name);
      summaries[name]={schema:1,result:'passed',source_commit:proof.source_commit,protocol_version:3,checked_at_ms:proof.checked_at_ms,checks:proof.checks,mutation_readbacks:proof.mutation_readbacks,unexpected_errors:0};
    }else{
      if(proof.reconnected!==true||proof.discovery!==true||!Number.isSafeInteger(proof.calls_succeeded)||proof.calls_succeeded<1)fail(name);
      summaries[name]={schema:1,result:'passed',source_commit:proof.source_commit,protocol_version:3,checked_at_ms:proof.checked_at_ms,reconnected:true,discovery:true,calls_succeeded:proof.calls_succeeded,unexpected_errors:0};
    }
  }
  const migration=await receipt('migration');
  if(migration.schema!==1||migration.result!=='passed'||!Number.isSafeInteger(migration.tables)||migration.tables<1||!Number.isSafeInteger(migration.rows)||migration.rows<0||!digest(migration.sqlite_source_sha256)||migration.all_tables_equal!==true||migration.restart_equal!==true||migration.foreign_keys_valid!==true||migration.identities_valid!==true||!/^[a-f0-9]{40}$/.test(migration.source_commit||''))fail('migration');
  summaries.migration=Object.fromEntries(['schema','result','source_commit','tables','rows','sqlite_source_sha256','all_tables_equal','restart_equal','foreign_keys_valid','identities_valid'].map(key=>[key,migration[key]]));
  const capabilities=await receipt('capabilities');
  if(capabilities.schema!==1||capabilities.result!=='passed'||capabilities.source_commit!==input.source_commit||capabilities.baseline_operations!==193||capabilities.preserved_operations!==193||capabilities.current_operations!==194||!digest(capabilities.baseline_catalog_sha256)||!digest(capabilities.current_catalog_sha256)||capabilities.authorization_unchanged!==true||capabilities.cli_matches_catalog!==true||capabilities.mcp_matches_catalog!==true||capabilities.live_discovery_verified!==true)fail('capabilities');
  const finalization=await receipt('finalization'),backup=await receipt('backup'),helper=await receipt('helper8');
  if(finalization.schema!==1||finalization.status!=='finalized'||finalization.source_commit!==input.source_commit||finalization.retired_sources!==4||finalization.live_acceptance_sha256!==original('live')||!digest(finalization.postgres_archive_sha256))fail('finalization');
  if(backup.verified!==true||backup.isolated_restore_removed!==true||backup.matched_frozen_authority!==false||backup.archive_sha256!==finalization.postgres_archive_sha256||!Number.isSafeInteger(backup.archive_bytes)||backup.archive_bytes<1)fail('backup');
  const gate=helper.gates;
  if(!digest(input.helper8_inventory_sha256)||!digest(input.helper8_binary_sha256)||helper.schema!==1||helper.status!=='retired'||helper.retired_sources!==8||helper.inventory_sha256!==input.helper8_inventory_sha256||gate?.source_commit!==input.source_commit||gate.finalization_sha256!==original('finalization')||gate.live_acceptance_sha256!==original('live')||gate.archive_sha256!==backup.archive_sha256||gate.verified_report_sha256!==original('backup')||['manifest_sha256','current_authority_sha256','postgres_config_sha256'].some(key=>!digest(gate[key]))||!path.isAbsolute(gate.finalized_transaction||''))fail('helper8');
  summaries.finalization={schema:1,status:'finalized',source_commit:finalization.source_commit,live_acceptance_sha256:finalization.live_acceptance_sha256,postgres_archive_sha256:finalization.postgres_archive_sha256,retired_sources:4};
  summaries.backup={verified:true,isolated_restore_removed:true,matched_frozen_authority:false,archive_sha256:backup.archive_sha256,archive_bytes:backup.archive_bytes};
  summaries.helper8={schema:1,status:'retired',inventory_sha256:helper.inventory_sha256,reviewed_binary_sha256:input.helper8_binary_sha256,retired_sources:8,gates:Object.fromEntries(['source_commit','manifest_sha256','finalization_sha256','live_acceptance_sha256','archive_sha256','verified_report_sha256','current_authority_sha256','postgres_config_sha256'].map(key=>[key,gate[key]]))};
  const targets=input.retirement_targets;
  if(!Array.isArray(targets)||targets.length!==12||new Set(targets.map(t=>t.path)).size!==12||targets.filter(t=>t.group==='finalizer').length!==4||targets.filter(t=>t.group==='helper8').length!==8)fail('retirement-targets');
  const absent=async target=>{
    if(!path.isAbsolute(target.path||'')||path.normalize(target.path)!==target.path||target.path.endsWith(path.sep)||target.path.split(path.sep).some(part=>part==='..'||part==='.')||!digest(target.sha256))fail('retirement-targets');
    let prefix=path.parse(target.path).root;
    for(const part of target.path.slice(prefix.length).split(path.sep)){
      prefix=path.join(prefix,part);
      try{const stat=await fs.lstat(prefix);if(stat.isSymbolicLink()||!stat.isDirectory())fail('retirement-absence');}catch(error){if(error.code==='ENOENT')return;throw error;}
    }
    fail('retirement-absence');
  };
  for(const target of targets)await absent(target);
  summaries.retirement={schema:1,result:'passed',absent_sources:12,targets:targets.map(t=>({group:t.group,path_sha256:hash(t.path),former_sha256:t.sha256}))};
  const completion=await read(input.completion,'completion',32768);
  const known=(value,keys)=>value&&typeof value==='object'&&!Array.isArray(value)&&Object.keys(value).every(key=>keys.includes(key));
  if(!known(completion,['schema_version','claim','source_sha256','config_sha256','capabilities'])||completion.schema_version!==1||completion.claim!=='complete'||completion.source_sha256!==source||completion.config_sha256!==configuration||!Array.isArray(completion.capabilities)||!completion.capabilities.length||completion.capabilities.length>512)fail('completion');
  const ids=new Set();
  const covered=new Set();
  for(const capability of completion.capabilities){
    if(!known(capability,['id','scope','state','expected_result','task_id','evidence_refs','enabled_control','rendered_evidence_refs'])||!/^[a-zA-Z0-9_.-]{1,128}$/.test(capability.id||'')||ids.has(capability.id)||!['product','test_only'].includes(capability.scope)||!['real_e2e','fixture_only','visual_only','external_blocked','deferred'].includes(capability.state)||typeof capability.expected_result!=='string'||capability.expected_result.length<3||capability.expected_result.length>2000||(capability.enabled_control!==undefined&&typeof capability.enabled_control!=='boolean'))fail('completion');
    for(const key of ['evidence_refs','rendered_evidence_refs'])if(capability[key]!==undefined&&(!Array.isArray(capability[key])||capability[key].length>32||capability[key].some(ref=>typeof ref!=='string'||!ref.length||ref.length>512||/[\r\n]/.test(ref))))fail('completion');
    ids.add(capability.id);
    if(capability.scope==='product'&&capability.state!=='real_e2e')fail('completion');
    const runtimeRef=ref=>typeof ref==='string'&&ref.startsWith(`run/${run}/evidence/`)&&Object.hasOwn(summaries,ref.slice(`run/${run}/evidence/`.length).replace(/\.json$/,''))&&ref.endsWith('.json');
    if(capability.scope==='product'&&(!Array.isArray(capability.evidence_refs)||!capability.evidence_refs.some(runtimeRef)))fail('completion');
    if(capability.scope==='product')for(const ref of capability.evidence_refs)if(runtimeRef(ref))covered.add(ref.slice(`run/${run}/evidence/`.length,-5));
    if(capability.enabled_control&&(capability.scope!=='product'||!Array.isArray(capability.rendered_evidence_refs)||!capability.rendered_evidence_refs.includes(`run/${run}/evidence/formal.json`)))fail('completion');
  }
  if(Object.keys(summaries).some(name=>!covered.has(name)))fail('completion');
  // Static inventory and isolated test receipts support the live observations;
  // neither can serve as a product capability's sole real_e2e reference.
  summaries.capabilities={schema:1,scope:'source_catalog_and_live_discovery',result:'passed',source_commit:capabilities.source_commit,...Object.fromEntries(['baseline_operations','preserved_operations','current_operations','baseline_catalog_sha256','current_catalog_sha256','authorization_unchanged','cli_matches_catalog','mcp_matches_catalog','live_discovery_verified'].map(key=>[key,capabilities[key]]))};
  if(input.supporting!==undefined){
    if(!Array.isArray(input.supporting)||input.supporting.length>32)fail('supporting');
    const evidence=[],supportIds=new Set();
    for(const item of input.supporting){
      if(!/^[a-zA-Z0-9_-]{1,80}$/.test(item.id||'')||supportIds.has(item.id)||!['source-test','isolated-runtime','source-size-audit','visual-audit'].includes(item.kind)||!/^[a-f0-9]{40}$/.test(item.source_commit||''))fail('supporting');
      supportIds.add(item.id);await read(item.receipt,`support-${item.id}`);
      evidence.push({id:item.id,kind:item.kind,source_commit:item.source_commit,sha256:item.receipt.sha256,original_sha256:item.receipt.original_sha256||item.receipt.sha256});
    }
    summaries.supporting={schema:1,scope:'supporting_only',evidence};
  }
  for(const [operation,params]of [['ping',{}],['plan.overview',{repository_id:input.repository_id,limit:1}],['decision.tail',{repository_id:input.repository_id,n:1}],['test.admission.status',{}],['health.summary',{}],['storage.inventory',{repository_id:input.repository_id,limit:1}]]){
    const response=await request(`/api/v3/${operation}`,JSON.stringify(params));let reply;try{reply=JSON.parse(response.body);}catch{fail('native-api');}
    check(`read-only v3 ${operation}`,response.status===200&&reply.protocol===3&&reply.ok===true);
    if(operation==='ping')check('exact v3 source and schema',reply.data?.protocol_version===3&&reply.data.schema_version===input.schema_version&&reply.data.source_commit===input.source_commit);
  }
  const observed=await observeNative({root,out,source,base,request,check,target:'postgres-v3-refactor',expectedCommit:input.source_commit});
  // Detect input changes before sealing bounded, caller-readable summaries.
  if(!inputBytes.equals(await sealed(filename,'acceptance-changed',131072)))fail('acceptance-changed');
  for(const item of accepted){const bytes=await sealed(item.ref.path,'acceptance-changed',item.max);if(hash(bytes)!==item.ref.sha256)fail('acceptance-changed');}
  for(const target of targets)await absent(target);
  await fs.mkdir(path.join(out,'evidence'),{mode:0o700});
  const retained=[];
  for(const [name,value]of Object.entries(summaries)){const bytes=Buffer.from(JSON.stringify(value));await fs.writeFile(path.join(out,'evidence',`${name}.json`),bytes,{mode:0o600,flag:'wx'});retained.push({file:`evidence/${name}.json`,sha256:hash(bytes),bytes:bytes.length});}
  const completionBytes=await sealed(input.completion.path,'acceptance-changed',32768);
  if(hash(completionBytes)!==input.completion.sha256)fail('acceptance-changed');
  await fs.writeFile(path.join(out,'completion.json'),completionBytes,{mode:0o600,flag:'wx'});
  await fs.writeFile(path.join(out,'delivery.json'),JSON.stringify(observed),{mode:0o600,flag:'wx'});
  for(const file of ['response.html','completion.json','delivery.json']){const bytes=await fs.readFile(path.join(out,file));retained.push({file,sha256:hash(bytes),bytes:bytes.length});}
  await fs.writeFile(path.join(out,'evidence-index.json'),JSON.stringify({schema:1,target:'postgres-v3-refactor',repository_id:input.repository_id,source_commit:input.source_commit,governed_source_sha256:source,governed_config_sha256:configuration,run_id:run,acceptance_sha256:hash(inputBytes),inputs:accepted.map(({ref,label,bytes})=>({id:label,sha256:ref.sha256,bytes,original_sha256:ref.original_sha256||ref.sha256,original_bytes:ref.original_bytes||bytes})),retained,formal_digest_scope:'Declared UI inputs and observed bindings; distinct from governed repository source.'}),{mode:0o600,flag:'wx'});
}

async function observeNative({root,out,source,base,request,check,target,expectedCommit}){
  const html=await request('/'),pingReply=await request('/api/v3/ping','{}'),ping=JSON.parse(pingReply.body);
  check('native Console HTML is available',html.status===200&&html.type?.startsWith('text/html'));
  check('installed daemon commit is identified',ping.ok&&/^[a-f0-9]{40}$/.test(ping.data.source_commit)&&(!expectedCommit||ping.data.source_commit===expectedCommit));
  const paths=execFileSync('git',['ls-files','-z','--','console'],{cwd:root,maxBuffer:1048576}).toString().split('\0').filter(file=>file&&!path.basename(file).startsWith('.')&&/\.(html|css|js|mjs|svg|woff2|ttf|pfb|bcmap|wasm|png|ico|json|txt)$/i.test(file)).sort();
  const assets=createHash('sha256').update('devcoordinator2-console-assets-v1\0');
  for(const file of paths){const response=await request('/'+file.slice(8).split('/').map(encodeURIComponent).join('/'));check(`served asset ${file}`,response.status===200&&response.body.equals(await fs.readFile(path.join(root,file))));assets.update(file).update('\0').update(String(response.body.length)).update('\0').update(hash(response.body)).update('\0');}
  await fs.writeFile(path.join(out,'response.html'),html.body,{mode:0o600,flag:'wx'});
  return {version:1,kind:'native-console',target,source_sha256:source,file:'response.html',observed_sha256:hash(html.body),checked_at_ms:html.checked,access:base.href,observation:'web_route_passed',native_console:{daemon_source_commit:ping.data.source_commit,assets_sha256:assets.digest('hex'),http_status:html.status,content_type:html.type}};
}

async function main(){
if(!['tickets','postgres-v3'].includes(mode))fail('mode');

const root=fileURLToPath(new URL('../',import.meta.url));
const base=new URL(process.env.CONSOLE_VERIFY_BASE_URL);
const source=process.env.CONSOLE_VERIFY_SOURCE_SHA256;
const relative=process.env.CONSOLE_VERIFY_OUT;
if(base.protocol!=='https:'||base.pathname!=='/'||base.search||base.hash||base.username||base.password)throw new Error('An exact HTTPS Console origin is required');
if(!/^[a-f0-9]{64}$/.test(source||'')||!process.env.DEVCOORDINATOR_EVIDENCE_DIR)throw new Error('Use the native executor with its accepted source digest');
if(!/^target\/native-console-[a-zA-Z0-9-]+$/.test(relative||''))throw new Error('Use a new native Console evidence directory');
const out=path.join(root,relative);await fs.mkdir(out,{recursive:false,mode:0o700});
const agent=new https.Agent({keepAlive:true});
const request=(pathname,body)=>new Promise((resolve,reject)=>{
  const req=https.request({agent,hostname:'127.0.0.1',servername:base.hostname,port:base.port||443,path:pathname,method:body?'POST':'GET',headers:{Host:base.host,...(body?{'Content-Type':'application/json'}:{})}},res=>{
    const parts=[];let size=0;res.on('data',chunk=>{size+=chunk.length;if(size>2097152){res.destroy(new Error('Response exceeds bound'));return;}parts.push(chunk);});res.on('error',reject);res.on('end',()=>resolve({status:res.statusCode,type:res.headers['content-type'],body:Buffer.concat(parts),checked:Date.now()}));
  });req.setTimeout(10000,()=>req.destroy(new Error('Response deadline exceeded')));req.on('error',reject);req.end(body);
});
const checks=[];const check=(name,pass)=>{checks.push({name,passed:!!pass});if(!pass)throw new Error(name);};
try{
if(mode==='postgres-v3'){
  await verifyV3({root,out,source,base,request,check});
}else{
const {chromium}=createRequire(path.join(process.env.CONSOLE_VERIFY_PLAYWRIGHT||path.join(root,'ci/playwright'),'package.json'))('playwright');
const browser=await chromium.launch({args:[`--host-resolver-rules=MAP ${base.hostname} 127.0.0.1`]});
try{
  const page=await browser.newPage({viewport:{width:1487,height:1058}});const errors=[];page.on('pageerror',error=>errors.push(error.name));
  await page.goto(base.href+'#/requests?view=received');await page.locator('#ticket-new').waitFor();
  check('received inbox and new request are available',await page.locator('.ticket-inbox').isVisible());
  await page.screenshot({path:path.join(out,'inbox-wide.png'),fullPage:true});
  await page.getByRole('button',{name:'New request',exact:true}).click();await page.locator('.ticket-dialog input[name=title]').waitFor();
  check('new request begins focused in view',await page.locator('.ticket-dialog input[name=title]').evaluate(e=>e===document.activeElement&&e.getBoundingClientRect().top<innerHeight));
  await page.locator('.ticket-dialog input[name=title]').fill('Unsaved ticket verification draft');await page.locator('.ticket-dialog').screenshot({path:path.join(out,'new-request.png')});await page.getByRole('button',{name:'Cancel',exact:true}).click();
  await page.setViewportSize({width:390,height:844});await page.screenshot({path:path.join(out,'inbox-narrow.png'),fullPage:true});check('phone inbox has no horizontal overflow',await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1));
  await page.goto(base.href+'requests');await page.locator('.ticket-inbox').waitFor();check('public page has no write controls',await page.locator('#ticket-new,.ticket-composer').count()===0);await page.screenshot({path:path.join(out,'public-narrow.png'),fullPage:true});check('no browser exceptions',errors.length===0);
  const publicReply=await request('/.well-known/devcoordinator2/tickets',JSON.stringify({action:{kind:'list',offset:0,limit:1,closed:null,mine:false},credential:null,server_label:null}));check('public ticket endpoint responds without sign-in',publicReply.status===200&&JSON.parse(publicReply.body).ok===true);
  await fs.writeFile(path.join(out,'delivery.json'),JSON.stringify(await observeNative({root,out,source,base,request,check,target:'upstream-feature-requests'})));
  if(process.env.CONSOLE_VERIFY_COMPLETION)await fs.copyFile(process.env.CONSOLE_VERIFY_COMPLETION,path.join(out,'completion.json'));
}finally{await browser.close();}
}
await fs.writeFile(path.join(out,'journey.json'),JSON.stringify({checks,checked_at_ms:Date.now()},null,2),{mode:0o600,flag:'wx'});console.log(JSON.stringify({checks:checks.length,failures:0}));
}finally{agent.destroy();}
}
main().catch(error=>{if(mode==='postgres-v3')console.error(JSON.stringify({error:'v3_acceptance_refused',stage:error.stage||'native-observation'}));else console.error(error);process.exitCode=1;});
