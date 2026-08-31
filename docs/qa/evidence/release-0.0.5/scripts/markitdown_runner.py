import json,pathlib,socket,sys,time

def denied(*a,**kw):raise OSError('Network disabled for local benchmark')
socket.create_connection=denied
socket.socket.connect=denied
socket.socket.connect_ex=denied
from markitdown import MarkItDown

manifest,output,result=map(pathlib.Path,sys.argv[1:])
output.mkdir(parents=True,exist_ok=True)
converter=MarkItDown(enable_plugins=False)
results=[]
for item in json.loads(manifest.read_text(encoding='utf-8'))['items']:
    dest=output/f"{item['id']}.md";start=time.perf_counter()
    try:
        md=converter.convert_local(item['inputPath']).text_content
        dest.write_text(md,encoding='utf-8',newline='\n')
        row={'id':item['id'],'status':'success' if md.strip() else 'empty','output':str(dest),'outputBytes':dest.stat().st_size}
    except BaseException as e:
        row={'id':item['id'],'status':'failed','errorCode':type(e).__name__,'error':str(e)[:6000]}
    row['processingDurationMs']=(time.perf_counter()-start)*1000
    results.append(row)
    result.write_text(json.dumps({'results':results},ensure_ascii=False),encoding='utf-8')
