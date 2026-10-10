// Disposable self-test observation only: no cookies, URLs, page contents or PIDs.
const {writeFileSync}=require('node:fs');
const {join}=require('node:path');
const {chromium}=require(join(process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES,'playwright'));
const report={browsers:[],contexts:[],attempts:0};
const launch=chromium.launch.bind(chromium);
chromium.launch=async(...args)=>{
  report.attempts++;
  if(String(report.attempts)===process.env.FORMAL_BROWSER_FAIL_LAUNCH)throw new Error('Injected worker browser launch failure');
  const browser=await launch(...args),row={id:report.browsers.length+1,closed:false};
  report.browsers.push(row);
  const create=browser.newContext.bind(browser),close=browser.close.bind(browser);
  browser.newContext=async(...contextArgs)=>{
    const context=await create(...contextArgs);
    const item={browser:row.id,cell:Boolean(contextArgs[0]?.viewport),start:performance.now(),end:null};
    report.contexts.push(item);
    const closeContext=context.close.bind(context);
    context.close=async(...closeArgs)=>{const result=await closeContext(...closeArgs);item.end=performance.now();return result;};
    return context;
  };
  browser.close=async(...closeArgs)=>{const result=await close(...closeArgs);row.closed=true;return result;};
  return browser;
};
process.once('exit',()=>writeFileSync(process.env.FORMAL_BROWSER_LIFECYCLE_FILE,JSON.stringify(report),{mode:0o600}));
