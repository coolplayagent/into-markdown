# CLI 与格式可执行示例

[English](cli-examples.en.md)

所有示例直接调用 `into-md`。CI 从真实 `--help` 命令树和 `formats --json` 反向核对覆盖率。

## 公开命令

<!-- cli-example: convert -->
- 转换：`into-md report.docx -o report.md --conflict error`
<!-- cli-example: ui -->
- 工作台：`into-md ui --no-open`
<!-- cli-example: formats -->
- 格式列表：`into-md formats --json`
<!-- cli-example: formats show -->
- 格式详情：`into-md formats show pdf --json`
<!-- cli-example: formats detect -->
- 检测：`into-md formats detect report.pdf --json`
<!-- cli-example: capabilities -->
- 能力列表：`into-md capabilities --json`
<!-- cli-example: capabilities list -->
- 显式列表：`into-md capabilities list --json`
<!-- cli-example: capabilities show -->
- 能力详情：`into-md capabilities show ocr --json`
<!-- cli-example: capabilities verify -->
- 验证能力：`into-md capabilities verify ocr --json`
<!-- cli-example: capabilities use -->
- 选择路由：`into-md capabilities use ocr --source off --scope project`
<!-- cli-example: capabilities reset -->
- 重置路由：`into-md capabilities reset ocr --scope project`
<!-- cli-example: setup -->
- 准备入口：`into-md setup --help`
<!-- cli-example: setup ocr -->
- OCR：`into-md setup ocr`
<!-- cli-example: setup media -->
- 语音：`into-md setup media`
<!-- cli-example: transcript -->
- 转写后处理：`into-md transcript --help`
<!-- cli-example: transcript relabel -->
- 说话人重命名：`into-md transcript relabel document-ir.json --speaker SPEAKER_00=Alice -o meeting.md`
<!-- cli-example: providers -->
- Provider 列表：`into-md providers --json`
<!-- cli-example: providers show -->
- Provider 详情：`into-md providers show team --json`
<!-- cli-example: providers add -->
- 添加 Provider：`into-md providers add team --type openai-compatible --base-url https://api.example/v1 --model model-name --api-key-env TEAM_API_KEY --capability image-description --scope project`
<!-- cli-example: providers remove -->
- 删除 Provider：`into-md providers remove team --scope project`
<!-- cli-example: providers set-default -->
- 默认 Provider：`into-md providers set-default team --scope project`
<!-- cli-example: providers capabilities -->
- Provider 能力：`into-md providers capabilities team --json`
<!-- cli-example: providers test -->
- Provider 探测：`into-md providers test team --allow-network --allow-host api.example`
<!-- cli-example: plugins -->
- 插件列表：`into-md plugins --json`
<!-- cli-example: plugins show -->
- 插件详情：`into-md plugins show example.plugin --json`
<!-- cli-example: plugins install -->
- 安装插件：`into-md plugins install ./example.imp --scope project`
<!-- cli-example: plugins verify -->
- 验证插件：`into-md plugins verify example.plugin --json --scope project`
<!-- cli-example: plugins enable -->
- 启用插件：`into-md plugins enable example.plugin --scope project`
<!-- cli-example: plugins disable -->
- 禁用插件：`into-md plugins disable example.plugin --scope project`
<!-- cli-example: plugins remove -->
- 删除插件：`into-md plugins remove example.plugin --scope project`
<!-- cli-example: plugins run -->
- 运行插件：`into-md plugins run example.plugin source.bin --input-format txt --scope project`
<!-- cli-example: config -->
- 配置入口：`into-md config --help`
<!-- cli-example: config paths -->
- 配置路径：`into-md config paths --json`
<!-- cli-example: config show -->
- 配置内容：`into-md config show --resolved --format json`
<!-- cli-example: config init -->
- 初始化配置：`into-md config init --scope project`
<!-- cli-example: config validate -->
- 验证配置：`into-md config validate into-markdown.toml`
<!-- cli-example: config get -->
- 读取配置：`into-md config get output.conflict`
<!-- cli-example: config set -->
- 设置配置：`into-md config set output.conflict '"error"' --scope project`
<!-- cli-example: config unset -->
- 删除配置：`into-md config unset output.conflict --scope project`
<!-- cli-example: config profile -->
- Profile 入口：`into-md config profile --help`
<!-- cli-example: config profile list -->
- Profile 列表：`into-md config profile list`
<!-- cli-example: config profile create -->
- 创建 Profile：`into-md config profile create review --scope project`
<!-- cli-example: config profile remove -->
- 删除 Profile：`into-md config profile remove review --scope project`
<!-- cli-example: doctor -->
- 诊断：`into-md doctor --json`
<!-- cli-example: completions -->
- 补全：`into-md completions bash`
<!-- cli-example: version -->
- 版本：`into-md version --json`

## 当前可用格式

<!-- format-example: pdf -->
- PDF：`into-md report.pdf --format pdf -o report.md --conflict error`
<!-- format-example: doc -->
- DOC：`into-md legacy.doc --format doc -o legacy.md --conflict error`
<!-- format-example: docx -->
- DOCX：`into-md report.docx --format docx -o report.md --conflict error`
<!-- format-example: ppt -->
- PPT：`into-md legacy.ppt --format ppt -o legacy.md --conflict error`
<!-- format-example: pptx -->
- PPTX：`into-md slides.pptx --format pptx -o slides.md --conflict error`
<!-- format-example: xls -->
- XLS：`into-md legacy.xls --format xls -o legacy.md --conflict error`
<!-- format-example: xlsx -->
- XLSX：`into-md workbook.xlsx --format xlsx -o workbook.md --conflict error`
<!-- format-example: odt -->
- ODT：`into-md document.odt --format odt -o document.md --conflict error`
<!-- format-example: ods -->
- ODS：`into-md sheet.ods --format ods -o sheet.md --conflict error`
<!-- format-example: odp -->
- ODP：`into-md slides.odp --format odp -o slides.md --conflict error`
<!-- format-example: rtf -->
- RTF：`into-md document.rtf --format rtf -o document.md --conflict error`
<!-- format-example: epub -->
- EPUB：`into-md book.epub --format epub -o book.md --conflict error`
<!-- format-example: text -->
- TXT：`into-md notes.txt --format text -o notes.md --conflict error`
<!-- format-example: markdown -->
- Markdown：`into-md notes.md --format markdown -o normalized.md --conflict error`
<!-- format-example: html -->
- HTML：`into-md page.html --format html -o page.md --conflict error`
<!-- format-example: csv -->
- CSV：`into-md table.csv --format csv -o table.md --conflict error`
<!-- format-example: tsv -->
- TSV：`into-md table.tsv --format tsv -o table.md --conflict error`
<!-- format-example: json -->
- JSON：`into-md data.json --format json -o data.md --conflict error`
<!-- format-example: xml -->
- XML：`into-md data.xml --format xml -o data.md --conflict error`
<!-- format-example: drawio -->
- Drawio：`into-md diagram.drawio --format drawio -o diagram.md --conflict error`
<!-- format-example: feed -->
- RSS/Atom：`into-md feed.xml --format feed -o feed.md --conflict error`
<!-- format-example: ipynb -->
- Notebook：`into-md notebook.ipynb --format ipynb -o notebook.md --conflict error`
<!-- format-example: image -->
- 图片：`into-md scan.png --format image --ocr always -o scan.md --conflict error`
<!-- format-example: zip -->
- ZIP：`into-md archive.zip --format zip -o archive.md --conflict error`
<!-- format-example: outlook-msg -->
- MSG：`into-md message.msg --format outlook-msg -o message.md --conflict error`
<!-- format-example: audio -->
- 音频：`into-md meeting.wav --format audio --ai audio-transcription=only -o meeting.md --conflict error`
<!-- format-example: video -->
- 视频：`into-md meeting.webm --format video --ai audio-transcription=only -o meeting.md --conflict error`

参数与失败边界见 [CLI](cli.md)和[格式矩阵](formats.md)。
