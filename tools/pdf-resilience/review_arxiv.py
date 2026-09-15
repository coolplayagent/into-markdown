#!/usr/bin/env python3
"""Build an offline, page-addressed source/output review packet from a corpus audit."""
import argparse
import html
import json
import os
import pathlib
import re
import urllib.parse


def relative(path, root):
    return urllib.parse.quote(os.path.relpath(path.resolve(), root.resolve()), safe='/')


def build(args):
    manifest = json.loads(args.manifest.read_text())
    audit = json.loads(args.audit.read_text())
    cases = {(c['id'], c['ocr']): c for c in audit['cases']}
    args.output.mkdir(parents=True, exist_ok=True)
    links = []
    for paper in manifest['papers']:
        choices = []
        for mode in ('auto', 'off'):
            case = cases[(paper['id'], mode)]
            document = args.results / paper['id'] / mode / 'document.md'
            parts = re.split(r'<a id="pdf-page-(\d+)"></a>', document.read_text())
            bodies = {int(parts[i]): parts[i + 1] for i in range(1, len(parts), 2)}
            for page in case['pages']:
                number = page['page']
                body = bodies.get(number, document.read_text() if len(parts) == 1 else '')
                images = [relative(document.parent / urllib.parse.unquote(path), args.output)
                          for path in re.findall(r'!\[[^\]]*\]\(<([^>]+)>\)', body)]
                source = args.inventory / paper['id'] / f'page-{number}.png'
                choices.append(dict(mode=mode, page=number, representation=page['representation'],
                    required=page['reviewRequired'], text=body, images=images,
                    sourceImage=relative(source, args.output) if source.exists() else None))
        payload = dict(title=paper['title'], pages=choices,
            source=relative(args.sources / f"{paper['id']}v{paper['version']}.pdf", args.output),
            sourceSha256=paper['sha256'], binarySha256=audit['binarySha256'])
        name = paper['id'] + '.html'
        (args.output / name).write_text(PAGE.replace('__DATA__', json.dumps(payload, ensure_ascii=True).replace('<', '\\u003c')))
        links.append(f'<li><a href="{name}">{html.escape(paper["id"] + " · " + paper["title"])}</a> · {paper["pages"]} 页</li>')
    (args.output / 'index.html').write_text('<!doctype html><meta charset="utf-8"><title>PDF 原件与结果核查</title><h1>100 篇 PDF 原件与结果核查</h1><p>每篇可切换 OCR 模式和页码。需要核查的页面已标记；页面展示不自动计为已核查。</p><ol>' + ''.join(links) + '</ol>')


PAGE = '''<!doctype html><html lang="zh-CN"><meta charset="utf-8"><title>PDF 页面核查</title>
<style>body{font:16px system-ui;margin:20px;background:#f6f7f9;color:#17212b}header{position:sticky;top:0;background:#f6f7f9;padding:12px;z-index:1}main{display:grid;grid-template-columns:1fr 1fr;gap:20px}section{background:white;padding:16px;min-width:0}img{max-width:100%;background:white}iframe{width:100%;height:90vh;border:0}pre{white-space:pre-wrap;overflow-wrap:anywhere;font:14px/1.6 ui-monospace,monospace}small{overflow-wrap:anywhere}select{padding:8px;font:inherit}h1{font-size:22px}</style>
<header><a href="index.html">全部论文</a><h1 id="title"></h1><select id="page"></select> <a id="original" target="_blank">打开原 PDF 对应页</a><p id="status"></p></header>
<main><section><h2>原件</h2><div id="source"></div></section><section><h2>交付内容</h2><div id="images"></div><pre id="text"></pre></section></main><small id="authority"></small>
<script>const data=__DATA__; const byId=id=>document.getElementById(id);
byId('title').textContent=data.title;
data.pages.forEach((p,i)=>{const o=document.createElement('option');o.value=i;o.textContent=`OCR ${p.mode} · 第 ${p.page} 页 · ${p.representation}${p.required?' · 需要核查':''}`;byId('page').append(o)});
function show(){const p=data.pages[Number(byId('page').value)];const url=data.source+'#page='+p.page;byId('original').href=url;byId('source').replaceChildren();const original=document.createElement(p.sourceImage?'img':'iframe');original.src=p.sourceImage||url;byId('source').append(original);byId('images').replaceChildren();p.images.forEach(url=>{const image=document.createElement('img');image.src=url;byId('images').append(image)});byId('text').textContent=p.text;byId('status').textContent='核查状态：尚未记录完成';}
byId('page').addEventListener('change',show);byId('authority').textContent='原件 SHA-256: '+data.sourceSha256+' · 程序 SHA-256: '+data.binarySha256;show();</script></html>'''


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('manifest', 'audit', 'results', 'sources', 'inventory', 'output'):
        parser.add_argument('--' + name, type=pathlib.Path, required=True)
    build(parser.parse_args())
