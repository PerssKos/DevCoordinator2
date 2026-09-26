// Unfiltered source-equality review for localization admission. Unlike the
// heuristic content audit, this includes short labels and technical-looking
// values; reviewers explicitly classify every match before enabling a locale.
import fs from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
const root=fileURLToPath(new URL('../../',import.meta.url));
const dir=path.join(root,'console/locales');
const manifest=JSON.parse(await fs.readFile(path.join(dir,'manifest.json')));
const requested=process.argv.includes('--locale')?process.argv[process.argv.indexOf('--locale')+1]:null;
const sourceEntry=manifest.locales.find(e=>e.tag===manifest.sourceLocale);
const read=async entry=>{const out={};for(const [ns,files] of Object.entries(entry.files)){out[ns]={};for(const f of files)Object.assign(out[ns],JSON.parse(await fs.readFile(path.join(dir,f),'utf8')))}return out};
const source=await read(sourceEntry); const locales=manifest.locales.filter(e=>e.tag!=='en'&&(!requested||e.tag===requested));
for(const entry of locales){const target=await read(entry);const matches=[];for(const [ns,msgs] of Object.entries(source))for(const [id,msg] of Object.entries(msgs)){const value=target[ns]?.[id];if(typeof msg==='string'&&value===msg)matches.push({namespace:ns,id,value});else if(msg&&typeof msg==='object'&&value&&typeof value==='object'){for(const form of Object.keys(msg.forms||{}))if(value.forms?.[form]===msg.forms[form])matches.push({namespace:ns,id:`${id}.${form}`,value:value.forms[form]});}}console.log(JSON.stringify({locale:entry.tag,status:entry.status,sourceEqual:matches.length,matches},null,2));}
