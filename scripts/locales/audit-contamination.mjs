// Find likely mixed-language product messages. This is a review aid, not a
// language classifier: brand names, identifiers and user-authored data stay out
// of the catalogs and are not examined here.
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const root=fileURLToPath(new URL('../../',import.meta.url));
const localeRoot=path.join(root,'console/locales');
const stopWords=['the','and','or','why','needs','you','your','could','not','load','loading','available','unavailable','response','history','is','are','to','view','show','open','remove','critical','dismissed','incident','reported','health','system','trends','host','capacity','search','find','details','logs','next','step','active','dismiss','restore','managed','storage','memory','free','average','scale','this','that','with','for','from','of','in','on','by','before','after','still','here','finish','action','already','recorded','complete','current','previous','matching','period','plan','task','repository','resources','review','refresh','try','again'];
const phrasePatterns=[/your comment and marks are still here/i,/to attach this comment/i,/the text label before saving/i,/wait for it to finish/i,/could not be confirmed/i,/is available/i,/is unavailable/i,/needs attention/i,/the stored incident/i,/open plan task/i,/view delivery evidence/i,/loading screenshot/i,/no .* available/i,/this project uses shared/i,/interactive plan timeline/i,/showing .* of .* open tasks/i];
const tokenRe=/[A-Za-z][A-Za-z'-]*/g;
export function contaminationReason(locale, namespace, id, value) {
  // Regression guards for observed donor-template copying. These do not claim
  // general language detection; names, cognates and reviewed loans remain valid.
  if (namespace === 'evidence' && locale !== 'de') {
    if (id === 'changesRequested' && /Änderung(?:en)? angefordert/u.test(value)) return 'german-donor-template';
    if (id === 'commentAdded' && locale !== 'lb' && /\bzu\s+\{count\}/u.test(value)) return 'german-donor-template';
  }
  const words=(value.match(tokenRe)||[]).map(x=>x.toLowerCase()).filter(x=>stopWords.includes(x));
  if (phrasePatterns.some(p=>p.test(value))) return 'phrase';
  return new Set(words).size>=3 ? 'english-token-cluster' : null;
}

async function audit() {
  const manifest=JSON.parse(await fs.readFile(path.join(localeRoot,'manifest.json')));
  const requested=process.argv.includes('--locale')?process.argv[process.argv.indexOf('--locale')+1]:null;
  const entries=manifest.locales.filter(e=>e.tag!==manifest.sourceLocale&&(!requested||e.tag===requested));
  let totalCandidates=0;
  for(const entry of entries) {
    const matches=[];
    for(const [namespace,files] of Object.entries(entry.files)) for(const file of files) {
      const data=JSON.parse(await fs.readFile(path.join(localeRoot,file),'utf8'));
      for(const [id,msg] of Object.entries(data)) {
        const variants=typeof msg==='string' ? [[null,msg]] : Object.entries(msg?.forms||{});
        for(const [form,value] of variants) {
          if(typeof value!=='string') continue;
          const reason=contaminationReason(entry.tag,namespace,id,value);
          if(reason) matches.push({namespace,id,...(form?{form}:{}),value,reason});
        }
      }
    }
    totalCandidates+=matches.length;
    console.log(JSON.stringify({locale:entry.tag,status:entry.status,candidates:matches.length,matches},null,2));
  }
  if(process.argv.includes('--strict') && totalCandidates) process.exitCode=1;
}
if(process.argv[1] && path.resolve(process.argv[1])===fileURLToPath(import.meta.url)) await audit();
