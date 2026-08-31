import { createContext, type ReactNode, useContext, useEffect, useMemo, useState } from "react";

export type Locale = "zh-CN" | "en";

const messages = {
  "zh-CN": {
    appName: "into-markdown 控制台",
    skip: "跳到主要内容",
    status: "服务状态",
    language: "语言",
    theme: "主题",
    system: "跟随系统",
    light: "浅色",
    dark: "深色",
    loading: "正在检查本地服务…",
    retry: "重试",
    apiAvailable: "本地 API 可用",
    apiUnavailable: "本地 API 不可用",
    consoleUnavailable: "文档控制台功能尚不可用",
    unavailableDetail: "此页面只提供安全控制台壳。文档、任务与管理功能不在当前范围内。",
    errorTitle: "无法读取服务状态",
    errorDetail: "请确认 into-md ui 仍在运行，然后重试。",
    unexpectedTitle: "页面遇到问题",
    unexpectedDetail: "页面已安全停止。你可以重新加载控制台。",
    reload: "重新加载",
    notFound: "页面不存在",
    backStatus: "返回服务状态",
    workbench: "转换工作台", documentConversion: "文稿转换", speechTranscription: "语音转写", workbenchIntro: "批量上传本地文档，配置一次性转换策略并实时跟踪每项任务。",
    localAdministration: "本地管理", detectionAuthority: "检测条件", resolvedConfig: "显示完全解析的配置", format: "格式", extension: "扩展名", mimeType: "MIME 类型", charset: "字符集", runChecks: "运行检查", path: "路径", configurationManagement: "配置管理", typedKey: "类型化键", promptKeyName: "提示词键名", selectPromptKey: "选择提示词键", validationPath: "验证路径", forceOverwriteConfig: "强制覆盖现有配置", paths: "路径", get: "读取", validate: "验证", initialize: "初始化", allowPrivateNetwork: "仅允许本次请求访问私有网络", allowInsecureTransport: "仅允许本次请求使用不安全传输",
    administration: "系统管理", administrationIntro: "查看转换能力、连接 AI 服务，并处理本机运行问题。", formats: "格式支持", providers: "AI 服务", plugins: "扩展插件", configuration: "设置", doctor: "运行诊断", verify: "验证", install: "安装", installPlugin: "安装插件", packageSource: "包来源", signerId: "签名密钥 ID", signerFingerprint: "签名密钥指纹", scope: "作用域", effective: "当前生效", shadowedBy: "被覆盖于", inherited: "继承自其他作用域", target: "目标平台", packageSha256: "包 SHA-256", timeoutMs: "超时（毫秒）", allowHosts: "允许的主机", detectFormat: "检测本地文件格式", localPath: "本地文件路径", detect: "检测", addProvider: "添加 AI 服务", providerName: "服务名称", baseUrl: "API 地址", model: "模型", environmentName: "密钥环境变量名", setDefault: "设为默认", unset: "取消设置", copyFrom: "复制自设置方案", show: "查看", enable: "启用", disable: "停用", configured: "已配置", missing: "缺失", testProvider: "测试连接", noneConfigured: "当前没有额外插件。", redactedConfiguration: "已隐藏敏感值的配置", profiles: "设置方案", save: "保存", create: "创建",
    convertDocuments: "转换文档", convertDocumentsIntro: "添加文件，自动识别内容，快速得到结构清晰的 Markdown。",
    sourceFiles: "来源文件", conversionSettings: "转换设置", outputFormat: "输出格式", recognitionMode: "图片文字识别",
    smart: "自动", precise: "强制识别", forceRecognition: "始终识别扫描内容", recommended: "推荐", separateAssets: "保存到同名文件夹", embedAssets: "直接写入 Markdown", omitAssets: "不保存附件", advancedSettings: "高级设置", openAdvancedSettings: "打开高级设置",
    ocrAutomaticHelp: "只在检测到扫描页或图片文字时启用 OCR。", ocrAlwaysHelp: "每一页都执行 OCR，适合纯扫描文件，处理时间会更长。", ocrOffHelp: "不识别扫描页和图片中的文字。",
    assetExtractHelp: "图片与附件保存到 Markdown 旁边的同名文件夹。", assetEmbedHelp: "把小型资源写入 Markdown，文件体积会明显增大。", assetOmitHelp: "只保留正文，不输出图片和附件。", imagesAndAttachments: "图片与附件",
    documentParsing: "文档解析", imageOcr: "图片 OCR", audioTranscription: "音频转写", localReady: "本地就绪", automaticDetection: "自动检测", enabled: "已启用", disabled: "未启用", enableInWorkbench: "可在工作台启用", audioReady: "能力就绪", audioNeedsSetup: "需要选择来源", sourceNeeded: "尚未选择来源", prepareDependencies: "准备能力", capabilities: "转换能力",
    systemReady: "系统就绪", systemNeedsAttention: "需要检查", checkingSystem: "正在检查", moreActions: "更多操作", latestResult: "最新转换结果", loadingPreview: "正在加载预览…",
    batchLimitSummary: "最多 100 个文件", resultsAndHistory: "结果与历史", manageHistory: "管理历史",
    capabilityCenter: "转换能力", capabilityCenterIntro: "查看文档转换服务与当前能力来源。", allLocalServicesReady: "所选转换能力已就绪，可以返回工作台继续。", checkingSystemDetail: "正在确认能力来源与任务存储。", audioEnvironment: "语音能力", audioEnvironmentReady: "当前转写与说话人识别来源已通过验证。", audioEnvironmentSetup: "会议逐字稿需要可用的转写来源；说话人识别可继续使用本地语音插件。准备完成后关闭此窗口，页面会重新检查。",
    prepareAudioTitle: "选择转写来源", installWhisperModel: "安装本地语音", prepareFfmpegRuntime: "本地运行", ffmpegRuntimeNote: "", copyCommand: "复制命令", done: "完成", installNow: "安装", installLocalOcr: "安装本地 OCR", installLocalSpeech: "安装本地语音", chooseAiService: "选择 AI 服务", runsOnThisDevice: "在这台设备上处理", usesNetwork: "通过已连接的在线服务处理", speechSourceUnavailable: "当前转写来源不可用，请重新选择。", installingComponents: "正在安装…", installComponentsFailed: "安装失败，请重试。",
    backWorkbench: "返回转换工作台", addDocuments: "添加文档", dropFiles: "将文件拖放到这里",
    chooseFiles: "选择文件", chooseFolder: "选择目录", selectedFiles: "已选择", detectedFormat: "格式", remove: "移除", options: "批量转换选项",
    formatHint: "格式提示", automatic: "自动", ocr: "本地 OCR", ocrConfidence: "OCR 最低置信度", always: "始终", off: "关闭",
    aiMode: "处理来源", localOnly: "只用本地", aiWhenLocalFails: "本地失败时使用 AI", preferAi: "优先使用 AI", aiOnly: "只用 AI", assetMode: "图片与附件", maxInput: "单文件上限 (MiB)", maxMemory: "内存上限 (MiB)", maxPages: "页数上限",
    networkAccess: "允许网络访问", networkDisabledNote: "关闭时仅处理本地内容。", networkEnabledNote: "开启后可访问互联网和局域网服务。",
    authorizeProvider: "我授权这些上传使用已配置的 AI 服务", authorizeRemoteConversion: "允许本次转换使用所选 AI 服务", authorizeMeetingProvider: "允许本次音频使用所选 AI 服务并联网转写", authorizationNote: "仅适用于本次上传，不会保存。", authorizationRequired: "请先同意本次使用所选 AI 服务。", remoteNetworkRequired: "所选图片识别来源需要联网。请先同意本次使用。", providerUnavailableFailure: "所选 AI 服务尚未就绪，请在能力与来源中检查连接和密钥设置", networkPolicyFailure: "当前网络安全设置阻止了所选 AI 服务，请检查允许访问的主机和局域网设置",
    convert: "开始转换", uploading: "正在上传…", tasks: "任务", refresh: "刷新", noTasks: "还没有任务",
    restoredTask: "已恢复任务", pending: "排队中", running: "运行中", converted: "正在发布", succeeded: "已完成", failed: "失败", interrupted: "已中断", cancelled: "已取消",
    cancel: "取消", downloadBundle: "下载 ZIP", downloadMarkdown: "下载 Markdown", streamError: "实时进度连接已中断，可刷新恢复。", loadTasksError: "无法恢复任务列表。",
    preview: "预览", download: "下载", resources: "资源", closePreview: "关闭预览", previewFailed: "无法读取预览。", previewTruncated: "大文件预览已截断；下载可查看完整内容。",
    tooManyFiles: "每批最多 100 个文件。", fileTooLarge: "文件超过当前单文件上限。", batchTooLarge: "每批文件总量不得超过 1 GiB。", uploadFailed: "上传失败", retryNeedsFile: "刷新后重试需要重新选择原文件。", unsupportedFiles: "已跳过不支持的文件：{files}",
    diagnosticsPreviewUnavailable: "诊断预览不可用，请在详情与资源中下载完整诊断。", archiveExtractionRequired: "RAR 归档请先解压后再转换", unsupportedFormatFailure: "未识别出受支持的文件格式，请检查文件类型", malformedInputFailure: "文件内容损坏或格式不正确", encryptedInputFailure: "文件已加密或受密码保护", resourceLimitFailure: "文件超出当前资源限制", ocrFailure: "文字识别失败", aiFailure: "AI 处理失败", networkFailure: "网络访问失败", ioFailure: "无法读取或写入文件", componentUnavailableFailure: "所需本地依赖尚未准备", timeoutFailure: "转换超时", recoveryFailure: "无法恢复此前的转换任务", internalFailure: "转换服务遇到内部错误", conversionFailedReason: "文件内容无法转换，请检查文件是否完整", invalidOptionsFailure: "转换设置无效", unreachableFailure: "无法连接本地转换服务", failureDetails: "失败详情",
    pin: "固定", unpin: "取消固定", pinned: "已固定", pinnedOnly: "仅固定任务", filterStatus: "状态筛选", allStatuses: "全部状态", loadMore: "加载更多", deleteTask: "永久删除", deleteWarning: "此操作不可恢复，将永久删除任务记录、输入与产物。是否继续？",
    taskDetails: "任务详情", created: "创建时间", updated: "更新时间", on: "开启",
    cleanup: "立即清理", cleanupWarning: "将按 30 天和 10 GiB 保留策略永久删除符合条件的未固定已完成任务，且不可恢复。是否继续？", cleanupResult: "清理完成：删除 {tasks} 个任务，释放 {bytes} MiB。",
    primaryNavigation: "主要导航", history: "历史记录", recentHistory: "最近记录", viewAllHistory: "查看全部", conversionResult: "转换结果", currentBatch: "当前批次", close: "关闭",
    previewMode: "预览模式", renderedPreview: "阅读预览", markdownSource: "Markdown 源码", detailsAndResources: "详情与资源", batchResults: "批次结果", moreBatchResults: "更多批次结果", noMarkdownResult: "此任务没有 Markdown 结果",
    noResources: "没有提取资源", diagnostics: "诊断信息", noDiagnostics: "没有诊断信息", otherArtifacts: "其他产物", file: "文件", size: "产物", artifacts: "项产物", batchOf: "同批 {count} 个文件",
    meetingNotes: "语音转写", meetingIntro: "录制或导入音频，生成带时间与说话人标记的忠实逐字稿。", liveMeeting: "录制音频", recordMeeting: "录制与导入",
    saveRecording: "保存录音", historySearch: "搜索记录", previousPage: "上一页", nextPage: "下一页", page: "页", noHistoryMatches: "没有符合条件的记录。",
    readyToRecord: "已准备录制", microphoneReady: "麦克风已待命", computerAudioReady: "电脑声音已待命", mixedAudioReady: "麦克风与电脑声音已待命", connectingAudioSource: "正在连接录制来源", recordingNow: "正在录制", recordingPaused: "录制已暂停", savingRecording: "正在保存录音", recordingReady: "录音已就绪",
    startRecording: "开始录制", pauseRecording: "暂停", resumeRecording: "继续", endRecording: "结束录制", discardRecording: "丢弃录音",
    microphone: "麦克风", systemDefaultMicrophone: "系统默认麦克风", microphonePermissionDenied: "没有获得麦克风权限，请在浏览器地址栏允许后重试。", microphonePermissionTimedOut: "麦克风授权没有响应，请检查浏览器地址栏的权限提示后重试。", microphoneUnavailable: "无法连接所选麦克风，请检查设备后重试。", recordingUnsupported: "当前浏览器不支持现场录制。",
    recordingSource: "录制来源", microphoneOnly: "仅麦克风", computerAudioOnly: "仅电脑声音", microphoneAndComputerAudio: "麦克风 + 电脑声音", computerAudioCaptureHelp: "开始后请选择要共享的标签页、窗口或屏幕，并开启共享音频；只保存声音，不保存画面。", systemAudioPermissionDenied: "没有获得电脑声音共享权限，请重新选择共享来源。", systemAudioPermissionTimedOut: "电脑声音共享没有响应，请完成或取消浏览器中的共享选择。", systemAudioMissing: "所选共享来源没有音频，请选择支持音频的来源并开启共享音频。", systemAudioUnavailable: "当前浏览器或所选来源无法录制电脑声音。",
    recordingRecovered: "已恢复上次中断前保存的录音，可直接生成逐字稿。", recordingStorageUnavailable: "浏览器本地录音存储不可用；当前录音无法跨刷新恢复。", recordingSaveFailed: "录音保存失败，请检查浏览器存储空间。",
    orImportRecording: "或者", importRecording: "导入已有录音", unsupportedRecording: "请选择 MP3、M4A、WAV、FLAC、OGG 或常见视频录音。", localDraft: "本地恢复录音",
    transcript: "逐字稿", transcriptSettings: "逐字稿设置", transcriptLanguage: "转写语言", simplifiedChinese: "简体中文", traditionalChinese: "繁体中文", english: "英语", transcriptLanguageHelp: "中文界面默认输出简体；也可保留自动检测或指定繁体。", distinguishSpeakers: "区分说话人", anonymousSpeakerLabels: "使用 Speaker 1、Speaker 2 等匿名标签", speakerSetupNeeded: "说话人组件尚未准备", expectedSpeakers: "预计人数", prepareAudioComponents: "准备音频组件",
    audioNeedsSetupNearby: "请选择可用的转写来源。", meetingTranscript: "语音逐字稿", viewTranscript: "查看逐字稿", transcribing: "正在生成逐字稿", generateTranscript: "生成逐字稿", regenerateTranscript: "重新转写", cancelTranscription: "取消转写", cancellingTranscription: "正在取消", chooseRecordingBeforeTranscript: "请先录制或导入一段音频", recentMeetings: "最近记录", noMeetingHistory: "还没有转写记录。",
    preparingAudio: "准备音频", recognizingContent: "识别内容", distinguishingSpeakers: "区分说话人", generatingTranscript: "生成逐字稿",
    speakerNames: "说话人名称", speakerNamesHint: "只更改显示名称，不会重新识别音频。", saveSpeakerNames: "保存名称", speakerLabelsLoadFailed: "无法读取说话人列表。", invalidSpeakerName: "名称需为 1–80 个字符，且不能包含控制字符。", speakerNamesSaved: "说话人名称已更新，逐字稿与下载产物已重新生成。", speakerNamesSaveFailed: "名称保存失败，产物可能已被其他页面更新，请刷新后重试。",
  },
  en: {
    appName: "into-markdown console",
    skip: "Skip to main content",
    status: "Service status",
    language: "Language",
    theme: "Theme",
    system: "System",
    light: "Light",
    dark: "Dark",
    loading: "Checking the local service…",
    retry: "Retry",
    apiAvailable: "Local API available",
    apiUnavailable: "Local API unavailable",
    consoleUnavailable: "Document console features are unavailable",
    unavailableDetail: "This page provides the secure console shell only. Documents, jobs, and administration are outside its scope.",
    errorTitle: "Could not read service status",
    errorDetail: "Make sure into-md ui is still running, then retry.",
    unexpectedTitle: "The page encountered a problem",
    unexpectedDetail: "The page stopped safely. You can reload the console.",
    reload: "Reload",
    notFound: "Page not found",
    backStatus: "Back to service status",
    workbench: "Conversion workbench", documentConversion: "Document conversion", speechTranscription: "Speech transcription", workbenchIntro: "Upload local documents in batches, set one-time conversion policy, and follow every task live.",
    localAdministration: "Local administration", detectionAuthority: "Detection authority", resolvedConfig: "Show fully resolved configuration", format: "Format", extension: "Extension", mimeType: "MIME type", charset: "Character set", runChecks: "Run checks", path: "Path", configurationManagement: "Configuration management", typedKey: "Typed key", promptKeyName: "Prompt key name", selectPromptKey: "Select prompt key", validationPath: "Validation path", forceOverwriteConfig: "Force overwrite existing configuration", paths: "Paths", get: "Get", validate: "Validate", initialize: "Initialize", allowPrivateNetwork: "Allow private-network access for this request", allowInsecureTransport: "Allow insecure transport for this request",
    administration: "System management", administrationIntro: "Check conversion capabilities, connect AI services, and resolve local runtime issues.", formats: "Format support", providers: "AI services", plugins: "Extensions", configuration: "Settings", doctor: "Diagnostics", verify: "Verify", install: "Install", installPlugin: "Install plugin", packageSource: "Package source", signerId: "Signing key ID", signerFingerprint: "Signing key fingerprint", scope: "Scope", effective: "Effective", shadowedBy: "Shadowed by", inherited: "Inherited from another scope", target: "Target", packageSha256: "Package SHA-256", timeoutMs: "Timeout (ms)", allowHosts: "Allowed hosts", detectFormat: "Detect a local file format", localPath: "Local file path", detect: "Detect", addProvider: "Add AI service", providerName: "Service name", baseUrl: "API address", model: "Model", environmentName: "API key environment variable", setDefault: "Set default", unset: "Unset", copyFrom: "Copy from profile", show: "Show", enable: "Enable", disable: "Disable", configured: "Configured", missing: "Missing", testProvider: "Test connection", noneConfigured: "No extra plugins are configured.", redactedConfiguration: "Configuration with secrets hidden", profiles: "Profiles", save: "Save", create: "Create",
    convertDocuments: "Convert documents", convertDocumentsIntro: "Add files, recognize content automatically, and get clean, structured Markdown.",
    sourceFiles: "Source files", conversionSettings: "Conversion settings", outputFormat: "Output format", recognitionMode: "Text in images",
    smart: "Automatic", precise: "Force recognition", forceRecognition: "Always scan every page", recommended: "Recommended", separateAssets: "Save beside Markdown", embedAssets: "Embed in Markdown", omitAssets: "Do not save attachments", advancedSettings: "Advanced settings", openAdvancedSettings: "Open advanced settings",
    ocrAutomaticHelp: "Run OCR only when scanned pages or text in images are detected.", ocrAlwaysHelp: "Run OCR on every page. Best for scan-only files, but slower.", ocrOffHelp: "Do not read text from scanned pages or images.",
    assetExtractHelp: "Save images and attachments in a matching folder beside the Markdown file.", assetEmbedHelp: "Write small assets into Markdown. The Markdown file can become much larger.", assetOmitHelp: "Keep text only and omit images and attachments.", imagesAndAttachments: "Images and attachments",
    documentParsing: "Document parsing", imageOcr: "Image OCR", audioTranscription: "Audio transcription", localReady: "Ready locally", automaticDetection: "Auto detect", enabled: "Enabled", disabled: "Disabled", enableInWorkbench: "Enable in workbench", audioReady: "Capability ready", audioNeedsSetup: "Choose a source", sourceNeeded: "No source selected", prepareDependencies: "Prepare", capabilities: "Conversion capabilities",
    systemReady: "System ready", systemNeedsAttention: "Needs attention", checkingSystem: "Checking", moreActions: "More actions", latestResult: "Latest result", loadingPreview: "Loading preview…",
    batchLimitSummary: "Up to 100 files", resultsAndHistory: "Results and history", manageHistory: "Manage history",
    capabilityCenter: "Conversion capabilities", capabilityCenterIntro: "Check document conversion services and their current sources.", allLocalServicesReady: "The selected capabilities are ready. Return to the workbench to continue.", checkingSystemDetail: "Checking capability sources and the task store.", audioEnvironment: "Speech capability", audioEnvironmentReady: "The selected transcription and speaker-identification sources passed verification.", audioEnvironmentSetup: "Meeting transcripts need a ready transcription source; speaker identification can continue using the local Speech plugin. Close this dialog after setup and the page will check again.",
    prepareAudioTitle: "Choose a transcription source", installWhisperModel: "Install local Speech", prepareFfmpegRuntime: "Runs locally", ffmpegRuntimeNote: "", copyCommand: "Copy command", done: "Done", installNow: "Install", installLocalOcr: "Install local OCR", installLocalSpeech: "Install local Speech", chooseAiService: "Choose AI service", runsOnThisDevice: "Processed on this device", usesNetwork: "Processed by a connected online service", speechSourceUnavailable: "The current transcription source is unavailable. Choose another source.", installingComponents: "Installing…", installComponentsFailed: "Installation failed. Try again.",
    backWorkbench: "Back to workbench", addDocuments: "Add documents", dropFiles: "Drop files here",
    chooseFiles: "Choose files", chooseFolder: "Choose folder", selectedFiles: "Selected", detectedFormat: "Format", remove: "Remove", options: "Batch conversion options",
    formatHint: "Format hint", automatic: "Automatic", ocr: "Local OCR", ocrConfidence: "Minimum OCR confidence", always: "Always", off: "Off",
    aiMode: "Processing source", localOnly: "Local only", aiWhenLocalFails: "Use AI if local fails", preferAi: "Prefer AI", aiOnly: "AI only", assetMode: "Images and attachments", maxInput: "File limit (MiB)", maxMemory: "Memory limit (MiB)", maxPages: "Page limit",
    networkAccess: "Allow network access", networkDisabledNote: "When off, conversions process local content only.", networkEnabledNote: "When on, conversions may access internet and local-network services.",
    authorizeProvider: "I authorize these uploads to use the configured AI service", authorizeRemoteConversion: "Allow this conversion to use the selected AI service", authorizeMeetingProvider: "Allow this audio to use the selected AI service and network transcription", authorizationNote: "Applies only to this upload and is not saved.", authorizationRequired: "Confirm use of the selected AI service for this upload.", remoteNetworkRequired: "The selected image-recognition source needs network access. Confirm it for this upload.", providerUnavailableFailure: "The selected AI service is not ready. Check its connection and key settings in Capabilities & sources.", networkPolicyFailure: "The current network policy blocks the selected AI service. Check its allowed hosts and local-network setting.",
    convert: "Start conversion", uploading: "Uploading…", tasks: "Tasks", refresh: "Refresh", noTasks: "No tasks yet",
    restoredTask: "Restored task", pending: "Queued", running: "Running", converted: "Publishing", succeeded: "Completed", failed: "Failed", interrupted: "Interrupted", cancelled: "Cancelled",
    cancel: "Cancel", downloadBundle: "Download ZIP", downloadMarkdown: "Download Markdown", streamError: "Live progress disconnected; refresh can recover it.", loadTasksError: "Could not restore the task list.",
    preview: "Preview", download: "Download", resources: "Resources", closePreview: "Close preview", previewFailed: "Could not load the preview.", previewTruncated: "Large preview truncated; download the artifact for complete content.",
    tooManyFiles: "A batch can contain at most 100 files.", fileTooLarge: "A file exceeds the selected per-file limit.", batchTooLarge: "A batch cannot exceed 1 GiB.", uploadFailed: "Upload failed", retryNeedsFile: "After refresh, select the original file again to retry.", unsupportedFiles: "Skipped unsupported files: {files}",
    diagnosticsPreviewUnavailable: "Diagnostics preview unavailable. Download the complete diagnostics from Details and resources.", archiveExtractionRequired: "Extract the RAR archive before converting", unsupportedFormatFailure: "No supported format was identified; check the file type", malformedInputFailure: "The file is damaged or its format is invalid", encryptedInputFailure: "The file is encrypted or password protected", resourceLimitFailure: "The file exceeds the current resource limits", ocrFailure: "Text recognition failed", aiFailure: "AI processing failed", networkFailure: "Network access failed", ioFailure: "The file could not be read or written", componentUnavailableFailure: "A required local dependency is not ready", timeoutFailure: "Conversion timed out", recoveryFailure: "The previous conversion task could not be restored", internalFailure: "The conversion service encountered an internal error", conversionFailedReason: "The file content could not be converted; check that the file is complete", invalidOptionsFailure: "The conversion settings are invalid", unreachableFailure: "The local conversion service is unavailable", failureDetails: "Failure details",
    pin: "Pin", unpin: "Unpin", pinned: "Pinned", pinnedOnly: "Pinned only", filterStatus: "Status filter", allStatuses: "All statuses", loadMore: "Load more", deleteTask: "Delete permanently", deleteWarning: "This cannot be undone. The task record, input, and artifacts will be permanently deleted. Continue?",
    taskDetails: "Task details", created: "Created", updated: "Updated", on: "On",
    cleanup: "Clean up now", cleanupWarning: "This permanently deletes eligible unpinned completed tasks under the 30-day and 10 GiB retention policy and cannot be undone. Continue?", cleanupResult: "Cleanup complete: removed {tasks} tasks and reclaimed {bytes} MiB.",
    primaryNavigation: "Primary navigation", history: "History", recentHistory: "Recent", viewAllHistory: "View all", conversionResult: "Conversion result", currentBatch: "Current batch", close: "Close",
    previewMode: "Preview mode", renderedPreview: "Reading view", markdownSource: "Markdown source", detailsAndResources: "Details and resources", batchResults: "Batch results", moreBatchResults: "More batch results", noMarkdownResult: "This task has no Markdown result",
    noResources: "No extracted resources", diagnostics: "Diagnostics", noDiagnostics: "No diagnostics", otherArtifacts: "Other artifacts", file: "File", size: "Artifacts", artifacts: "artifacts", batchOf: "{count} files in batch",
    meetingNotes: "Speech transcription", meetingIntro: "Record or import audio and create a faithful transcript with timestamps and speaker labels.", liveMeeting: "Record audio", recordMeeting: "Record or import",
    saveRecording: "Save recording", historySearch: "Search history", previousPage: "Previous", nextPage: "Next", page: "Page", noHistoryMatches: "No matching history.",
    readyToRecord: "Ready to record", microphoneReady: "Microphone ready", computerAudioReady: "Computer audio ready", mixedAudioReady: "Microphone and computer audio ready", connectingAudioSource: "Connecting recording sources", recordingNow: "Recording", recordingPaused: "Recording paused", savingRecording: "Saving recording", recordingReady: "Recording ready",
    startRecording: "Start recording", pauseRecording: "Pause", resumeRecording: "Resume", endRecording: "End recording", discardRecording: "Discard recording",
    microphone: "Microphone", systemDefaultMicrophone: "System default microphone", microphonePermissionDenied: "Microphone permission was denied. Allow it in the browser address bar and retry.", microphonePermissionTimedOut: "The microphone permission request did not respond. Check the browser address bar and retry.", microphoneUnavailable: "The selected microphone is unavailable. Check the device and retry.", recordingUnsupported: "This browser cannot record meetings.",
    recordingSource: "Recording source", microphoneOnly: "Microphone only", computerAudioOnly: "Computer audio only", microphoneAndComputerAudio: "Microphone + computer audio", computerAudioCaptureHelp: "Choose a tab, window, or screen after starting and enable shared audio. Video is never saved.", systemAudioPermissionDenied: "Computer audio sharing was not allowed. Choose a share source again.", systemAudioPermissionTimedOut: "Computer audio sharing did not respond. Complete or cancel the browser share picker.", systemAudioMissing: "The selected share has no audio. Choose an audio-capable source and enable shared audio.", systemAudioUnavailable: "This browser or selected source cannot capture computer audio.",
    recordingRecovered: "Recovered the recording saved before the interruption. It is ready to transcribe.", recordingStorageUnavailable: "Local browser recording storage is unavailable; this recording cannot survive a refresh.", recordingSaveFailed: "The recording could not be saved. Check browser storage space.",
    orImportRecording: "or", importRecording: "Import recording", unsupportedRecording: "Choose an MP3, M4A, WAV, FLAC, OGG, or common video recording.", localDraft: "recovered local recording",
    transcript: "Transcript", transcriptSettings: "Transcript settings", transcriptLanguage: "Transcript language", simplifiedChinese: "Simplified Chinese", traditionalChinese: "Traditional Chinese", english: "English", transcriptLanguageHelp: "Sets the recognition hint and deterministic Chinese script output.", distinguishSpeakers: "Distinguish speakers", anonymousSpeakerLabels: "Uses anonymous labels such as Speaker 1 and Speaker 2", speakerSetupNeeded: "Speaker components need setup", expectedSpeakers: "Expected speakers", prepareAudioComponents: "Prepare audio components",
    audioNeedsSetupNearby: "Choose an available transcription source.", meetingTranscript: "Transcript", viewTranscript: "View transcript", transcribing: "Creating transcript", generateTranscript: "Create transcript", regenerateTranscript: "Transcribe again", cancelTranscription: "Cancel transcription", cancellingTranscription: "Cancelling", chooseRecordingBeforeTranscript: "Record or import audio first", recentMeetings: "Recent", noMeetingHistory: "No transcripts yet.",
    preparingAudio: "Preparing audio", recognizingContent: "Recognizing content", distinguishingSpeakers: "Distinguishing speakers", generatingTranscript: "Generating transcript",
    speakerNames: "Speaker names", speakerNamesHint: "Changes display names only and does not transcribe audio again.", saveSpeakerNames: "Save names", speakerLabelsLoadFailed: "Could not load the speaker list.", invalidSpeakerName: "Use 1–80 characters without control characters.", speakerNamesSaved: "Speaker names and downloadable artifacts were updated.", speakerNamesSaveFailed: "Could not save names. Another page may have updated the artifacts; refresh and retry.",
  },
} as const;

export type MessageKey = keyof (typeof messages)["en"];
interface I18nValue {
  locale: Locale;
  setLocale(locale: Locale): void;
  t(key: MessageKey): string;
}

const I18nContext = createContext<I18nValue | null>(null);

function initialLocale(): Locale {
  return navigator.languages.some((locale) => locale.toLowerCase().startsWith("zh")) ? "zh-CN" : "en";
}

export function I18nProvider({ children }: { children: ReactNode }) {
  const [locale, setLocale] = useState<Locale>(initialLocale);
  useEffect(() => {
    document.documentElement.lang = locale;
    document.documentElement.dir = "ltr";
  }, [locale]);
  const value = useMemo<I18nValue>(
    () => ({ locale, setLocale, t: (key) => messages[locale][key] }),
    [locale],
  );
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18nValue {
  const value = useContext(I18nContext);
  if (!value) throw new Error("i18n provider is unavailable");
  return value;
}
