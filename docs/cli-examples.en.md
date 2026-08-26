# Executable CLI and format examples

[中文](cli-examples.md)

Every example invokes `into-md` directly. CI compares coverage with the real `--help` tree and
`formats --json` catalog.

## Public commands

<!-- cli-example: convert -->
- Convert: `into-md report.docx -o report.md --conflict error`
<!-- cli-example: ui -->
- Workbench: `into-md ui --no-open`
<!-- cli-example: formats -->
- Formats: `into-md formats --json`
<!-- cli-example: formats show -->
- Format details: `into-md formats show pdf --json`
<!-- cli-example: formats detect -->
- Detect: `into-md formats detect report.pdf --json`
<!-- cli-example: capabilities -->
- Capabilities: `into-md capabilities --json`
<!-- cli-example: capabilities list -->
- Explicit list: `into-md capabilities list --json`
<!-- cli-example: capabilities show -->
- Capability details: `into-md capabilities show ocr --json`
<!-- cli-example: capabilities verify -->
- Verify capability: `into-md capabilities verify ocr --json`
<!-- cli-example: capabilities use -->
- Select route: `into-md capabilities use ocr --source off --scope project`
<!-- cli-example: capabilities reset -->
- Reset route: `into-md capabilities reset ocr --scope project`
<!-- cli-example: setup -->
- Setup entry: `into-md setup --help`
<!-- cli-example: setup ocr -->
- OCR: `into-md setup ocr`
<!-- cli-example: setup media -->
- Media: `into-md setup media`
<!-- cli-example: transcript -->
- Transcript processing: `into-md transcript --help`
<!-- cli-example: transcript relabel -->
- Rename speakers: `into-md transcript relabel document-ir.json --speaker SPEAKER_00=Alice -o meeting.md`
<!-- cli-example: providers -->
- Providers: `into-md providers --json`
<!-- cli-example: providers show -->
- Provider details: `into-md providers show team --json`
<!-- cli-example: providers add -->
- Add provider: `into-md providers add team --type openai-compatible --base-url https://api.example/v1 --model model-name --api-key-env TEAM_API_KEY --capability image-description --scope project`
<!-- cli-example: providers remove -->
- Remove provider: `into-md providers remove team --scope project`
<!-- cli-example: providers set-default -->
- Default provider: `into-md providers set-default team --scope project`
<!-- cli-example: providers capabilities -->
- Provider capabilities: `into-md providers capabilities team --json`
<!-- cli-example: providers test -->
- Provider probe: `into-md providers test team --allow-network --allow-host api.example`
<!-- cli-example: plugins -->
- Plugins: `into-md plugins --json`
<!-- cli-example: plugins show -->
- Plugin details: `into-md plugins show example.plugin --json`
<!-- cli-example: plugins install -->
- Install plugin: `into-md plugins install ./example.imp --scope project`
<!-- cli-example: plugins verify -->
- Verify plugin: `into-md plugins verify example.plugin --json --scope project`
<!-- cli-example: plugins enable -->
- Enable plugin: `into-md plugins enable example.plugin --scope project`
<!-- cli-example: plugins disable -->
- Disable plugin: `into-md plugins disable example.plugin --scope project`
<!-- cli-example: plugins remove -->
- Remove plugin: `into-md plugins remove example.plugin --scope project`
<!-- cli-example: plugins run -->
- Run plugin: `into-md plugins run example.plugin source.bin --input-format txt --scope project`
<!-- cli-example: config -->
- Config entry: `into-md config --help`
<!-- cli-example: config paths -->
- Config paths: `into-md config paths --json`
<!-- cli-example: config show -->
- Config content: `into-md config show --resolved --format json`
<!-- cli-example: config init -->
- Initialize config: `into-md config init --scope project`
<!-- cli-example: config validate -->
- Validate config: `into-md config validate into-markdown.toml`
<!-- cli-example: config get -->
- Read config: `into-md config get output.conflict`
<!-- cli-example: config set -->
- Set config: `into-md config set output.conflict '"error"' --scope project`
<!-- cli-example: config unset -->
- Remove config: `into-md config unset output.conflict --scope project`
<!-- cli-example: config profile -->
- Profile entry: `into-md config profile --help`
<!-- cli-example: config profile list -->
- Profiles: `into-md config profile list`
<!-- cli-example: config profile create -->
- Create profile: `into-md config profile create review --scope project`
<!-- cli-example: config profile remove -->
- Remove profile: `into-md config profile remove review --scope project`
<!-- cli-example: doctor -->
- Diagnose: `into-md doctor --json`
<!-- cli-example: completions -->
- Completions: `into-md completions bash`
<!-- cli-example: version -->
- Version: `into-md version --json`

## Currently available formats

<!-- format-example: pdf -->
- PDF: `into-md report.pdf --format pdf -o report.md --conflict error`
<!-- format-example: doc -->
- DOC: `into-md legacy.doc --format doc -o legacy.md --conflict error`
<!-- format-example: docx -->
- DOCX: `into-md report.docx --format docx -o report.md --conflict error`
<!-- format-example: ppt -->
- PPT: `into-md legacy.ppt --format ppt -o legacy.md --conflict error`
<!-- format-example: pptx -->
- PPTX: `into-md slides.pptx --format pptx -o slides.md --conflict error`
<!-- format-example: xls -->
- XLS: `into-md legacy.xls --format xls -o legacy.md --conflict error`
<!-- format-example: xlsx -->
- XLSX: `into-md workbook.xlsx --format xlsx -o workbook.md --conflict error`
<!-- format-example: odt -->
- ODT: `into-md document.odt --format odt -o document.md --conflict error`
<!-- format-example: ods -->
- ODS: `into-md sheet.ods --format ods -o sheet.md --conflict error`
<!-- format-example: odp -->
- ODP: `into-md slides.odp --format odp -o slides.md --conflict error`
<!-- format-example: rtf -->
- RTF: `into-md document.rtf --format rtf -o document.md --conflict error`
<!-- format-example: epub -->
- EPUB: `into-md book.epub --format epub -o book.md --conflict error`
<!-- format-example: text -->
- TXT: `into-md notes.txt --format text -o notes.md --conflict error`
<!-- format-example: markdown -->
- Markdown: `into-md notes.md --format markdown -o normalized.md --conflict error`
<!-- format-example: html -->
- HTML: `into-md page.html --format html -o page.md --conflict error`
<!-- format-example: csv -->
- CSV: `into-md table.csv --format csv -o table.md --conflict error`
<!-- format-example: tsv -->
- TSV: `into-md table.tsv --format tsv -o table.md --conflict error`
<!-- format-example: json -->
- JSON: `into-md data.json --format json -o data.md --conflict error`
<!-- format-example: xml -->
- XML: `into-md data.xml --format xml -o data.md --conflict error`
<!-- format-example: feed -->
- RSS/Atom: `into-md feed.xml --format feed -o feed.md --conflict error`
<!-- format-example: ipynb -->
- Notebook: `into-md notebook.ipynb --format ipynb -o notebook.md --conflict error`
<!-- format-example: image -->
- Image: `into-md scan.png --format image --ocr always -o scan.md --conflict error`
<!-- format-example: zip -->
- ZIP: `into-md archive.zip --format zip -o archive.md --conflict error`
<!-- format-example: outlook-msg -->
- MSG: `into-md message.msg --format outlook-msg -o message.md --conflict error`
<!-- format-example: audio -->
- Audio: `into-md meeting.wav --format audio --ai audio-transcription=only -o meeting.md --conflict error`
<!-- format-example: video -->
- Video: `into-md meeting.webm --format video --ai audio-transcription=only -o meeting.md --conflict error`

See the [CLI](cli.md) and [format matrix](formats.md).
