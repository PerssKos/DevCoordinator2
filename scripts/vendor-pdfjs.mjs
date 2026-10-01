// Reproduce the reviewed static PDF renderer from the pinned official package.
// Obtain with: npm pack pdfjs-dist@6.3.289 --ignore-scripts
// Extract into a new temporary directory, then pass its package directory here.
import fs from 'node:fs/promises';
import path from 'node:path';
import {createHash} from 'node:crypto';
const source=path.resolve(process.argv[2]||'');
const root=path.resolve(path.dirname(new URL(import.meta.url).pathname),'..');
const pkg=JSON.parse(await fs.readFile(path.join(source,'package.json')));
if(pkg.name!=='pdfjs-dist'||pkg.version!=='6.3.289')throw new Error('Expected official pdfjs-dist 6.3.289 package');
const output=path.join(root,'console/vendor/pdfjs');
const files=['build/pdf.min.mjs','build/pdf.worker.min.mjs','LICENSE'];
for(const directory of ['cmaps','standard_fonts','wasm'])for(const entry of await fs.readdir(path.join(source,directory),{withFileTypes:true})){
  if(!entry.isFile())throw new Error('Unexpected package layout');
  files.push(directory+'/'+entry.name);
}
const manifest={package:pkg.name,version:pkg.version,source:'https://registry.npmjs.org/pdfjs-dist/-/pdfjs-dist-6.3.289.tgz',license:'Apache-2.0',files:{}};
for(const file of files){const bytes=await fs.readFile(path.join(source,file));const target=file.startsWith('build/')?file.slice(6):file==='LICENSE'?'LICENSE.txt':file;await fs.mkdir(path.dirname(path.join(output,target)),{recursive:true});await fs.writeFile(path.join(output,target),bytes);manifest.files[target]={bytes:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex')};}
await fs.writeFile(path.join(output,'source.json'),JSON.stringify(manifest,null,2)+'\n');
console.log(JSON.stringify({version:pkg.version,files:files.length}));
