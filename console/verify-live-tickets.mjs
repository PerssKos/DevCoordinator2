// Native, read-only post-install verification; execute through run-local.
import fs from 'node:fs/promises';
import path from 'node:path';
import https from 'node:https';
import {createHash} from 'node:crypto';
import {createRequire} from 'node:module';
import {execFileSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';

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
const {chromium}=createRequire(path.join(process.env.CONSOLE_VERIFY_PLAYWRIGHT||path.join(root,'ci/playwright'),'package.json'))('playwright');
const browser=await chromium.launch({args:[`--host-resolver-rules=MAP ${base.hostname} 127.0.0.1`]});
const checks=[];const check=(name,pass)=>{checks.push({name,passed:!!pass});if(!pass)throw new Error(name);};
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
  const html=await request('/'),pingReply=await request('/api/v2/ping','{}'),ping=JSON.parse(pingReply.body);check('native Console HTML is available',html.status===200&&html.type?.startsWith('text/html'));check('installed daemon commit is identified',ping.ok&&/^[a-f0-9]{40}$/.test(ping.data.source_commit));
  const paths=execFileSync('git',['ls-files','-z','--','console'],{cwd:root,maxBuffer:1048576}).toString().split('\0').filter(file=>file&&!path.basename(file).startsWith('.')&&/\.(html|css|js|mjs|svg|woff2|ttf|pfb|bcmap|wasm|png|ico|json|txt)$/i.test(file)).sort();
  const assets=createHash('sha256').update('devcoordinator2-console-assets-v1\0');
  for(const file of paths){const response=await request('/'+file.slice(8).split('/').map(encodeURIComponent).join('/'));check(`served asset ${file}`,response.status===200&&response.body.equals(await fs.readFile(path.join(root,file))));assets.update(file).update('\0').update(String(response.body.length)).update('\0').update(createHash('sha256').update(response.body).digest('hex')).update('\0');}
  await fs.writeFile(path.join(out,'response.html'),html.body);
  await fs.writeFile(path.join(out,'delivery.json'),JSON.stringify({version:1,kind:'native-console',target:'upstream-feature-requests',source_sha256:source,file:'response.html',observed_sha256:createHash('sha256').update(html.body).digest('hex'),checked_at_ms:html.checked,access:base.href,observation:'web_route_passed',native_console:{daemon_source_commit:ping.data.source_commit,assets_sha256:assets.digest('hex'),http_status:html.status,content_type:html.type}}));
  if(process.env.CONSOLE_VERIFY_COMPLETION)await fs.copyFile(process.env.CONSOLE_VERIFY_COMPLETION,path.join(out,'completion.json'));
  await fs.writeFile(path.join(out,'journey.json'),JSON.stringify({checks,checked_at_ms:Date.now()},null,2));console.log(JSON.stringify({checks:checks.length,failures:0}));
}finally{await browser.close();agent.destroy();}
