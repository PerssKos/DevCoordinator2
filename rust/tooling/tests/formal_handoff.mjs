import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { spawn, execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { access, mkdir, mkdtemp, readFile, writeFile, symlink, truncate, readdir } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import { createRequire } from 'node:module';
import { captureEvidenceScreenshot } from '../../../skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs';
import { formalDecision } from '../../../skills/formal-web-ui-verification/scripts/formal_handoff_contract.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const scratch = await mkdtemp(join(process.env.FORMAL_WEB_UI_HANDOFF_SCRATCH ?? process.env.TMPDIR ?? tmpdir(), 'formal-handoff-'));
const html = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><style>body{margin:0;color:#111;background:white;font:16px system-ui}main{padding:20px;grid-column:2;min-width:0}#editor{position:fixed;left:20px;top:120px;background:white;padding:12px;border:1px solid #555}#editor[hidden]{display:none}input{width:220px;max-width:100%;box-sizing:border-box}#editor-label{display:grid;gap:6px}button{min-height:32px}h1{font-size:24px}#layout{display:grid;grid-template-columns:0px minmax(0,1fr)}#nav{display:none}</style></head><body><div id="layout"><nav id="nav"></nav><main id="primary"><h1 id="heading">HDL workspace</h1><button id="label">Edit signal</button><form id="editor" role="dialog" data-ui-contextual-overlay="Inline declaration editor" hidden><h2 id="editor-heading">Edit signal</h2><label id="editor-label">Declaration<input id="declaration"></label><button type="button" id="cancel">Cancel</button></form></main></div><script>document.querySelector('#label').addEventListener('dblclick',()=>{document.querySelector('#editor').hidden=false;document.querySelector('#declaration').focus()});document.querySelector('#cancel').onclick=()=>{document.querySelector('#editor').hidden=true;document.querySelector('#label').focus()};</script></body></html>`;
const revision = createHash('sha256').update(await readFile(fileURLToPath(import.meta.url))).digest('hex');
const captureHtml = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><style nonce="fixture">body{margin:0;background:white;color:#008888;font:16px sans-serif;height:1302px}#private{position:absolute;left:12px;top:472px;width:366px;height:20px;color:#de1d48}input{color:#de1d48;border:1px solid #222;background:white}#declaration{position:absolute;left:12px;top:584px;width:340px;height:40px}#public{position:absolute;left:12px;top:524px}#echo{position:absolute;left:12px;top:1000px;width:300px;height:20px;color:#de1d48}#shadow{position:absolute;left:12px;top:700px}iframe{position:absolute;left:12px;top:790px;width:340px;height:80px}#closed-choice{position:relative;width:366px;margin:450px 0 0 12px}#closed-choice>summary{display:flex}#closed-choice .options{position:absolute;left:0;top:50px;width:366px}#closed-choice .options button{display:flex;flex-direction:column;width:100%;border:0;padding:0;color:#de1d48}#closed-choice .options span{height:20px}#checkpoint{position:absolute;left:12px;top:1090px;width:300px;height:44px}</style></head><body><div id="private">SYNTHETIC_PRIVATE_ID</div><details id="closed-choice"><summary>Cabinet</summary><div class="options"><button><span>SYNTHETIC_ECHO_ID</span><span>SYNTHETIC_ECHO_ID</span></button><button><span>SYNTHETIC_ECHO_ID</span><span>SYNTHETIC_ECHO_ID</span></button><button><span>SYNTHETIC_ECHO_ID</span><span>SYNTHETIC_ECHO_ID</span></button></div></details><div id="public">Public action must remain readable</div><input id="declaration" value="SYNTHETIC_INPUT"><div id="shadow"></div><iframe title="Isolated fixture" src="/capture-child"></iframe><div id="echo">SYNTHETIC_ECHO_ID</div><p id="checkpoint" tabindex="-1">Validation needs attention</p></body></html>`;
const variants = new Map([
  ['/grid-reserved', html.replace('grid-template-columns:0px', 'grid-template-columns:200px')],
  ['/character-wrap', html.replace('h1{font-size:24px}', 'h1{font-size:24px;width:1px;overflow-wrap:anywhere}')],
  ['/single-character', html.replace('HDL workspace</h1>', 'X</h1>')],
  ['/clipped', html.replace('button{min-height:32px}', 'button{min-height:32px;width:30px;overflow:hidden;white-space:nowrap}')],
  ['/overflow', html.replace('body{margin:0', 'body{width:2000px;margin:0')],
  ['/offscreen', html.replace('main{padding:20px;grid-column:2;min-width:0}', 'main{padding:20px;margin-top:1000px}')],
  ['/visually-hidden-identity', html.replace('<button id="label">Edit signal</button>', '<div style="position:absolute;width:1px;height:1px;overflow:hidden;clip-path:inset(50%)"><button id="label">Edit signal</button></div>')],
]);
const recoveryMarker = '<div id="recovery-clip"><div id="recovery-state" style="height:20px;line-height:20px">Loading the selected object</div></div>';
const recoveryHtml = html.replace('</main>', recoveryMarker + '</main>');
for (const [name, style] of [
  ['short-visible', ''], ['short-partial', '#recovery-state{position:fixed;bottom:-1px}'],
  ['short-clipped', '#recovery-clip{height:19px;overflow:hidden}'],
  ['short-scroll-clipped', '#recovery-clip{height:19px;overflow:auto}'],
  ['short-hidden', '#recovery-clip{opacity:0}'], ['short-zero', '#recovery-state{height:0!important;overflow:hidden}'],
  ['tall-23-visible', '#recovery-state{height:40px!important;position:fixed;bottom:-17px}'],
  ['tall-24-visible', '#recovery-state{height:40px!important;position:fixed;bottom:-16px}'],
]) variants.set('/' + name, recoveryHtml.replace('</style>', style + '</style>'));
for (const name of ['loading', 'access-denied', 'error', 'identity-visible', 'identity-hidden', 'marker-missing', 'marker-ambiguous', 'marker-hidden', 'marker-empty', 'marker-zero', 'marker-clipped', 'marker-partial', 'text-opacity', 'text-hidden', 'text-clipped', 'text-scroll-clipped', 'text-nested-visible', 'text-visible-with-hidden-decoration', 'text-visible-contents', 'text-visible-contents-clip-declaration']) {
  let document = recoveryHtml;
  if (!name.startsWith('identity-')) document = document.replace('<button id="label">Edit signal</button>', '');
  if (name === 'identity-hidden') document = document.replace('id="label"', 'id="label" hidden');
  if (name === 'marker-missing') document = document.replace(recoveryMarker, '');
  if (name === 'marker-ambiguous') document = document.replace(recoveryMarker, recoveryMarker + recoveryMarker);
  if (name === 'marker-hidden') document = document.replace('id="recovery-clip"', 'id="recovery-clip" style="opacity:0"');
  if (name === 'marker-empty') document = document.replace('Loading the selected object', '');
  if (name === 'marker-zero') document = document.replace('height:20px;line-height:20px', 'height:0;line-height:20px;overflow:hidden');
  if (name === 'marker-clipped') document = document.replace('id="recovery-clip"', 'id="recovery-clip" style="height:19px;overflow:hidden"');
  if (name === 'marker-partial') document = document.replace('height:20px;line-height:20px', 'height:20px;line-height:20px;position:fixed;bottom:-1px');
  if (name === 'text-opacity') document = document.replace('Loading the selected object', '<span style="opacity:0">Loading the selected object</span>');
  if (name === 'text-hidden') document = document.replace('Loading the selected object', '<span style="visibility:hidden">Loading the selected object</span>');
  if (name === 'text-clipped') document = document.replace('Loading the selected object', '<span style="display:block;height:10px;overflow:hidden">Loading the selected object</span>');
  if (name === 'text-scroll-clipped') document = document.replace('height:20px;line-height:20px', 'height:20px;line-height:40px;overflow:auto');
  if (name === 'text-nested-visible') document = document.replace('Loading the selected object', '<span>Loading <span>the selected object</span></span>');
  if (name === 'text-visible-with-hidden-decoration') document = document.replace('Loading the selected object', 'Loading the selected object<span hidden>Decorative detail</span>');
  if (name === 'text-visible-contents') document = document.replace('Loading the selected object', '<span style="display:contents">Loading the selected object</span>');
  if (name === 'text-visible-contents-clip-declaration') document = document.replace('Loading the selected object', '<span style="display:contents;overflow:hidden;width:1px;height:1px"><span>Loading the selected object</span></span>');
  // The absent identity must not cause unrelated fixture script exceptions.
  document = document.replace("document.querySelector('#label').addEventListener", "document.querySelector('#label')?.addEventListener");
  variants.set('/recovery-' + name, document);
}
const uploadedRequests = [];
const continuationObservations = new Map();
for (const mode of ['collapse', 'collapse-extra-jump', 'no-shrink-jump', 'collapse-focus-lost', 'collapse-offscreen', 'expand']) {
  variants.set('/continuation-' + mode, `<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><style>body{margin:0;background:white;color:#111;font:16px sans-serif}main{padding:20px}#spacer{height:900px}#retained{height:130px}h1{font-size:24px}button{height:44px}#removed{height:${mode === 'expand' ? 0 : 600}px}footer{height:220px}</style><main id="primary"><div id="spacer">Expected delivery collection</div><section id="retained"><h1 id="heading">Parcel contents</h1><button id="label" data-ui-continuation-anchor>Parcel 20459000000021</button></section><div id="removed"></div><footer>More parcels remain</footer></main><script>
  const read=()=>({x:scrollX,y:scrollY,maxY:Math.max(0,document.scrollingElement.scrollHeight-document.scrollingElement.clientHeight),height:document.scrollingElement.clientHeight});
  document.querySelector('#label').onclick=async event=>{
    const before=read();
    if('${mode}'!=='no-shrink-jump')document.querySelector('#removed').style.height='${mode === 'expand' ? 600 : 0}px';
    void document.body.offsetHeight;
    event.target.focus({preventScroll:true});
    if('${mode}'==='collapse-extra-jump')scrollTo(0,Math.max(0,scrollY-60));
    if('${mode}'==='no-shrink-jump')scrollTo(0,scrollY+60);
    if('${mode}'==='collapse-focus-lost')event.target.blur();
    if('${mode}'==='collapse-offscreen'){event.target.style.position='absolute';event.target.style.top='0px';}
    const after=read();
    await fetch('/continuation-observe?mode=${mode}',{method:'POST',body:JSON.stringify({before,after})});
    document.documentElement.dataset.continued='true';
  };</script>`);
}
let uploadMutation = null;
const uploadHtml = html.replace('<button id="label">Edit signal</button>', '<button id="label">Import measurements</button><label>Fixture file<input type="file" id="upload"></label><p id="filename"></p><p id="upload-result" data-ui-continuation-anchor>Choose a measurement file</p>').replace('</body>', `<script>document.querySelector('#upload').onchange=async event=>{const file=event.target.files[0];const result=await fetch('/uploaded',{method:'POST',headers:{'x-upload-name':file.name,'x-upload-type':file.type},body:file});if(result.ok){document.querySelector('#filename').textContent=file.name;document.querySelector('#upload-result').textContent='Measurements loaded';document.querySelector('#upload-result').dataset.ready='true';event.target.focus();}}</script></body>`);
let requestCount = 0;
const sessionObservations = [];
const server = createServer((request, response) => {
  requestCount++;
  const pathname = new URL(request.url, 'http://fixture').pathname;
  if (pathname === '/session-observe') {
    sessionObservations.push(Object.fromEntries(new URL(request.url, 'http://fixture').searchParams));
    response.writeHead(204); response.end(); return;
  }
  if (pathname.startsWith('/session-')) {
    if (pathname === '/session-redirect') {
      response.writeHead(302, { location: `http://localhost:${server.address().port}/session-foreign` }); response.end(); return;
    }
    const identity = new URL(request.url, 'http://fixture').searchParams.get('identity') ?? 'A';
    const cookieValid = request.headers.cookie?.includes(`sess=SESSION_PRIVATE_${identity}`) ?? false;
    response.writeHead(200, { 'content-type': 'text/html', 'x-ui-source-revision': revision,
      ...(pathname === '/session-legacy-login' ? { 'set-cookie': 'sess=SESSION_PRIVATE_B; Path=/' } : {}),
      ...(pathname === '/session-opaque' ? { 'content-security-policy': 'sandbox allow-scripts' } : {}),
    });
    if (pathname === '/session-frame') {
      response.end('<!doctype html><script>parent.postMessage({kind:"session-frame",empty:sessionStorage.getItem("SESSION_PRIVATE_KEY")===null},"*")</script>'); return;
    }
    if (pathname === '/session-foreign') {
      response.end('<!doctype html><script>navigator.sendBeacon("/session-observe?foreignEmpty="+(sessionStorage.getItem("SESSION_PRIVATE_KEY")===null))</script>'); return;
    }
    const mode = new URL(request.url, 'http://fixture').searchParams.get('mode') ?? (['/session-reload','/session-logout'].includes(pathname) ? pathname.slice('/session-'.length) : null);
    const script = pathname.endsWith('login')
      ? `document.querySelector('#heading').dataset.session=${JSON.stringify(cookieValid ? 'authenticated' : 'denied')};`
      : `const present=sessionStorage.getItem('SESSION_PRIVATE_KEY')===${JSON.stringify('SESSION_PRIVATE_VALUE_'+identity)};
const returning=window.name==='session-return';
sessionStorage.removeItem('SESSION_PRIVATE_KEY');
if(${JSON.stringify(mode)}==='immediate'&&!returning){window.name='session-return';sessionStorage.clear();navigator.sendBeacon('/session-observe?identity=${identity}&phase=first&present='+present);location.reload();}
else {
const observed=()=>fetch('/session-observe?identity=${identity}&phase='+(returning?'return':'first')+'&present='+present+'&cookie=${cookieValid}').then(()=>{const heading=document.querySelector('#heading');if(returning){heading.tabIndex=-1;heading.focus()}heading.dataset.session=returning?(!present?'cleared':'reseeded'):((present||${JSON.stringify(mode)}==='no-seed')&&${cookieValid}?'ready':'missing')});
if(${JSON.stringify(mode)}==='frames'){
let remaining=2,empty=true;addEventListener('message',event=>{if(event.data?.kind!=='session-frame')return;empty=empty&&event.data.empty;if(--remaining===0){fetch('/session-observe?framesEmpty='+empty).then(()=>empty?observed():document.querySelector('#heading').dataset.session='reseeded')}});
for(const host of [location.origin,location.origin.replace('127.0.0.1','localhost')]){const frame=document.createElement('iframe');frame.title='Isolated storage frame';frame.src=host+'/session-frame';document.querySelector('#primary').append(frame);}
}else observed();
document.querySelector('#label').onclick=()=>{window.name='session-return';sessionStorage.clear();if(${JSON.stringify(mode)}==='logout')document.cookie='sess=; Max-Age=0; Path=/';location.reload();};
}`;
    response.end(html.replace('</body>', `<script>${script}</script></body>`)); return;
  }
  if (pathname === '/upload' && uploadMutation) { uploadMutation(); uploadMutation = null; }
  if (pathname === '/continuation-observe') {
    const chunks = []; request.on('data', chunk => chunks.push(chunk));
    request.on('end', () => { continuationObservations.set(new URL(request.url, 'http://fixture').searchParams.get('mode'), JSON.parse(Buffer.concat(chunks))); response.writeHead(200); response.end('observed'); });
    return;
  }
  if (pathname === '/uploaded') {
    const chunks = []; request.on('data', chunk => chunks.push(chunk));
    request.on('end', () => { uploadedRequests.push({ name: request.headers['x-upload-name'], type: request.headers['x-upload-type'], content: Buffer.concat(chunks).toString(), sha256: createHash('sha256').update(Buffer.concat(chunks)).digest('hex') }); response.writeHead(200); response.end('saved'); });
    return;
  }
  const status = Number(pathname.match(/^\/http-(403|404|503)(?:-overflow)?$/)?.[1] ?? 200);
  const csp = pathname === '/capture-csp' || pathname === '/capture-child' ? { 'content-security-policy': "default-src 'self';style-src 'nonce-fixture'" } : {};
  response.writeHead(status, { 'content-type': 'text/html', 'x-ui-source-revision': revision, ...csp });
  response.end(pathname === '/upload-hidden' ? uploadHtml.replace('type="file" id="upload"', 'type="file" id="upload" hidden').replace('event.target.focus()', "document.querySelector('#label').focus()") : pathname === '/upload' ? uploadHtml : pathname === '/capture-child' ? '<!doctype html><style nonce="fixture">body{background:white}input{color:#de1d48;background:white;border:1px solid #222;width:280px}</style><input value="SYNTHETIC_CHILD_INPUT">' : pathname.startsWith('/capture') ? captureHtml : pathname.endsWith('-overflow') ? variants.get('/overflow') : variants.get(pathname) ?? html);
});
server.listen(0, '127.0.0.1');
await once(server, 'listening');
const target = {
  name: 'handoff', url: `http://127.0.0.1:${server.address().port}/`, theme: 'light',
  sourceBinding: { expected: revision }, waitFor: { selector: '#primary' },
  journeys: [{ id: 'edit-signal', frequencyPercent: 100, risk: 'normal' }], primaryJourney: 'edit-signal',
  regions: [{ selector: '#primary', role: 'primary-content', journey: 'edit-signal' }],
  reviewInputs: [{ path: 'rust/tooling/tests/formal_handoff.mjs', kind: 'ui-code' }],
};
const base = { repoRoot: root, targets: [target], viewports: [{ name: 'desktop', width: 1440, height: 900 }], performance: { ttfbMs: 10000, lcpMs: 10000, ttfbLocalOnly: false }, maxPageCount: 2 };
const geometry = (primary, heading, identifier) => [
  { id: 'primary-width', kind: 'primary-content-width', selector: primary, minWidthRatio: primary === '#primary' ? 0.75 : 0.15 },
  { id: 'heading', kind: 'readable-heading', selector: heading },
  { id: 'identifier', kind: 'readable-canonical-identifier', selector: identifier },
  { id: 'wrapping', kind: 'no-character-wrapping', selector: heading },
  { id: 'overflow', kind: 'document-horizontal-overflow', selector: primary },
  { id: 'placement', kind: 'initial-viewport-placement', selector: primary },
  { id: 'clipping', kind: 'clipping', selector: identifier },
];
const full = (pathname = '/') => structuredClone({ ...base, targets: [{ ...target, url: `${target.url.slice(0, -1)}${pathname}`, geometryAssertions: [...geometry('#primary', '#heading', '#label'), { id: 'hidden-track', kind: 'hidden-navigation-track', selector: '#nav', primarySelector: '#primary', track: { selector: '#layout', axis: 'columns', index: 0 }, maxReservedSize: 0 }] }], fixtureDataShapes: [{ id: 'workspace', revision: 'v1', target: 'handoff', route: pathname, state: 'base', conditionalDom: ['#layout', '#nav', '#primary', '#label'], layoutEffect: 'Workspace and attached hidden navigation track' }], requiredCoverage: [{ target: 'handoff', state: 'base', viewport: 'desktop', width: 1440 }] });
const results = [];
async function capturePrivacy() {
  const require = createRequire(import.meta.url);
  const modules = process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES ?? join(root, 'ci/playwright/node_modules');
  const { chromium, devices } = require(join(modules, 'playwright'));
  const { PNG } = require(join(modules, 'playwright-core/lib/utilsBundle.js'));
  const browser = await chromium.launch({ headless: true });
  try {
    for (const csp of [false, true]) for (const phone of [false, true]) for (const scrolled of [false, true]) {
      const name = `capture-${csp ? 'csp' : 'plain'}-${phone ? 'phone' : 'desktop'}-${scrolled ? 'scrolled' : 'top'}`;
      const context = await browser.newContext(phone ? { ...devices['iPhone 13'] } : { viewport: { width: 390, height: 664 } });
      const page = await context.newPage();
      try {
        await page.goto(`${target.url.slice(0, -1)}/capture${csp ? '-csp' : ''}`);
        await page.locator('#checkpoint').focus();
        await page.evaluate(scrolled => {
          const existing = new CSSStyleSheet();existing.replaceSync('#public{letter-spacing:.1px}');document.adoptedStyleSheets = [existing];
          const shadow = document.querySelector('#shadow').attachShadow({ mode: 'open' });
          shadow.innerHTML = '<style nonce="fixture">input{color:#de1d48;background:white;border:1px solid #222;width:300px}</style><input value="SYNTHETIC_SHADOW_INPUT">';
          window.scrollTo({ top: scrolled ? 552 : 0, behavior: 'instant' });
        }, scrolled);
        const settings = { screenshotDir: join(scratch, name), screenshotMasks: [] };
        const captureTarget = { name, screenshotMasks: [{ selector: '#private', reason: 'synthetic identity' }], verificationState: { actions: [{ action: 'fill', selector: '#declaration', value: 'SYNTHETIC_ECHO_ID' }] } };
        const state = async () => ({ main: await page.evaluate(() => ({ scroll: { x: scrollX, y: scrollY }, focus: document.activeElement?.id, color: getComputedStyle(document.querySelector('#declaration')).color, sheets: document.adoptedStyleSheets.length, shadowSheets: document.querySelector('#shadow').shadowRoot.adoptedStyleSheets.length })), children: await Promise.all(page.frames().filter(frame => frame !== page.mainFrame()).map(frame => frame.evaluate(() => ({ color: getComputedStyle(document.querySelector('input')).color, sheets: document.adoptedStyleSheets.length })))) });
        const original = await state();
        const privateAreas = await page.locator('#private, #echo').evaluateAll(elements => elements.map(element => {
          const box = element.getBoundingClientRect();
          return { x: box.x + scrollX, y: box.y + scrollY, width: box.width, height: box.height };
        }));
        const failures = [];
        for (const kind of ['viewport', 'full-page']) {
          const shot = await captureEvidenceScreenshot(page, captureTarget, { name: phone ? 'phone' : 'desktop' }, settings, name, kind);
          const png = PNG.sync.read(await readFile(shot.path));
          let protectedPixels = 0, maskPixels = 0, publicPixels = 0, misplacedMaskPixels = 0;
          for (let index = 0; index < png.data.length; index += 4) {
            const [red, green, blue] = png.data.subarray(index, index + 3);
            if (red > 150 && green < 80 && blue < 130) protectedPixels++;
            if (red === 119 && green === 119 && blue === 119) {
              maskPixels++;
              const x = (index / 4) % png.width + (kind === 'viewport' ? original.main.scroll.x : 0);
              const y = Math.floor(index / 4 / png.width) + (kind === 'viewport' ? original.main.scroll.y : 0);
              if (!privateAreas.some(box => x >= Math.floor(box.x) && x < Math.ceil(box.x + box.width) && y >= Math.floor(box.y) && y < Math.ceil(box.y + box.height))) misplacedMaskPixels++;
            }
            if (red < 60 && green > 90 && blue > 90) publicPixels++;
          }
          if (protectedPixels) failures.push(`${kind}: ${protectedPixels} protected text pixels`);
          if (misplacedMaskPixels) failures.push(`${kind}: hidden choice creates ${misplacedMaskPixels} mask pixels outside rendered private regions`);
          if (kind === 'full-page' && (maskPixels < 13000 || publicPixels < 100)) failures.push(`${kind}: mask or ordinary content missing`);
          try { assert.deepEqual(await state(), original); } catch { failures.push(`${kind}: screenshot changed page state`); }
        }
        // The same option labels must still be private when the disclosure opens.
        await page.locator('#closed-choice').evaluate(element => { element.open = true; });
        const optionAreas = await page.locator('#closed-choice .options span').evaluateAll(elements => elements.map(element => {
          const box = element.getBoundingClientRect();
          return { x: box.x + scrollX, y: box.y + scrollY, width: box.width, height: box.height, visible: element.checkVisibility() };
        }));
        const openShot = await captureEvidenceScreenshot(page, captureTarget, { name: 'open-choice' }, settings, name, 'full-page');
        const openPng = PNG.sync.read(await readFile(openShot.path));
        if (optionAreas.length !== 6 || optionAreas.some(box => !box.visible || box.width <= 0 || box.height <= 0)) failures.push('open choices must actually render');
        for (const box of optionAreas) {
          const offset = (Math.floor(box.y + box.height / 2) * openPng.width + Math.floor(box.x + box.width / 2)) * 4;
          if (!openPng.data.subarray(offset, offset + 3).every(channel => channel === 119)) failures.push('visible echoed option was not masked');
        }
        for (let index = 0; index < openPng.data.length; index += 4) {
          const [red, green, blue] = openPng.data.subarray(index, index + 3);
          if (red > 150 && green < 80 && blue < 130) { failures.push('open choice capture retained protected text'); break; }
        }
        await page.locator('#closed-choice').evaluate(element => { element.open = false; });
        try { assert.deepEqual(await state(), original); } catch { failures.push('open choice screenshot changed page state'); }
        await page.locator('#declaration').evaluate(element => element.style.setProperty('color', '#de1d48', 'important'));
        try { await captureEvidenceScreenshot(page, captureTarget, { name: 'color-override' }, settings, name, 'full-page'); }
        catch { failures.push('transparent text fill must allow an unrelated color override'); }
        await page.locator('#declaration').evaluate(element => { element.style.removeProperty('color'); element.style.setProperty('-webkit-text-fill-color', '#de1d48', 'important'); });
        try {
          await assert.rejects(captureEvidenceScreenshot(page, captureTarget, { name: 'blocked' }, settings, name, 'full-page'), /form redaction could not be applied/);
          await assert.rejects(access(join(settings.screenshotDir, `${name}-${name}-blocked-full-page.png`)), { code: 'ENOENT' });
        } catch { failures.push('unredacted input must block retention'); }
        await page.locator('#declaration').evaluate(element => element.style.removeProperty('-webkit-text-fill-color'));
        try { assert.deepEqual(await state(), original); } catch { failures.push('failed redaction changed page state'); }
        const screenshot = page.screenshot.bind(page);
        page.screenshot = async () => { throw new Error('synthetic capture failure'); };
        try {
          await assert.rejects(captureEvidenceScreenshot(page, captureTarget, { name: 'failed' }, settings, name, 'full-page'), /synthetic capture failure/);
          await assert.rejects(access(join(settings.screenshotDir, `${name}-${name}-failed-full-page.png`)), { code: 'ENOENT' });
          assert.deepEqual(await state(), original);
        } catch { failures.push('failed capture must restore the page without an artifact'); }
        finally { page.screenshot = screenshot; }
        if (csp) {
          const protectedColor = await page.evaluate(() => { const injected = document.createElement('style');injected.textContent = '#public{color:red!important}';document.head.append(injected);const color = getComputedStyle(document.querySelector('#public')).color;injected.remove();return color; });
          if (protectedColor !== 'rgb(0, 136, 136)') failures.push('page CSP was not preserved');
        }
        results.push({ name, passed: failures.length === 0, failures });
      } catch (error) { results.push({ name, passed: false, error: error.message }); }
      finally { await context.close(); }
    }
  } finally { await browser.close(); }
}
let measuredUserAgent;
async function verify(name, config, check, setupError = null) {
  const directory = join(scratch, name); await mkdir(directory, { mode: 0o700 });
  const path = join(directory, 'config.json'); await writeFile(path, JSON.stringify(config));
  const args = [join(root, 'skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs'), '--config', path, '--json-out', join(directory, 'report.json'), '--markdown-out', join(directory, 'report.md')];
  if (process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES) args.push('--playwright-module-dir', process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES);
  const child = spawn(process.execPath, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '', stderr = ''; child.stdout.on('data', chunk => { stdout += chunk; }); child.stderr.on('data', chunk => { stderr += chunk; });
  const [exitCode] = await once(child, 'exit');
  await writeFile(join(directory, 'stdout.json'), stdout); await writeFile(join(directory, 'stderr.txt'), stderr);
  try {
    const receipt = JSON.parse(stdout.trim());
    const report = JSON.parse(await readFile(join(directory, 'report.json'), 'utf8'));
    assert(Buffer.byteLength(stdout) <= 2048, 'Receipt must remain bounded');
    assert.equal(stderr, '');
    if (setupError) {
      assert.equal(exitCode, 2); assert.equal(receipt.formal.result, 'blocked');
      assert.match(report.error.message, setupError);
      results.push({ name, passed: true, exitCode }); return;
    }
    await check({ exitCode, receipt, report, directory });
    results.push({ name, passed: true, exitCode });
  } catch (error) { results.push({ name, passed: false, exitCode, error: error.message }); }
}
async function configurationFixtures(recoveryOnly = false) {
  async function check(name, config, status, inspect = () => {}) {
    const directory = join(scratch, name); await mkdir(directory, { mode: 0o700 });
    const configPath = join(directory, 'config.json');
    config.browserExecutable = join(directory, 'browser-must-not-start');
    config.authProfiles ??= [{ name: 'private', url: target.url, actions: [{ action: 'fill', selector: '#private', value: 'PREFLIGHT_PRIVATE_CANARY' }] }];
    await writeFile(configPath, JSON.stringify(config));
    const beforeRequests = requestCount;
    const child = spawn(process.execPath, [join(root, 'skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs'), '--config', configPath, '--config-only', '--json-out', join(directory, 'report.json')], { cwd: directory, env: { ...process.env, DEVCOORDINATOR_EVIDENCE_DIR: directory }, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = ''; child.stdout.on('data', chunk => { stdout += chunk; }); child.stderr.on('data', chunk => { stderr += chunk; });
    const [exitCode] = await once(child, 'exit');
    try {
      const receipt = JSON.parse(stdout.trim());
      assert.equal(exitCode, status === 'valid' ? 0 : 2);
      assert.equal(receipt.kind, 'configuration-preflight'); assert.equal(receipt.status, status);
      assert.equal(receipt.browserStarted, false); assert.equal(receipt.readinessEligible, false);
      assert.equal(receipt.formal, undefined); assert.equal(receipt.qualified, undefined);
      assert.equal(requestCount, beforeRequests, 'preflight must not navigate, authenticate or probe targets');
      assert(Buffer.byteLength(stdout) <= 2048); assert(!stdout.includes('PREFLIGHT_PRIVATE_CANARY')); assert(!stdout.includes('SESSION_PRIVATE_')); assert.equal(stderr, '');
      assert.deepEqual(await readdir(directory), ['config.json'], 'preflight cannot create formal acceptance artifacts');
      inspect(receipt);
      results.push({ name, passed: true, exitCode });
    } catch (error) { results.push({ name, passed: false, exitCode, error: error.message }); }
    await writeFile(join(directory, 'preflight.json'), stdout);
  }
  function many(count) {
    const config = full(); config.maxPageCount = count;
    const original = config.targets[0], shape = config.fixtureDataShapes[0];
    config.targets = Array.from({ length: count }, (_, index) => ({ ...original, name: `target-${index}` }));
    config.fixtureDataShapes = config.targets.map(row => ({ ...shape, id: row.name, target: row.name }));
    config.requiredCoverage = config.targets.map(row => ({ target: row.name, state: 'base', viewport: 'desktop', width: 1440 }));
    return config;
  }
  for (const kind of ['primary-content-width', 'readable-heading', 'readable-canonical-identifier', 'no-character-wrapping', 'document-horizontal-overflow', 'initial-viewport-placement', 'clipping']) {
    const config = full(); config.targets[0].geometryAssertions = config.targets[0].geometryAssertions.filter(row => row.kind !== kind);
    await check('config-missing-' + kind, config, 'invalid', receipt => assert.equal(receipt.gapCounts['required-geometry-' + kind + '-missing'], 1));
  }
  const na = recoveryConfig('loading');
  await check('config-identity-not-applicable', na, 'valid', receipt => assert.deepEqual(receipt.gapCounts, {}));
  for (const [name, mutate] of [
    ['other-kind', row => { row.kind = 'readable-heading'; }],
    ['state', row => { row.applicability.state = 'ordinary'; }],
    ['status', row => { row.applicability.status = 'skipped'; }],
    ['reason', row => { row.applicability.reason = ''; }],
    ['marker', row => { delete row.applicability.stateSelector; }],
    ['allowance', row => { row.allowance = { reason: 'Must not waive N/A proof' }; }],
    ['region', row => { delete row.selector; row.region = 'Fixture content'; }],
  ]) {
    const config = structuredClone(na); mutate(config.targets[0].geometryAssertions.find(row => row.id === 'identifier'));
    await check('config-invalid-applicability-' + name, config, 'invalid');
  }
  if (recoveryOnly) return;
  for (const count of [270, 512]) await check(`config-${count}-shapes`, many(count), 'valid', receipt => { assert.equal(receipt.plannedCells, count); assert.equal(receipt.declaredShapes, count); assert.deepEqual(receipt.gapCounts, {}); });
  await check('config-513-shapes', many(513), 'invalid', receipt => assert.equal(receipt.error, 'configuration-invalid'));
  const missing = full(); missing.maxPageCount = 5;
  missing.targets.push({ ...missing.targets[0], name: 'handoff-breakpoints', breakpointProfile: { name: 'edge', baseViewport: 'desktop', breakpoints: [800], height: 900 } });
  await check('config-missing-breakpoint-shape', missing, 'invalid', receipt => { assert.equal(receipt.plannedCells, 5); assert.equal(receipt.gapCounts['data-shape-not-declared'], 4); });
  const complete = structuredClone(missing); complete.fixtureDataShapes.push({ ...complete.fixtureDataShapes[0], id: 'edge-shape', target: 'handoff-breakpoints' });
  await check('config-complete-breakpoint-shape', complete, 'valid', receipt => assert.equal(receipt.plannedCells, 5));
  const ambiguous = structuredClone(missing); delete ambiguous.fixtureDataShapes[0].target;
  await check('config-ambiguous-shape', ambiguous, 'invalid', receipt => assert.equal(receipt.gapCounts['ambiguous-target'], 1));
  const uncovered = full(); uncovered.requiredCoverage[0].state = 'undeclared-state';
  await check('config-missing-required-cell', uncovered, 'invalid', receipt => assert.equal(receipt.gapCounts['required-coverage-not-mapped'], 1));
  const duplicate = full(); duplicate.fixtureDataShapes.push({ ...duplicate.fixtureDataShapes[0] });
  await check('config-duplicate-shape-id', duplicate, 'invalid');
  const otherLimit = full(); otherLimit.targets[0].geometryAssertions = Array.from({ length: 257 }, (_, index) => ({ ...otherLimit.targets[0].geometryAssertions[0], id: `geometry-${index}` }));
  await check('config-geometry-limit-unchanged', otherLimit, 'invalid');
  const sessionConfig = () => {
    const config = full(); config.targets[0].authProfile = 'fixture';
    config.authProfiles = [{ name: 'fixture', url: target.url, actions: [{ action: 'focus', selector: '#label' }],
      cookies: [{ name: 'SESSION_PRIVATE_COOKIE', value: 'SESSION_PRIVATE_VALUE', url: target.url }],
      sessionStorage: { origin: new URL(target.url).origin, entries: [{ name: 'SESSION_PRIVATE_KEY', value: 'SESSION_PRIVATE_VALUE' }] } }];
    return config;
  };
  await check('config-session-valid', sessionConfig(), 'valid');
  const invalidSessions = [
    ['origin-path', seed => { seed.origin += '/'; }],
    ['origin-scheme', seed => { seed.origin = 'file:///private'; }],
    ['origin-credentials', seed => { seed.origin = 'http://PRIVATE:VALUE@localhost'; }],
    ['origin-mismatch', seed => { seed.origin = 'https://different.invalid'; }],
    ['entries-type', seed => { seed.entries = 'SESSION_PRIVATE_VALUE'; }],
    ['entries-empty', seed => { seed.entries = []; }],
    ['duplicate', seed => { seed.entries.push({ ...seed.entries[0] }); }],
    ['key-overflow', seed => { seed.entries[0].name = 'x'.repeat(1025); }],
    ['value-overflow', seed => { seed.entries[0].value = 'x'.repeat(262145); }],
    ['value-type', seed => { seed.entries[0].value = {}; }],
    ['entries-overflow', seed => { seed.entries = Array.from({ length: 65 }, (_, i) => ({ name: String(i), value: '' })); }],
    ['bytes-overflow', seed => { seed.entries = Array.from({ length: 4 }, (_, i) => ({ name: String(i), value: 'x'.repeat(262144) })); }],
    ['unknown-field', seed => { seed.script = 'SESSION_PRIVATE_VALUE'; }],
  ];
  for (const [name, mutate] of invalidSessions) {
    const config = sessionConfig(); mutate(config.authProfiles[0].sessionStorage);
    await check(`config-session-${name}`, config, 'invalid');
  }
  const boundary = sessionConfig(); boundary.authProfiles[0].sessionStorage.entries = Array.from({ length: 64 }, (_, i) => ({ name: String(i), value: '' }));
  await check('config-session-64-entries', boundary, 'valid');
  const bytes = sessionConfig(); bytes.authProfiles[0].sessionStorage.entries = Array.from({ length: 4 }, (_, i) => ({ name: String(i).padEnd(1024, 'x'), value: 'x'.repeat(261120) }));
  await check('config-session-byte-boundary', bytes, 'valid');
  const individual = sessionConfig(); individual.authProfiles[0].sessionStorage.entries = [{ name: 'я'.repeat(512), value: 'x'.repeat(262144) }];
  await check('config-session-individual-boundary', individual, 'valid');
  individual.authProfiles[0].sessionStorage.entries[0].name += 'я';
  await check('config-session-utf8-overflow', individual, 'invalid');
  const invalidCookie = sessionConfig(); invalidCookie.authProfiles[0].cookies = ['SESSION_PRIVATE_BROKEN'];
  await check('config-session-cookie-invalid', invalidCookie, 'invalid');
  for (const scope of ['root','defaults','target','state']) {
    const config = sessionConfig(), seed = config.authProfiles[0].sessionStorage;
    if (scope === 'root') config.sessionStorage = seed;
    if (scope === 'defaults') config.targetDefaults = { sessionStorage: seed };
    if (scope === 'target') config.targets[0].sessionStorage = seed;
    if (scope === 'state') config.targets[0].states = [{ name: 'misplaced', actions: [{ action: 'focus', selector: '#label' }], sessionStorage: seed }];
    await check(`config-session-misplaced-${scope}`, config, 'invalid');
  }
}
function recoveryConfig(name, state = name) {
  const config = full('/recovery-' + name), assertions = config.targets[0].geometryAssertions;
  assertions.find(row => row.kind === 'readable-canonical-identifier').applicability = { status: 'not-applicable', state, stateSelector: '#recovery-state', reason: 'The selected object is unavailable in this declared recovery state' };
  assertions.find(row => row.kind === 'clipping').selector = '#heading';
  config.fixtureDataShapes[0].conditionalDom = ['#layout', '#primary'];
  return config;
}
async function recoveryGeometryFixtures() {
  for (const [name, expected] of [['short-visible', false], ['short-partial', true], ['short-clipped', true], ['short-scroll-clipped', true], ['short-hidden', true], ['short-zero', true], ['tall-23-visible', true], ['tall-24-visible', false]]) {
    const config = full('/' + name); config.targets[0].regions = [{ selector: '#recovery-state', role: 'primary-content', journey: 'edit-signal' }];
    await verify(name, config, ({ receipt, report }) => {
      const measured = report.pages[0].metrics.journey;
      assert.equal(measured.findings.some(row => ['primary-journey-content-missing', 'primary-journey-outside-initial-viewport'].includes(row.rule)), expected);
      if (name === 'short-visible') { assert.equal(receipt.formal.result, 'passed'); assert.equal(measured.regions[0].requiredVisibleHeight, 20); }
      if (name.startsWith('tall-')) assert.equal(measured.regions[0].requiredVisibleHeight, 24);
    });
  }
  for (const state of ['loading', 'access-denied', 'error']) await verify('identity-not-applicable-' + state, recoveryConfig(state), ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'passed'); assert.equal(receipt.formal.coverage.gapCount, 0);
    const row = report.pages[0].metrics.handoff.geometry.find(row => row.id === 'identifier');
    assert.equal(row.status, 'not-applicable'); assert.equal(row.measurements.matchCount, 0);
    assert.equal(row.measurements.stateMatchCount, 1); assert.equal(row.measurements.stateVisible, true);
    assert.equal(row.applicability.state, state); assert(!JSON.stringify(row).includes('Loading the selected object'));
    for (const mutate of [row => { delete row.measurements.stateVisibleBox; }, row => { delete row.measurements.stateWrapping.fullyVisibleText; }, row => { row.measurements.matchCount = 1; }, row => { row.kind = 'readable-heading'; }]) {
      const forged = structuredClone(report); mutate(forged.pages[0].metrics.handoff.geometry.find(row => row.id === 'identifier'));
      assert.equal(formalDecision(forged, 0, { handoffRequirements: { gaps: [] } }, []).result, 'incomplete', 'An unmeasured N/A row cannot establish a formal pass');
    }
  });
  for (const [name, expected] of [['identity-visible', 'failed'], ['identity-hidden', 'failed'], ['marker-missing', 'incomplete'], ['marker-ambiguous', 'incomplete'], ['marker-hidden', 'failed'], ['marker-empty', 'failed'], ['marker-zero', 'failed'], ['marker-clipped', 'failed'], ['marker-partial', 'failed']]) await verify('identity-not-applicable-' + name, recoveryConfig(name, 'loading'), ({ receipt, report }) => {
    assert.equal(receipt.formal.result, expected);
    const row = report.pages[0].metrics.handoff.geometry.find(row => row.id === 'identifier');
    assert.equal(row.status, expected); assert.notEqual(row.status, 'not-applicable');
  });
  const ordinary = recoveryConfig('loading'); delete ordinary.targets[0].geometryAssertions.find(row => row.id === 'identifier').applicability;
  await verify('ordinary-missing-identity', ordinary, ({ receipt }) => assert.equal(receipt.formal.result, 'incomplete'));
  const invalidIdentity = recoveryConfig('loading'); invalidIdentity.targets[0].geometryAssertions.find(row => row.id === 'identifier').selector = '[';
  await verify('invalid-absent-identity-selector', invalidIdentity, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'incomplete'); assert.equal(report.pages[0].metrics.handoff.geometry.find(row => row.id === 'identifier').reason, 'invalid-identity-selector'); });
  for (const name of ['text-opacity', 'text-hidden', 'text-clipped', 'text-scroll-clipped', 'text-nested-visible', 'text-visible-with-hidden-decoration', 'text-visible-contents', 'text-visible-contents-clip-declaration']) await verify('identity-not-applicable-' + name, recoveryConfig(name, 'loading'), ({ receipt, report }) => {
    const positive = name.startsWith('text-nested-') || name.startsWith('text-visible-');
    const row = report.pages[0].metrics.handoff.geometry.find(row => row.id === 'identifier');
    assert.equal(row.status, positive ? 'not-applicable' : 'failed', 'Container visibility cannot substitute for fully visible recovery text');
    assert.equal(receipt.formal.result, positive ? 'passed' : 'failed');
  });
}
async function continuationPrecisionFixtures() {
  for (const mode of ['collapse', 'collapse-extra-jump', 'no-shrink-jump', 'collapse-focus-lost', 'collapse-offscreen', 'expand']) {
    const config = full('/continuation-' + mode), row = config.targets[0];
    config.viewports = [{ name: 'phone', width: 390, height: 664 }];
    row.includeBase = false;
    row.geometryAssertions = geometry('#retained', '#heading', '#label');
    row.regions = [{ selector: '#retained', role: 'primary-content', journey: 'edit-signal' }];
    row.states = [{ name: mode, actions: [{ action: 'focus', selector: '#label' }, { action: 'click', selector: '#label' }], waitFor: { selector: 'html[data-continued=true]' }, continuation: { kind: 'in-page', anchor: '#label', focusWithin: '#label', triggerActionIndex: 1 } }];
    config.fixtureDataShapes[0] = { ...config.fixtureDataShapes[0], state: mode, conditionalDom: ['#retained', '#label'] };
    config.requiredCoverage = [{ target: 'handoff', state: mode, viewport: 'phone', width: 390 }];
    await verify('continuation-precision-' + mode, config, async ({ receipt, report, directory }) => {
      const observation = continuationObservations.get(mode);
      await writeFile(join(directory, 'fixture-scroll-observation.json'), JSON.stringify(observation, null, 2));
      assert(observation, 'real click must record before and after scroll extent');
      const { before, after } = observation;
      const evidence = report.pages[0].continuation.evidence;
      const findings = report.pages[0].findings.filter(row => row.rule.startsWith('continuation-'));
      if (mode === 'collapse') {
        assert(before.y > after.maxY + 8); assert(after.maxY < before.maxY); assert.equal(after.y, after.maxY);
        assert.equal(evidence.focusSatisfied, true); assert.equal(evidence.anchorVisibleInViewport, true);
        assert.deepEqual(findings, [], 'unavoidable document clamp preserves the focused continuation');
        assert.equal(evidence.scrollDelta, before.y - after.y);
        assert.equal(evidence.residualScrollDelta, 0);
        assert.deepEqual(evidence.documentClamp, { beforeScrollY: before.y, beforeMaxScrollY: before.maxY, afterScrollY: after.y, afterMaxScrollY: after.maxY, expectedScrollY: after.maxY });
        assert.equal(receipt.formal.result, 'passed');
      } else if (mode === 'expand') {
        assert(after.maxY > before.maxY); assert.equal(after.y, before.y);
        assert.equal(evidence.documentClamp, null);
        assert.deepEqual(findings, []); assert.equal(receipt.formal.result, 'passed');
      } else {
        const rule = mode.endsWith('focus-lost') ? 'continuation-focus-missing' : mode.endsWith('offscreen') ? 'continuation-anchor-offscreen' : 'continuation-document-jump';
        assert(findings.some(row => row.rule === rule), 'real continuation defect remains blocking: ' + rule);
        assert.equal(receipt.formal.result, 'failed');
      }
    });
  }
  await verify('visually-hidden-explicit-identity', full('/visually-hidden-identity'), ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'failed', 'global hidden-text precision cannot waive a declared canonical identifier');
    assert(report.pages[0].metrics.handoff.geometry.some(row => ['identifier', 'clipping'].includes(row.id) && row.status === 'failed'));
  });
}
async function sessionFixtures() {
  const config = full('/session-cell?identity=A');
  config.viewports.push({ name: 'mobile', width: 390, height: 900 }); config.maxPageCount = 4;
  config.execution = { maxConcurrency: 4 };
  config.cookies = [{ name: 'sess', value: 'SESSION_PRIVATE_GLOBAL', url: target.url }];
  config.authProfiles = ['A', 'B'].map(identity => ({
    name: `session-${identity}`, url: new URL(`/session-login?identity=${identity}`, target.url).href,
    cookies: [{ name: 'sess', value: `SESSION_PRIVATE_${identity}`, url: target.url }],
    actions: [{ action: 'focus', selector: '#label' }],
    waitFor: { selector: '#heading[data-session="authenticated"]', timeoutMs: 500 },
    sessionStorage: { origin: new URL(target.url).origin, entries: [{ name: 'SESSION_PRIVATE_KEY', value: `SESSION_PRIVATE_VALUE_${identity}` }] },
  }));
  config.targets = ['A', 'B'].map(identity => ({ ...config.targets[0], name: `session-${identity}`,
    url: new URL(`/session-cell?identity=${identity}`, target.url).href, authProfile: `session-${identity}`,
    execution: { parallelSafe: true, resourceLocks: [] },
    waitFor: { selector: '#heading[data-session="ready"]', timeoutMs: 500 },
  }));
  config.fixtureDataShapes = config.targets.map(row => ({ ...config.fixtureDataShapes[0], id: row.name, target: row.name, route: new URL(row.url).pathname + new URL(row.url).search }));
  config.requiredCoverage = config.targets.flatMap(row => config.viewports.map(viewport => ({ target: row.name, state: 'base', viewport: viewport.name, width: viewport.width })));
  await verify('session-fresh-private-profiles', config, async ({ receipt, report, directory }) => {
    assert.equal(receipt.formal.result, 'passed');
    assert.deepEqual(sessionObservations.slice(-4).map(row => [row.identity, row.present, row.cookie]).sort(), [['A','true','true'],['A','true','true'],['B','true','true'],['B','true','true']]);
    assert(report.pages.every(page => page.outcome === 'checked'));
    assert(report.pages.every(page => page.execution.parallelSafe));
    assert(report.pages.some((left, i) => report.pages.some((right, j) => i !== j && Date.parse(left.startedAt) < Date.parse(right.endedAt) && Date.parse(right.startedAt) < Date.parse(left.endedAt))), 'Parallel session cells must actually overlap');
    for (const file of ['report.json','report.md','journey-evidence.json','review-queue.json','formal-receipt.json','stdout.json','stderr.txt']) {
      assert(!(await readFile(join(directory,file),'utf8')).includes('SESSION_PRIVATE_'), file);
    }
  });
  const single = mode => {
    const result = structuredClone(config); result.targets = [result.targets[0]]; result.authProfiles = [result.authProfiles[0]];
    result.viewports = [result.viewports[0]]; result.maxPageCount = 2;
    result.targets[0].url = ['reload','logout'].includes(mode) ? new URL(`/session-${mode}`, target.url).href : result.targets[0].url + `&mode=${mode}`;
    result.fixtureDataShapes = [{ ...result.fixtureDataShapes[0], route: new URL(result.targets[0].url).pathname + new URL(result.targets[0].url).search }];
    result.requiredCoverage = [result.requiredCoverage[0]];
    return result;
  };
  await verify('session-no-frame-seed', single('frames'), ({ receipt }) => {
    assert.equal(receipt.formal.result, 'passed');
    assert(sessionObservations.some(row => row.framesEmpty === 'true'));
  });
  for (const mode of ['reload','logout']) {
    const result = single(mode), row = result.targets[0];
    row.includeBase = false;
    row.states = [{ name: 'cleared', actions: [{ action: 'click', selector: '#label' }],
      waitFor: { selector: '#heading[data-session="cleared"]', timeoutMs: 1000 },
      continuation: { kind: 'navigation', anchor: '#heading', expectedPath: new URL(row.url).pathname + new URL(row.url).search },
    }];
    result.fixtureDataShapes[0].state = 'cleared'; result.requiredCoverage[0].state = 'cleared';
    await verify(`session-no-reseed-${mode}`, result, ({ receipt }) => {
      assert.equal(receipt.formal.result, 'passed');
      assert.equal(sessionObservations.at(-1).present, 'false');
      if (mode === 'logout') assert.equal(sessionObservations.at(-1).cookie, 'false');
    });
  }
  const immediateStart = sessionObservations.length;
  await verify('session-immediate-clear-reload', single('immediate'), ({ receipt }) => {
    const rows = sessionObservations.slice(immediateStart);
    assert.deepEqual(rows.map(row => [row.phase,row.present]), [['first','true'],['return','false']], 'Even a reload from the first app script cannot reapply the seed');
    assert.notEqual(receipt.formal.result, 'passed', 'Interrupted initial navigation cannot masquerade as a fresh normal pass');
  });
  for (const kind of ['redirect','opaque']) {
    const result = single(kind); result.targets[0].url = new URL(`/session-${kind}`, target.url).href;
    result.fixtureDataShapes[0].route = `/session-${kind}`;
    const start = sessionObservations.length;
    await verify(`session-fail-closed-${kind}`, result, ({ receipt, report }) => {
      assert.notEqual(receipt.formal.result, 'passed'); assert.equal(report.pages[0].outcome, 'navigation_error');
      assert.match(report.pages[0].skipReason, kind === 'opaque' ? /session fixture storage unavailable/ : /session fixture initialization failed/);
      if (kind === 'redirect') assert(sessionObservations.slice(start).some(row => row.foreignEmpty === 'true'));
    });
  }
  const broken = structuredClone(config); broken.authProfiles[0].cookies[0].value = 'SESSION_PRIVATE_INVALID';
  await verify('session-failed-profile-isolation', broken, async ({ report, directory }) => {
    assert.deepEqual(report.pages.map(row => row.outcome).sort(), ['auth_setup_error','auth_setup_error','checked','checked']);
    assert(!(await readFile(join(directory,'report.json'),'utf8')).includes('SESSION_PRIVATE_'));
  });
  const legacy = single('legacy');
  legacy.cookies[0].value = 'SESSION_PRIVATE_A'; delete legacy.authProfiles[0].cookies;
  legacy.authProfiles[0].url = new URL('/session-legacy-login?identity=A', target.url).href;
  await verify('session-legacy-global-cookie-precedence', legacy, ({ receipt }) => assert.equal(receipt.formal.result, 'passed'));
  const anonymous = single('no-seed'); anonymous.authProfiles = []; delete anonymous.targets[0].authProfile; anonymous.cookies[0].value = 'SESSION_PRIVATE_A';
  await verify('session-legacy-without-profile', anonymous, ({ receipt }) => assert.equal(receipt.formal.result, 'passed'));
  const cacheDirectory = join(scratch, 'session-cache'); await mkdir(cacheDirectory, { mode: 0o700 });
  const cached = single('cache'); cached.development = { cache: { directory: cacheDirectory, dataRevision: 'session-v1' } };
  let originalKey;
  await verify('session-private-cache-write', cached, ({ receipt, report }) => {
    assert.notEqual(receipt.formal.result, 'passed');
    assert.equal(report.pages[0].cache.hit, false); originalKey = report.pages[0].cache.key;
    assert.equal(report.pages[0].cache.write.written, true);
  });
  await verify('session-private-cache-hit', cached, ({ receipt, report }) => {
    assert.notEqual(receipt.formal.result, 'passed'); assert.equal(report.pages[0].cache.hit, true);
  });
  const cookieChanged = structuredClone(cached); cookieChanged.authProfiles[0].cookies.push({ name: 'SESSION_PRIVATE_EXTRA', value: 'SESSION_PRIVATE_COOKIE_CHANGED', url: target.url });
  await verify('session-private-cookie-invalidates-cache', cookieChanged, ({ report }) => {
    assert.equal(report.pages[0].cache.hit, false); assert.notEqual(report.pages[0].cache.key, originalKey); assert.equal(report.pages[0].outcome, 'checked');
  });
  const seedChanged = structuredClone(cached); seedChanged.authProfiles[0].sessionStorage.entries[0].value = 'SESSION_PRIVATE_CHANGED';
  await verify('session-private-seed-invalidates-cache', seedChanged, ({ receipt, report }) => {
    assert.notEqual(receipt.formal.result, 'passed'); assert.equal(report.pages[0].cache.hit, false);
    assert.notEqual(report.pages[0].cache.key, originalKey); assert.notEqual(report.pages[0].outcome, 'checked');
  });
  for (const file of await readdir(cacheDirectory, { recursive: true })) if (file.endsWith('.json')) {
    assert(!(await readFile(join(cacheDirectory,file),'utf8')).includes('SESSION_PRIVATE_'), 'Private session data leaked into the evidence cache');
  }
}
async function uploadFixtures() {
  const repository = join(scratch, 'upload-repository');
  await mkdir(join(repository, 'fixtures'), { recursive: true });
  await writeFile(join(repository, 'ui.html'), uploadHtml);
  const fixtureName = 'PRIVATE_FIXTURE_NAME.csv';
  const fixtureValue = 'time,current\n0,UPLOAD_PRIVATE_SOURCE_VALUE\n';
  const fixture = `fixtures/${fixtureName}`;
  await writeFile(join(repository, fixture), fixtureValue);
  const uploaded = full('/upload'); uploaded.repoRoot = repository;
  const uploadTarget = uploaded.targets[0];
  uploadTarget.reviewInputs = [{ path: 'ui.html', kind: 'ui-code' }];
  uploadTarget.includeBase = false;
  uploadTarget.states = [{ name: 'uploaded', actions: [{ action: 'setInputFiles', selector: '#upload', value: fixture }], waitFor: { selector: '#upload-result[data-ready=true]' }, continuation: { kind: 'in-page', anchor: '#upload-result', focusWithin: '#primary' } }];
  uploaded.requiredCoverage[0].state = 'uploaded'; uploaded.fixtureDataShapes[0].state = 'uploaded'; uploaded.fixtureDataShapes[0].conditionalDom.push('#upload-result[data-ready=true]');
  let originalFingerprint;
  const verifyPrivacy = async directory => {
    for (const name of ['report.json', 'report.md', 'journey-evidence.json', 'review-queue.json', 'formal-receipt.json', 'stdout.json']) {
      const text = await readFile(join(directory, name), 'utf8');
      for (const privateValue of [fixtureName, fixtureValue.trim(), fixture]) assert(!text.includes(privateValue), `${name} leaked upload data`);
    }
  };
  await verify('upload-real-file', uploaded, async ({ receipt, report, directory }) => {
    assert.equal(receipt.formal.result, 'passed');
    assert.deepEqual(uploadedRequests.at(-1), { name: fixtureName, type: 'text/csv', content: fixtureValue, sha256: createHash('sha256').update(fixtureValue).digest('hex') });
    originalFingerprint = report.pages[0].target.intentFingerprint;
    assert(originalFingerprint, 'Upload input must bind the review fingerprint');
    await verifyPrivacy(directory);
  });
  const renamed = structuredClone(uploaded); renamed.targets[0].states[0].actions[0].value = 'fixtures/renamed.csv';
  await writeFile(join(repository, 'fixtures/renamed.csv'), fixtureValue);
  await verify('upload-renamed-fixture-fingerprint', renamed, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'passed'); assert.notEqual(report.pages[0].target.intentFingerprint, originalFingerprint); });
  const binary = Buffer.from([0x50, 0x4b, 3, 4, 0, 0xff, 0x80, 0x0a]);
  await writeFile(join(repository, 'fixtures/measurements.xlsx'), binary);
  const excel = structuredClone(uploaded); excel.targets[0].states[0].actions[0].value = 'fixtures/measurements.xlsx';
  await verify('upload-xlsx-binary-mime', excel, ({ receipt }) => { assert.equal(receipt.formal.result, 'passed'); assert.equal(uploadedRequests.at(-1).type, 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet'); assert.equal(uploadedRequests.at(-1).sha256, createHash('sha256').update(binary).digest('hex')); });
  const hidden = structuredClone(uploaded); hidden.targets[0].url += '-hidden'; hidden.fixtureDataShapes[0].route = '/upload-hidden';
  await verify('upload-hidden-native-input', hidden, ({ receipt }) => { assert.equal(receipt.formal.result, 'passed'); assert.equal(uploadedRequests.at(-1).content, fixtureValue); });
  await writeFile(join(repository, fixture), `${fixtureValue}1,3\n`);
  await verify('upload-changed-file', uploaded, async ({ receipt, report, directory }) => {
    assert.equal(receipt.formal.result, 'passed');
    assert.notEqual(report.pages[0].target.intentFingerprint, originalFingerprint);
    assert.equal(uploadedRequests.at(-1).content, `${fixtureValue}1,3\n`);
    await verifyPrivacy(directory);
  });
  await writeFile(join(repository, 'fixtures/empty.csv'), '');
  const empty = structuredClone(uploaded); empty.targets[0].states[0].actions[0].value = 'fixtures/empty.csv';
  await verify('upload-empty-file', empty, ({ receipt }) => { assert.equal(receipt.formal.result, 'passed'); assert.equal(uploadedRequests.at(-1).content, ''); });
  for (const [name, value] of [['absolute', join(repository, fixture)], ['traversal', '../outside.csv'], ['dot-segment', 'fixtures/../ui.html'], ['array', [fixture]], ['object', { name: fixtureName, buffer: fixtureValue }], ['empty-path', ''], ['backslash', 'fixtures\\example.csv'], ['private-state', '.devcoordinator/secret.csv']]) {
    const invalid = structuredClone(uploaded); invalid.targets[0].states[0].actions[0].value = value;
    const count = uploadedRequests.length;
    await verify(`upload-invalid-${name}`, invalid, null, /setInputFiles value must be one repository-relative fixture path/);
    assert.equal(uploadedRequests.length, count);
  }
  execFileSync('mkfifo', [join(repository, 'fixtures/pipe.csv')]);
  await symlink(join(repository, fixture), join(repository, 'fixtures/link.csv'));
  await symlink(join(repository, 'fixtures'), join(repository, 'linked-fixtures'));
  await writeFile(join(repository, 'fixtures/oversized.csv'), ''); await truncate(join(repository, 'fixtures/oversized.csv'), 16 * 1024 * 1024 + 1);
  for (const [name, value] of [['special-file', 'fixtures/pipe.csv'], ['symlink', 'fixtures/link.csv'], ['symlink-directory', `linked-fixtures/${fixtureName}`], ['missing', 'fixtures/absent.csv'], ['directory', 'fixtures'], ['oversized', 'fixtures/oversized.csv']]) {
    const invalid = structuredClone(uploaded); invalid.targets[0].states[0].actions[0].value = value;
    const count = uploadedRequests.length;
    await verify(`upload-rejected-${name}`, invalid, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'incomplete'); assert.equal(report.pages[0].outcome, 'journey_contract_error'); assert.match(report.pages[0].skipReason, /unchanged regular file/); });
    assert.equal(uploadedRequests.length, count);
  }
  const beforeMutation = uploadedRequests.length;
  uploadMutation = () => writeFileSync(join(repository, fixture), 'changed after planning');
  await verify('upload-changed-after-planning', uploaded, async ({ receipt, report, directory }) => { assert.equal(receipt.formal.result, 'failed'); assert.equal(uploadedRequests.length, beforeMutation); assert(report.pages[0].actionTimings.some(action => action.action === 'setInputFiles' && action.outcome === 'failed')); await verifyPrivacy(directory); });
  const authUpload = structuredClone(uploaded); authUpload.authProfiles = [{ name: 'login', url: target.url, actions: [{ action: 'setInputFiles', selector: '#upload', value: fixture }] }];
  await verify('upload-not-authentication-action', authUpload, null, /supported only in target states/);
  const absentRoot = structuredClone(uploaded); delete absentRoot.repoRoot;
  await verify('upload-requires-root', absentRoot, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'incomplete'); assert.match(report.pages[0].skipReason, /canonical repoRoot/); });
  const noContinuation = structuredClone(uploaded); delete noContinuation.targets[0].states[0].continuation;
  await verify('upload-requires-continuation', noContinuation, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'incomplete'); assert.match(report.pages[0].skipReason, /continuation/); });
  const failedAction = structuredClone(uploaded); failedAction.targets[0].states[0].actions[0].selector = '#label';
  await verify('upload-failed-action-private', failedAction, async ({ receipt, directory }) => { assert.equal(receipt.formal.result, 'failed'); await verifyPrivacy(directory); });
}
try {
  if (process.argv.includes('--continuation-precision-only')) await continuationPrecisionFixtures();
  else if (process.argv.includes('--recovery-geometry-only')) { await configurationFixtures(true); await recoveryGeometryFixtures(); }
  else if (process.argv.includes('--session-only')) await sessionFixtures();
  else {
  if (!process.argv.includes('--capture-only') && !process.argv.includes('--upload-only') && !process.argv.includes('--config-only-selftest')) await sessionFixtures();
  if (!process.argv.includes('--capture-only') && !process.argv.includes('--upload-only')) await configurationFixtures();
  if (!process.argv.includes('--config-only-selftest')) {
  if (!process.argv.includes('--capture-only')) await uploadFixtures();
  if (!process.argv.includes('--upload-only')) await capturePrivacy();
  if (!process.argv.includes('--capture-only') && !process.argv.includes('--upload-only')) {
  await recoveryGeometryFixtures();
  await continuationPrecisionFixtures();
  await Promise.all([
    verify('formal-receipt', base, ({ exitCode, receipt }) => {
      assert.equal(exitCode, 0); assert(receipt.formal, 'The verifier must emit its measured formal receipt');
      assert(['passed', 'failed', 'blocked', 'incomplete'].includes(receipt.formal.result));
    }),
    verify('double-click', { ...base, targets: [{ ...target, states: [{ name: 'editor', actions: [{ action: 'dblclick', selector: '#label' }], waitFor: { selector: '#editor:not([hidden])' }, regions: [{ selector: '#editor', role: 'primary-content', journey: 'edit-signal' }], continuation: { kind: 'in-page', anchor: '#editor-heading', focusWithin: '#editor' } }] }] }, ({ exitCode, receipt, report }) => {
      assert.equal(exitCode, 0, 'The actual double-click must open the focused editor');
      assert.equal(report.pages.length, 2); assert(report.pages.every(page => page.outcome === 'checked'));
      assert.equal(receipt.formal.result, 'incomplete', 'Undeclared shapes and geometry cannot pass');
    }),
  ]);
  const expect = expected => ({ receipt }) => assert.equal(receipt.formal.result, expected);
  for (const status of [403, 404, 503]) {
    const expectedError = full(`/http-${status}`); expectedError.targets[0].expectedHttpStatus = status;
    await verify(`expected-http-${status}`, expectedError, ({ receipt, report }) => {
      assert.equal(receipt.formal.result, 'passed');
      assert.equal(report.pages[0].status, status);
      assert.equal(report.pages[0].expectedHttpStatus, status);
      assert.equal(report.pages[0].outcome, 'checked');
      assert(report.pages[0].screenshots.viewport && report.pages[0].screenshots.fullPage);
    });
  }
  await verify('unexpected-http-error', full('/http-503'), expect('blocked'));
  const wrongStatus = full('/http-404'); wrongStatus.targets[0].expectedHttpStatus = 503;
  await verify('different-http-error', wrongStatus, expect('blocked'));
  const wrongSuccess = full(); wrongSuccess.targets[0].expectedHttpStatus = 503;
  await verify('unexpected-success-not-error-view', wrongSuccess, expect('blocked'));
  const brokenError = full('/http-503-overflow'); brokenError.targets[0].expectedHttpStatus = 503;
  await verify('http-error-layout-still-checked', brokenError, expect('failed'));
  for (const [name, value] of [['success', 200], ['string', '503'], ['range', [400, 599]], ['null', null], ['fraction', 503.5], ['outside', 600]]) {
    const invalid = full('/http-503'); invalid.targets[0].expectedHttpStatus = value;
    await verify(`http-error-invalid-${name}`, invalid, null, /expectedHttpStatus.*integer HTTP error status/);
  }
  const blanketError = full('/http-503'); blanketError.targetDefaults = { expectedHttpStatus: 503 };
  await verify('http-error-not-global-default', blanketError, null, /explicit target, not targetDefaults/);
  const stateError = full('/http-503'); stateError.targets[0].states = [{ name: 'error', expectedHttpStatus: 503 }];
  await verify('http-error-not-state-override', stateError, null, /explicit target, not a state override/);
  const unrecognizedError = full('/http-503'); unrecognizedError.targets[0].expectedHttpStatus = 503; delete unrecognizedError.targets[0].waitFor;
  await verify('http-error-needs-recovery-marker', unrecognizedError, ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'incomplete');
    assert.equal(report.pages[0].outcome, 'journey_contract_error');
    assert.equal(report.pages[0].skipReason, 'an expected HTTP error target requires an explicit waitFor.selector for its rendered recovery view');
  });
  const staleError = full('/http-503'); staleError.targets[0].expectedHttpStatus = 503; staleError.targets[0].sourceBinding.expected = 'different-error-source';
  await verify('http-error-needs-source-identity', staleError, ({ receipt, report }) => { assert.notEqual(receipt.formal.result, 'passed'); assert.equal(report.pages[0].outcome, 'stale_deployment'); });
  await verify('fresh-complete', full(), async ({ receipt, report, directory }) => {
    assert.equal(receipt.formal.result, 'passed'); assert.equal(receipt.formal.freshComplete, true);
    assert.equal(receipt.formal.coverage.requiredCells, 1); assert.equal(receipt.formal.coverage.checkedCells, 1);
    for (const key of ['sourceSha256', 'configSha256', 'verifierSha256', 'planSha256', 'candidateId']) assert.match(receipt.formal[key], /^[a-f0-9]{64}$/);
    assert.match(receipt.formal.sourceDigestScope, /not the complete repository/);
    const manifest = await readFile(join(directory, 'formal-artifacts.json'));
    assert.equal(createHash('sha256').update(manifest).digest('hex'), receipt.formal.evidence.manifestSha256);
    assert(report.pages[0].metrics.handoff.geometry.every(row => row.status === 'passed'));
    measuredUserAgent = report.pages[0].metrics.handoff.browser.userAgent;
  });
  await verify('reserved-hidden-column', full('/grid-reserved'), ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'failed'); const row = report.pages[0].metrics.handoff.geometry.find(row => row.kind === 'hidden-navigation-track');
    assert.equal(row.measurements.visible, false); assert.equal(row.measurements.reservedSize, 200); assert.equal(row.status, 'failed');
  });
  const missingShape = full(); missingShape.fixtureDataShapes[0].conditionalDom.push('#absent-shape');
  await verify('missing-shape', missingShape, expect('incomplete'));
  const missingGeometry = full(); missingGeometry.targets[0].geometryAssertions[1].selector = '#absent-heading'; missingGeometry.targets[0].geometryAssertions[1].allowance = { reason: 'Allowance cannot excuse missing evidence' };
  await verify('missing-selector-with-allowance', missingGeometry, expect('incomplete'));
  const unresolvedTrack = full(); unresolvedTrack.targets[0].geometryAssertions.at(-1).track.selector = '#primary';
  await verify('unresolved-grid-track', unresolvedTrack, expect('incomplete'));
  const partial = full(); partial.targets[0].geometryAssertions = partial.targets[0].geometryAssertions.slice(0, 1);
  await verify('partial-geometry', partial, expect('incomplete'));
  await verify('character-wrapping', full('/character-wrap'), expect('failed'));
  await verify('single-character-guard', full('/single-character'), expect('passed'));
  const allowed = full('/character-wrap'); allowed.targets[0].geometryAssertions.find(row => row.kind === 'no-character-wrapping').allowance = { reason: 'Intentional stacked fixture title' };
  await verify('intentional-stacked-text', allowed, ({ report }) => assert.equal(report.pages[0].metrics.handoff.geometry.find(row => row.kind === 'no-character-wrapping').status, 'allowed'));
  for (const [name, route, kind] of [['clipping', '/clipped', 'clipping'], ['document-overflow', '/overflow', 'document-horizontal-overflow'], ['initial-viewport-placement', '/offscreen', 'initial-viewport-placement']]) await verify(name, full(route), ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'failed'); assert.equal(report.pages[0].metrics.handoff.geometry.find(row => row.kind === kind).status, 'failed');
  });
  const width = full(); width.targets[0].geometryAssertions[0].minWidth = 2000;
  await verify('primary-width', width, expect('failed'));
  const required = full(); required.requiredCoverage[0].viewport = 'missing';
  await verify('missing-required-cell', required, expect('incomplete'));
  const development = full(); development.development = { changedPaths: ['rust/tooling/tests/formal_handoff.mjs'] };
  await verify('development-subset', development, expect('incomplete'));
  const reported = full(); reported.reportedBrowserStates = [{ id: 'reported', target: 'handoff', state: 'base', theme: 'light', viewport: base.viewports[0], device: 'desktop', userAgent: 'unknown-browser', auth: 'anonymous', zoom: 1 }];
  await verify('reported-browser-mismatch', reported, expect('incomplete'));
  reported.reportedBrowserStates[0].zoom = 2;
  await verify('browser-zoom-not-css-zoom', reported, expect('incomplete'));
  const exactBrowser = full(); exactBrowser.reportedBrowserStates = [{ id: 'exact', target: 'handoff', state: 'base', theme: 'light', viewport: base.viewports[0], device: 'desktop', userAgent: measuredUserAgent, auth: 'anonymous', zoom: 1 }];
  await verify('reported-browser-exact', exactBrowser, expect('passed'));
  const requestedEngine = structuredClone(exactBrowser); requestedEngine.reportedBrowserStates[0].engine = 'webkit';
  await verify('unsupported-explicit-renderer', requestedEngine, expect('incomplete'));
  requestedEngine.reportedBrowserStates[0].engine = 'chromium';
  await verify('measured-explicit-renderer', requestedEngine, expect('passed'));
  const noRequired = structuredClone(exactBrowser); noRequired.requiredCoverage = [];
  await verify('reported-browser-without-required-cell', noRequired, expect('incomplete'));
  const phone = full(); phone.viewports = [{ name: 'phone', device: 'iPhone 13' }]; phone.requiredCoverage = [{ target: 'handoff', state: 'base', viewport: 'phone', width: 390 }];
  await verify('actual-phone-context', phone, ({ receipt, report }) => {
    assert.equal(receipt.formal.result, 'passed'); const browser = report.pages[0].metrics.handoff.browser;
    assert.equal(browser.device, 'iPhone 13'); assert.equal(browser.emulatedDevice, true); assert.equal(browser.isMobile, true); assert(browser.touchPoints > 0); assert.equal(browser.engine, 'chromium');
  });
  const query = full('/?fixture=owner'); await verify('exact-query-route', query, expect('passed'));
  query.fixtureDataShapes[0].route = '/'; await verify('missing-query-shape', query, expect('incomplete'));
  const ambiguous = full(); ambiguous.fixtureDataShapes[0].target = undefined; ambiguous.targets.push({ ...ambiguous.targets[0], name: 'other' });
  await verify('ambiguous-shape-target', ambiguous, expect('incomplete'));
  const mismatchedSource = full(); mismatchedSource.targets[0].sourceBinding.expected = 'stale-source';
  await verify('stale-source-binding', mismatchedSource, expect('incomplete'));
  const cacheDirectory = await mkdtemp(join(tmpdir(), 'formal-handoff-cache-'));
  const cached = full(); cached.development = { cache: { directory: cacheDirectory, dataRevision: 'v1' } };
  await verify('cache-first', cached, expect('incomplete'));
  await verify('cache-hit', cached, ({ receipt, report }) => { assert.equal(receipt.formal.result, 'incomplete'); assert.equal(report.pages[0].cache.hit, true); });
  const auth = full(); auth.targets[0].authProfile = 'signed-in'; auth.authProfiles = [{ name: 'signed-in', url: target.url, actions: [{ action: 'click', selector: '#missing-auth', timeoutMs: 10 }] }];
  await verify('auth-unavailable', auth, expect('blocked'));
  const privacy = full(); privacy.targets[0].states = [{ name: 'private-input', actions: [{ action: 'dblclick', selector: '#label' }, { action: 'fill', selector: '#declaration', value: 'SECRET_HANDOFF_INPUT' }], waitFor: { selector: '#editor:not([hidden])' }, regions: [{ selector: '#editor', role: 'primary-content', journey: 'edit-signal' }], continuation: { kind: 'in-page', anchor: '#editor-heading', focusWithin: '#editor' }, geometryAssertions: geometry('#editor', '#editor-heading', '#editor-label') }];
  privacy.fixtureDataShapes.push({ id: 'editor', revision: 'v1', target: 'handoff', route: '/', state: 'private-input', conditionalDom: ['#editor', '#declaration'], layoutEffect: 'Revealed declaration editor' });
  privacy.requiredCoverage.push({ target: 'handoff', state: 'private-input', viewport: 'desktop', width: 1440 });
  await verify('private-input-redacted', privacy, async ({ receipt, directory }) => {
    assert.equal(receipt.formal.result, 'passed');
    for (const name of ['report.json', 'report.md', 'journey-evidence.json', 'review-queue.json', 'formal-receipt.json']) assert(!(await readFile(join(directory, name), 'utf8')).includes('SECRET_HANDOFF_INPUT'), name);
  });
  const invalidDirectory = join(scratch, 'not-a-directory'); await writeFile(invalidDirectory, 'disposable fixture');
  const artifact = full(); artifact.screenshotDir = invalidDirectory;
  await verify('artifact-unavailable', artifact, expect('blocked'));
  const setup = full(); setup.targets[0].states = [{ name: 'unsupported', actions: [{ action: 'evaluate', selector: '#label' }] }];
  await verify('setup-blocked', setup, expect('blocked'));
  const failedAction = full(); failedAction.targets[0].states = [{ name: 'failed-action', actions: [{ action: 'dblclick', selector: '#absent-label', timeoutMs: 10 }], continuation: { kind: 'in-page', anchor: '#editor-heading', focusWithin: '#editor' } }];
  await verify('rendered-action-failed', failedAction, expect('failed'));
  }
  }
  }
} finally { await new Promise(resolve => server.close(resolve)); }
await writeFile(join(scratch, 'results.json'), JSON.stringify({ results }, null, 2));
console.log(JSON.stringify({ scratch, results }));
assert(results.every(result => result.passed), 'Formal handoff regression fixtures failed');
