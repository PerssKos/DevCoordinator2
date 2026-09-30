// Real two-server ticket journey using the existing isolated control-plane fixture.
// No requests are intercepted and no installed daemon or persistent state is used.
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import {spawn} from 'node:child_process';
import {createRequire} from 'node:module';
import {createEdge} from '../edge/devcoordinator2-edge.mjs';
import {canonicalJson} from '../edge/lib/routes-store.mjs';

const root=path.resolve(path.dirname(new URL(import.meta.url).pathname),'..');
const out=process.env.TICKET_VERIFY_OUT || '/tmp/dc2-ticket-journey';
const fixture=process.env.TICKET_FIXTURE_BINARY || '/home/DevCoordinator2/target/debug/examples/glossary_console_fixture';
const binary=process.env.TICKET_MCP_BINARY || '/home/DevCoordinator2/target/debug/devcoordinator2';
const {chromium}=createRequire('/opt/holyskills-validation-runtime/package.json')('playwright');
const checks=[],servers=[],mcps=[],errors=[];let browser,temporary,ticketId,origin,upstream;
await fs.mkdir(out,{recursive:true});
const log=async(name,value)=>fs.writeFile(path.join(out,name),typeof value==='string'?value:JSON.stringify(value,null,2));
async function check(name,work,page){const start=Date.now();try{await work();checks.push({name,status:'passed',duration_ms:Date.now()-start});}catch(error){const file=`failure-${checks.length}.log`;await log(file,error.stack||String(error));if(page)await page.screenshot({path:path.join(out,`${file}.png`),fullPage:true}).catch(()=>{});checks.push({name,status:'failed',evidence:file});}}

async function server(name){
  const directory=path.join(temporary,name);await fs.mkdir(directory);
  let child,sequence=0,offline=false;const pending=new Map();
  const start=()=>{
    child=spawn(fixture,[path.join(directory,'authority'),'tickets',`${name}.example`],{stdio:['pipe','pipe','pipe']});
    child.stderr.on('data',data=>fs.appendFile(path.join(out,`${name}-backend.log`),data));
    readline.createInterface({input:child.stdout}).on('line',line=>{let result;try{result=JSON.parse(line);}catch{return;}const callback=pending.get(result.id);pending.delete(result.id);callback?.resolve(result);});
    child.on('exit',()=>{for(const entry of pending.values())entry.reject(new Error('Fixture exited'));pending.clear();});
  };
  const request=(operation,params={},identity=null)=>new Promise((resolve,reject)=>{
    const id=`request-${++sequence}`;pending.set(id,{resolve,reject});child.stdin.write(JSON.stringify({protocol:2,id,operation,params,client:{kind:identity?'edge':'other',identity}})+'\n');
  });
  start();assert.equal((await request('ping')).ok,true);
  const socketPath=path.join(directory,'bridge.sock');
  const socketServer=net.createServer({allowHalfOpen:true},socket=>{
    if(offline){socket.destroy();return;}
    let buffer='';socket.on('error',()=>{});socket.on('data',data=>{buffer+=data;if(!buffer.endsWith('\n'))return;const input=JSON.parse(buffer);buffer='';request(input.operation,input.params,input.client.identity||null).then(result=>socket.end(JSON.stringify({...result,id:input.id})+'\n'),()=>socket.destroy());});
  });
  await new Promise(resolve=>socketServer.listen(socketPath,resolve));
  const payload={generation:1,published_at:new Date().toISOString(),domain:`${name}.example`,routes:[],access:{owners:['owner@example.test'],grants:[]}};
  const routesFile=path.join(directory,'routes.json');await fs.writeFile(routesFile,JSON.stringify({schema:1,payload_sha256:crypto.createHash('sha256').update(canonicalJson(payload)).digest('hex'),...payload}));
  const edge=await createEdge({baseDomain:`${name}.example`,consoleHost:'127.0.0.1',httpPort:0,httpOnly:true,sessionSecret:crypto.randomBytes(32).toString('hex'),oidcIssuer:'http://127.0.0.1:1',oidcClientId:'',oidcClientSecret:'',routesFile,stateDir:path.join(directory,'edge'),daemonSocket:socketPath,consoleDir:path.join(root,'console'),trustLocalConsole:true},{log:{warn(){},error(){},info(){},debug(){}}});
  const [port]=await edge.listen();const base=`http://127.0.0.1:${port}`;
  const value={base,socketPath,request,setOffline(value){offline=value;},async call(operation,params={},identity){const response=await request(operation,params,identity);assert.equal(response.ok,true,`${operation}: ${response.error?.message}`);return response.data;},async restart(){const stopped=new Promise(resolve=>child.once('exit',resolve));child.stdin.end();await stopped;start();assert.equal((await request('ping')).ok,true);},async stop(){await edge.close();socketServer.close();child.stdin.end();}};
  servers.push(value);return value;
}
async function mcp(server){
  const child=spawn(binary,['mcp'],{env:{...process.env,DEVCOORDINATOR2_SOCKET:server.socketPath},stdio:['pipe','pipe','pipe']});
  let seq=0;const pending=new Map();
  readline.createInterface({input:child.stdout}).on('line',line=>{const result=JSON.parse(line);const callback=pending.get(result.id);pending.delete(result.id);callback?.resolve(result);});
  const rpc=(method,params)=>new Promise((resolve,reject)=>{const id=++seq;pending.set(id,{resolve,reject});child.stdin.write(JSON.stringify({jsonrpc:'2.0',id,method,params})+'\n');});
  child.on('exit',()=>{for(const e of pending.values())e.reject(new Error('MCP exited'));pending.clear();});
  const ready=await rpc('initialize',{protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'codex',version:'1'}});assert.ok(!ready.error);
  child.stdin.write(JSON.stringify({jsonrpc:'2.0',method:'notifications/initialized'})+'\n');
  const client={rpc,async call(name,args){const result=await rpc('tools/call',{name,arguments:args});assert.ok(!result.error,JSON.stringify(result.error));assert.equal(result.result.isError,false,JSON.stringify(result.result.content));return result.result.structuredContent;},close(){child.stdin.end();}};mcps.push(client);return client;
}
const remote=async(server,action,credential=null,extra={})=>{
  const response=await fetch(server.base+'/.well-known/devcoordinator2/tickets',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({action,credential,server_label:credential?'Third server':null,...extra})});return response.json();
};
const action=(server,action,target)=>server.call('ticket.request',{target,action});
function storedZip(name,body){
  const n=Buffer.from(name),data=Buffer.from(body),local=Buffer.alloc(30),central=Buffer.alloc(46),end=Buffer.alloc(22);
  local.writeUInt32LE(0x04034b50);local.writeUInt16LE(20,4);local.writeUInt32LE(data.length,18);local.writeUInt32LE(data.length,22);local.writeUInt16LE(n.length,26);
  central.writeUInt32LE(0x02014b50);central.writeUInt16LE(20,4);central.writeUInt16LE(20,6);central.writeUInt32LE(data.length,20);central.writeUInt32LE(data.length,24);central.writeUInt16LE(n.length,28);
  end.writeUInt32LE(0x06054b50);end.writeUInt16LE(1,8);end.writeUInt16LE(1,10);end.writeUInt32LE(central.length+n.length,12);end.writeUInt32LE(local.length+n.length+data.length,16);
  return Buffer.concat([local,n,data,central,n,end]);
}
const png=Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a4GkAAAAASUVORK5CYII=','base64');
const uploadFiles=[{name:'backup-screen.png',mimeType:'image/png',buffer:png},{name:'requirements.txt',mimeType:'text/plain',buffer:Buffer.from('Preserve seven days of backups.')}];

try {
  temporary=await fs.mkdtemp(path.join(os.tmpdir(),'dc2-ticket-'));
  [origin,upstream]=await Promise.all([server('atlas'),server('upstream')]);
  browser=await chromium.launch();const context=await browser.newContext({viewport:{width:1487,height:1058}});const page=await context.newPage();page.on('pageerror',e=>errors.push(e.message));
  await check('configure the upstream through Console and preserve it on reload',async()=>{
    await page.goto(origin.base+'/#/requests');await page.getByRole('button',{name:'Upstream server',exact:true}).click();await page.locator('[name=upstream]').fill(upstream.base);await page.getByRole('button',{name:'Save setting',exact:true}).click();await page.waitForFunction(()=>!document.querySelector('dialog'));
    await page.reload();await page.getByRole('button',{name:'New request',exact:true}).waitFor();assert.equal((await origin.call('ticket.settings')).upstream,upstream.base);
  },page);
  await check('cancel and restore a request draft, then create with multiple real attachments',async()=>{
    uploadFiles[0].buffer=await page.screenshot();await page.getByRole('button',{name:'New request',exact:true}).click();await page.locator('dialog [name=title]').fill('Allow scheduled database backups');await page.locator('dialog [name=body]').fill('Let us choose backup times and retention.');await page.getByRole('button',{name:'Cancel',exact:true}).click();await page.getByRole('button',{name:'New request',exact:true}).click();assert.equal(await page.locator('dialog [name=title]').inputValue(),'Allow scheduled database backups');
    await page.locator('dialog input[type=file]').setInputFiles(uploadFiles);await page.waitForFunction(()=>[...document.querySelectorAll('dialog .ticket-pending small')].length===2&&[...document.querySelectorAll('dialog .ticket-pending small')].every(e=>e.textContent==='Attached'));
    await page.getByRole('button',{name:'Save request',exact:true}).click();await page.locator('.ticket-subject h2').waitFor();ticketId=new URLSearchParams(page.url().split('?')[1]).get('ticket');assert.ok(ticketId);
    const saved=await action(upstream,{kind:'get',ticket_id:ticketId},'local');assert.equal(saved.ticket.attachments.length,2);assert.equal(saved.ticket.body,'Let us choose backup times and retention.');
  },page);
  await check('each originating-server reply retains multiple attachments after reload',async()=>{
    await page.locator('#ticket-reply').fill('Here are the requested details.');await page.locator('.ticket-composer input[type=file]').setInputFiles(uploadFiles.map(f=>({...f,name:'reply-'+f.name})));await page.waitForFunction(()=>document.querySelectorAll('.ticket-composer .ticket-pending small').length===2&&[...document.querySelectorAll('.ticket-composer .ticket-pending small')].every(e=>e.textContent==='Attached'));
    await page.getByRole('button',{name:'Send reply',exact:true}).click();await page.locator('.ticket-comment').first().waitFor();await page.reload();await page.locator('.ticket-comment').first().waitFor();assert.equal(await page.locator('.ticket-comment .ticket-file').count(),2);
    await page.getByRole('button',{name:'View reply-requirements.txt',exact:true}).click();await page.waitForSelector('dialog pre');assert.equal(await page.locator('dialog pre').textContent(),'Preserve seven days of backups.');await page.getByRole('button',{name:'Close dialog',exact:true}).click();
    await page.getByRole('button',{name:'View reply-backup-screen.png',exact:true}).click();await page.waitForFunction(()=>document.querySelector('.ticket-preview-image')?.complete);assert.ok(await page.locator('.ticket-preview-image').evaluate(e=>e.naturalWidth>0));await page.getByRole('button',{name:'Close dialog',exact:true}).click();
  },page);
  const admin=await context.newPage();admin.on('pageerror',e=>errors.push(e.message));
  await check('upstream edits, replies with its own documents, and closes the same ticket',async()=>{
    await admin.goto(upstream.base+'/#/requests?view=received');await admin.locator(`[data-ticket-id="${ticketId}"]`).click();await admin.getByRole('button',{name:'Edit request',exact:true}).click();await admin.locator('dialog [name=body]').fill('Agreed: configurable backup times and retention.');await admin.getByRole('button',{name:'Save request',exact:true}).click();await admin.waitForFunction(()=>!document.querySelector('dialog'));
    await admin.locator('#ticket-reply').fill('Please review the proposal.');await admin.locator('.ticket-composer input[type=file]').setInputFiles([{name:'proposal.docx',mimeType:'application/vnd.openxmlformats-officedocument.wordprocessingml.document',buffer:storedZip('word/document.xml','<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Schedule and retention proposal</w:t></w:r></w:p></w:body></w:document>')},uploadFiles[1]]);await admin.waitForFunction(()=>document.querySelectorAll('.ticket-composer .ticket-pending small').length===2&&[...document.querySelectorAll('.ticket-composer .ticket-pending small')].every(e=>e.textContent==='Attached'));await admin.getByRole('button',{name:'Send reply',exact:true}).click();await admin.waitForFunction(()=>document.querySelectorAll('.ticket-comment').length===2);
    await admin.getByRole('button',{name:'View proposal.docx',exact:true}).click();await admin.waitForSelector('dialog pre');assert.match(await admin.locator('dialog pre').textContent(),/Schedule and retention proposal/);await admin.getByRole('button',{name:'Close dialog',exact:true}).click();
    await admin.locator('.ticket-menu summary').click();await admin.getByRole('button',{name:'Close ticket',exact:true}).click();await admin.waitForFunction(()=>document.querySelector('.ticket-controls .ticket-state')?.textContent==='Closed');await page.reload();await page.waitForFunction(()=>document.querySelector('.ticket-controls .ticket-state')?.textContent==='Closed');assert.equal(await page.locator('.ticket-comment .ticket-file').count(),4);
  },admin);
  await check('public readers see comments and files but cannot mutate, spoof authority or read other Console data',async()=>{
    const result=await remote(upstream,{kind:'get',ticket_id:ticketId});assert.equal(result.ok,true);assert.equal(result.data.can_manage,false);const ticket=result.data.ticket;
    const comments=await remote(upstream,{kind:'comments',ticket_id:ticketId,offset:0,limit:20});assert.equal(comments.data.items.length,2);
    const file=comments.data.items[0].attachments[0];const bytes=await remote(upstream,{kind:'file',ticket_id:ticketId,comment_id:comments.data.items[0].id,file_id:file.id,offset:0});assert.equal(bytes.ok,true);assert.equal(crypto.createHash('sha256').update(Buffer.from(bytes.data.data_base64,'base64')).digest('hex'),file.sha256);
    for(const credential of [null,'1'.repeat(64)]){
      const denied=await remote(upstream,{kind:'edit',ticket_id:ticketId,expected_revision:ticket.revision,title:'Unwanted change',body:'Rejected'},credential);assert.equal(denied.ok,false);assert.equal(denied.error.code,'permission_denied');
    }
    assert.equal((await remote(upstream,{kind:'get',ticket_id:ticketId},null,{administrator:true})).ok,false);
    const anon=await browser.newContext();const publicPage=await anon.newPage();await publicPage.goto(upstream.base+'/requests?ticket='+ticketId);await publicPage.locator('.ticket-subject h2').waitFor();assert.equal(await publicPage.locator('#ticket-edit,.ticket-composer,#ticket-new').count(),0);await publicPage.screenshot({path:path.join(out,'public-wide.png'),fullPage:true});await anon.close();
    const ordinary=await origin.request('ticket.settings',{},'viewer@example.test');assert.equal(ordinary.ok,true);assert.equal(ordinary.data.can_configure,false);const deniedSetting=await origin.request('ticket.configure',{upstream:'other.example',expected_revision:ordinary.data.revision},'viewer@example.test');assert.equal(deniedSetting.ok,false);
  },page);
  await check('real MCP clients on both servers create, discuss, close and discover public tickets',async()=>{
    const a=await mcp(origin),b=await mcp(upstream);const list=await a.rpc('tools/list',{});assert.ok(list.result.tools.some(t=>t.name==='ticket_request'));assert.ok(!list.result.tools.some(t=>t.name==='ticket_remote'));
    const created=await a.call('ticket_request',{action:{kind:'create',request_key:'mcp-request',title:'MCP feature request',body:'Created through the real MCP server.',attachments:[]}});
    const comment=await b.call('ticket_request',{target:'local',action:{kind:'comment',ticket_id:created.ticket.id,request_key:'mcp-reply',body:'Upstream agent reply.',attachments:[]}});assert.equal(comment.comment.body,'Upstream agent reply.');
    const refreshed=await a.call('ticket_request',{action:{kind:'get',ticket_id:created.ticket.id}});const closed=await a.call('ticket_request',{action:{kind:'close',ticket_id:created.ticket.id,expected_revision:refreshed.ticket.revision,closed:true}});assert.equal(closed.ticket.closed,true);
  },page);
  await check('network failure preserves a reply draft and retry saves it once',async()=>{
    await page.reload();await page.locator('#ticket-reply').waitFor();await page.locator('#ticket-reply').fill('Retry after the upstream reconnects.');upstream.setOffline(true);
    await page.getByRole('button',{name:'Send reply',exact:true}).click();await page.locator('.ticket-error:not([hidden])').waitFor();assert.equal(await page.locator('#ticket-reply').inputValue(),'Retry after the upstream reconnects.');
    upstream.setOffline(false);await page.getByRole('button',{name:'Send reply',exact:true}).click();await page.waitForFunction(()=>document.querySelectorAll('.ticket-comment').length===3);
    const comments=await action(origin,{kind:'comments',ticket_id:ticketId,offset:0,limit:20});assert.equal(comments.items.filter(c=>c.body==='Retry after the upstream reconnects.').length,1);assert.equal(await page.locator('.ticket-error').isVisible(),false);
  },page);
  await check('upload chunks resume safely and reject incomplete or foreign files',async()=>{
    const a=await mcp(origin);const bytes=Buffer.from('Attachment through MCP');const hash=crypto.createHash('sha256').update(bytes).digest('hex');
    const args={action:{kind:'upload_start',request_key:'mcp-upload',name:'agent.txt',byte_size:bytes.length,sha256:hash}};const upload=await a.call('ticket_request',args);
    const incomplete=await origin.request('ticket.request',{action:{kind:'comment',ticket_id:ticketId,request_key:'incomplete',body:'Must not appear',attachments:[upload.upload_id]}});assert.equal(incomplete.ok,false);
    const part={action:{kind:'upload_chunk',upload_id:upload.upload_id,offset:0,data_base64:bytes.toString('base64')}};assert.equal((await a.call('ticket_request',part)).offset,bytes.length);assert.equal((await a.call('ticket_request',part)).offset,bytes.length);
    const reply=await a.call('ticket_request',{action:{kind:'comment',ticket_id:ticketId,request_key:'mcp-attachment-comment',body:'Agent attachment',attachments:[upload.upload_id]}});assert.equal(reply.comment.attachments.length,1);
    const denied=await remote(upstream,{kind:'upload_chunk',upload_id:upload.upload_id,offset:bytes.length,data_base64:'YQ=='},'2'.repeat(64));assert.equal(denied.ok,false);
    const duplicate=await a.call('ticket_request',{action:{kind:'comment',ticket_id:ticketId,request_key:'mcp-attachment-comment',body:'Agent attachment',attachments:[upload.upload_id]}});assert.equal(duplicate.comment.id,reply.comment.id);
  },page);
  await check('restart both databases without losing ticket ownership, settings, replies or files',async()=>{
    await origin.restart();await upstream.restart();assert.equal((await origin.call('ticket.settings')).upstream,upstream.base);
    const result=await action(origin,{kind:'get',ticket_id:ticketId});assert.equal(result.ticket.closed,true);assert.equal(result.can_manage,true);assert.equal(result.ticket.attachments.length,2);const replies=await action(origin,{kind:'comments',ticket_id:ticketId,offset:0,limit:20});assert.equal(replies.items.length,4);assert.equal(replies.items[1].attachments.length,2);
  },page);
  await check('selected inbox remains usable in both themes and wide/narrow layouts',async()=>{
    await page.goto(origin.base+'/#/requests?ticket='+ticketId);await page.locator('.ticket-comment').first().waitFor();
    for(const theme of ['dark','light'])for(const width of [1487,741,740,390]){
      await page.setViewportSize({width,height:1058});await page.evaluate(theme=>document.documentElement.dataset.theme=theme,theme);await page.locator('.ticket-subject').waitFor();
      assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1),`${theme} ${width} horizontal overflow`);
      assert.ok(await page.locator('#ticket-reply').isVisible());await page.screenshot({path:path.join(out,`${theme}-${width}.png`),fullPage:true});
    }
    await page.setViewportSize({width:1487,height:1058});await page.evaluate(()=>document.documentElement.dataset.theme='dark');
    assert.deepEqual(errors,[]);
  },page);
  await check('long discussions reveal the newest reply and page backward without gaps',async()=>{
    for(let i=0;i<24;i++)await action(origin,{kind:'comment',ticket_id:ticketId,request_key:'long-thread-'+i,body:'Discussion item '+i,attachments:[]});
    await page.reload();await page.getByText('Discussion item 23',{exact:true}).waitFor();assert.equal(await page.locator('.ticket-comment').count(),20);
    await page.getByRole('button',{name:'Show earlier comments',exact:true}).click();await page.getByText('Here are the requested details.',{exact:true}).waitFor();assert.equal(await page.locator('.ticket-comment').count(),28);
    await page.locator('#ticket-reply').fill('Newest visible reply');await page.getByRole('button',{name:'Send reply',exact:true}).click();await page.getByText('Newest visible reply',{exact:true}).waitFor();
  },page);
  await check('PDF pages render without document scripts and page controls work',async()=>{
    const document=await context.newPage();await document.setContent('<h1>Backup proposal</h1><p>First page details.</p><div style="break-before:page"><h1>Retention rules</h1><p>Second page details.</p></div>');const pdf=await document.pdf();await document.close();
    await page.locator('#ticket-reply').fill('Review both PDF pages.');await page.locator('.ticket-composer input[type=file]').setInputFiles({name:'proposal.pdf',mimeType:'application/pdf',buffer:pdf});await page.waitForFunction(()=>document.querySelector('.ticket-pending small')?.textContent==='Attached');await page.getByRole('button',{name:'Send reply',exact:true}).click();await page.getByRole('button',{name:'View proposal.pdf',exact:true}).click();await page.locator('canvas[data-page="1"]').waitFor();assert.match(await page.locator('.ticket-pdf-text pre').textContent(),/First page details/);await page.getByRole('button',{name:'Next page',exact:true}).click();await page.locator('canvas[data-page="2"]').waitFor();assert.match(await page.locator('.ticket-pdf-text pre').textContent(),/Second page details/);await page.getByRole('button',{name:'Previous page',exact:true}).click();await page.locator('canvas[data-page="1"]').waitFor();
    await page.locator('.ticket-dialog').screenshot({path:path.join(out,'pdf-preview.png')});await page.getByRole('button',{name:'Close dialog',exact:true}).click();
  },page);
  if(process.env.TICKET_VERIFY_HOLD==='1'){
    await log('running.json',{origin:origin.base,upstream:upstream.base,ticket_id:ticketId});
    await new Promise(resolve=>process.once('SIGUSR1',resolve));
  }
  await check('changing the upstream keeps old tickets reachable; removal hides the ticket and its files',async()=>{
    const settings=await origin.call('ticket.settings');await origin.call('ticket.configure',{upstream:'local',expected_revision:settings.revision});const local=await action(origin,{kind:'create',request_key:'local-mode',title:'Local upstream',body:'This server owns the ticket.',attachments:[]});assert.equal(local.ticket.upstream,'https://atlas.example');
    const old=await action(origin,{kind:'get',ticket_id:ticketId},upstream.base);assert.equal(old.ticket.id,ticketId);
    await page.goto(origin.base+`/#/requests?ticket=${ticketId}&target=${encodeURIComponent(upstream.base)}`);await page.locator('.ticket-menu summary').click();await page.getByRole('button',{name:'Remove request',exact:true}).click();await page.waitForFunction(()=>!document.querySelector('.ticket-subject'));assert.equal((await remote(upstream,{kind:'get',ticket_id:ticketId})).ok,false);
  },page);
}catch(error){await log('fatal.log',error.stack||String(error));checks.push({name:'journey setup',status:'failed',evidence:'fatal.log'});}
finally{
  for(const client of mcps)client.close();await browser?.close();for(const server of servers)await server.stop();
  await log('report.json',{checks,page_errors:errors});console.log(JSON.stringify({checks:checks.length,passed:checks.filter(c=>c.status==='passed').length,failures:checks.filter(c=>c.status==='failed'),report:path.join(out,'report.json')}));
  process.exitCode=checks.some(c=>c.status==='failed')?1:0;
}
