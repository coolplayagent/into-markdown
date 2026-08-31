"""Recreate hardlinked staging from the public identity manifest and the user's corpus."""
from pathlib import Path
import argparse,hashlib,json,os

ROOT=Path(__file__).resolve().parents[1]

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--dataset',type=Path,required=True)
    parser.add_argument('--identity-manifest',type=Path,default=ROOT/'input-manifest.json')
    args=parser.parse_args()
    manifest=json.loads(args.identity_manifest.read_text(encoding='utf-8'))
    dataset=args.dataset.resolve();items=[]
    for original in manifest['items']:
        item=dict(original);source=(dataset/item['relativePath']).resolve()
        assert source.is_relative_to(dataset)
        with source.open('rb') as stream:digest=hashlib.file_digest(stream,'sha256').hexdigest()
        assert digest==item['sha256'] and source.stat().st_size==item['bytes'],str(source)
        destination=ROOT/'inputs'/item['extension']/f'{item["id"]}.{item["extension"]}'
        destination.parent.mkdir(exist_ok=True,parents=True)
        if not destination.exists():os.link(source,destination)
        item.update(inputPath=str(destination),originalPath=str(source))
        oracle=ROOT/'oracles'/(item['id']+'.json')
        if oracle.exists():item['oraclePath']=str(oracle)
        items.append(item)
    manifest.update(datasetRoot=str(dataset),items=items)
    (ROOT/'manifest.json').write_text(json.dumps(manifest,ensure_ascii=False,indent=2),encoding='utf-8')
    print(f'Verified and staged {len(items)} inputs; source files unchanged.')

if __name__=='__main__':main()
