import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
import {after,before,test} from 'node:test';
import {pageVerifier} from '../../../skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs';
const require=createRequire(import.meta.url);
const {chromium}=require(`${process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES}/playwright`);
let browser;
before(async()=>{browser=await chromium.launch({headless:true});});
after(async()=>{await browser?.close();});

for(const declared of [true,false])test(`gutter reachability respects ${declared?'declared':'undeclared'} popup coverage`,async()=>{
  const page=await browser.newPage({viewport:{width:390,height:844}});
  try{
    await page.setContent(`<!doctype html><style>body{margin:0;color:#111;background:white}.scroll{position:relative;display:flex;width:210px;height:180px;overflow:auto;line-height:24px;font:14px monospace}.gutter{position:sticky;left:0;width:45px;flex:none;background:#aaa;z-index:2}.content{width:600px;flex:none}.word{display:inline-block;margin-left:60px;margin-top:80px;width:30px;height:24px}.popup{position:fixed;left:55px;top:65px;width:150px;height:80px;background:#ddd;z-index:3}</style><div class="scroll"><div class="gutter">1<br>2<br>3<br>4<br>5</div><div class="content"><span class="word">abc</span></div></div><aside class="popup" role="dialog" aria-label="Preview" ${declared?'data-ui-contextual-overlay="Temporary preview"':''}>Preview</aside>`);
    await page.locator('.scroll').evaluate(element=>element.scrollLeft=90);
    const before=await page.locator('.scroll').evaluate(element=>element.scrollLeft);
    await page.evaluate(()=>{window.__FORMAL_WEB_UI_CONFIG__={rules:{strictTruncation:false},inspectThemePalette:false};});
    const result=await page.evaluate(pageVerifier);
    const blocked=result.findings.filter(finding=>['occluded','partially-occluded'].includes(finding.rule)&&finding.selector.includes('word'));
    if(declared){assert.deepEqual(blocked,[]);assert.ok(result.findings.some(finding=>finding.rule==='allowed-contextual-overlay'&&finding.selector.includes('word')));}
    else assert.ok(blocked.some(finding=>finding.severity==='critical'));
    assert.equal(await page.locator('.scroll').evaluate(element=>element.scrollLeft),before);
  }finally{await page.close();}
});
