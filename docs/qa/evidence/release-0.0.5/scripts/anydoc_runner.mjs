import fs from 'node:fs';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
import {performance} from 'node:perf_hooks';
globalThis.fetch=()=>{throw new Error('Network disabled for local benchmark');};
const [manifest,output,result,module]=process.argv.slice(2);
const {toMarkdown}=await import(pathToFileURL(module).href);
fs.mkdirSync(output,{recursive:true});
const results=[];
for(const item of JSON.parse(fs.readFileSync(manifest,'utf8')).items){
  const dest=path.join(output,`${item.id}.md`),start=performance.now();
  let row;
  try{
    const md=await toMarkdown(item.inputPath,{ocr:'reject'});
    fs.writeFileSync(dest,md,'utf8');
    row={id:item.id,status:md.trim()?'success':'empty',output:dest,outputBytes:Buffer.byteLength(md)};
  }catch(e){row={id:item.id,status:'failed',errorCode:e?.code??e?.name,error:String(e?.message??e).slice(0,6000)};}
  row.processingDurationMs=performance.now()-start;results.push(row);
  fs.writeFileSync(result,JSON.stringify({results}),'utf8');
}
