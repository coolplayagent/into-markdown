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
    workbench: "转换工作台", workbenchIntro: "批量上传本地文档，配置一次性转换策略并实时跟踪每项任务。",
    convertDocuments: "转换文档", convertDocumentsIntro: "添加文件，自动识别内容，快速得到结构清晰的 Markdown。",
    sourceFiles: "来源文件", conversionSettings: "转换设置", outputFormat: "输出格式", recognitionMode: "识别模式",
    smart: "自动", precise: "强制识别", forceRecognition: "强制识别", recommended: "推荐", separateAssets: "单独保存", embedAssets: "嵌入文档", omitAssets: "忽略资源", advancedSettings: "高级设置",
    documentParsing: "文档解析", imageOcr: "图片 OCR", audioTranscription: "音频转写", localReady: "本地就绪", automaticDetection: "自动检测", enabled: "已启用", disabled: "未启用", enableInWorkbench: "可在工作台启用", audioReady: "环境就绪", audioNeedsSetup: "需要准备", prepareDependencies: "准备依赖", capabilities: "本地能力",
    systemReady: "系统就绪", systemNeedsAttention: "需要检查", checkingSystem: "正在检查", moreActions: "更多操作", latestResult: "最新转换结果", loadingPreview: "正在加载预览…",
    batchLimitSummary: "最多 100 个文件", resultsAndHistory: "结果与历史", manageHistory: "管理历史",
    capabilityCenter: "本地能力", capabilityCenterIntro: "查看文档转换服务是否可以正常工作。", allLocalServicesReady: "本地转换服务已就绪，可以返回工作台继续。", checkingSystemDetail: "正在确认本地转换服务与任务存储。", audioEnvironment: "音频转写环境", audioEnvironmentReady: "Whisper 模型与固定 LGPL FFmpeg 运行时已经通过本机验证。", audioEnvironmentSetup: "音频转写需要 Whisper 模型和与当前版本匹配的固定 LGPL FFmpeg 运行时；准备完成后重新启动工作台。",
    prepareAudioTitle: "准备音频转写", installWhisperModel: "安装 Whisper 模型", prepareFfmpegRuntime: "准备 FFmpeg 运行时", ffmpegRuntimeNote: "完整安装包已随附受审运行时；精简包需要安装与当前版本匹配的 LGPL 运行时。", copyCommand: "复制命令", done: "完成",
    backWorkbench: "返回转换工作台", addDocuments: "添加文档", dropFiles: "将文件拖放到这里",
    chooseFiles: "选择文件", chooseFolder: "选择目录", selectedFiles: "已选择", detectedFormat: "格式", remove: "移除", options: "批量转换选项",
    formatHint: "格式提示", automatic: "自动", ocr: "本地 OCR", ocrConfidence: "OCR 最低置信度", always: "始终", off: "关闭",
    aiMode: "AI 辅助模式", assetMode: "资源输出", maxInput: "单文件上限 (MiB)", maxMemory: "内存上限 (MiB)", maxPages: "页数上限",
    networkAccess: "允许网络访问", networkDisabledNote: "关闭时仅处理本地内容。", networkEnabledNote: "开启后可访问互联网和局域网服务。",
    authorizeProvider: "我授权这些上传使用已配置的 Provider", authorizationNote: "Provider 授权仅适用于本次上传，不会保存。", authorizationRequired: "请明确勾选本次所需的 Provider 授权。",
    convert: "开始转换", uploading: "正在上传…", tasks: "任务", refresh: "刷新", noTasks: "还没有任务",
    restoredTask: "已恢复任务", pending: "排队中", running: "运行中", converted: "正在发布", succeeded: "已完成", failed: "失败", interrupted: "已中断", cancelled: "已取消",
    cancel: "取消", downloadBundle: "下载 ZIP", downloadMarkdown: "下载 Markdown", streamError: "实时进度连接已中断，可刷新恢复。", loadTasksError: "无法恢复任务列表。",
    preview: "预览", download: "下载", resources: "资源", closePreview: "关闭预览", previewFailed: "无法读取预览。", previewTruncated: "大文件预览已截断；下载可查看完整内容。",
    tooManyFiles: "每批最多 100 个文件。", fileTooLarge: "文件超过当前单文件上限。", batchTooLarge: "每批文件总量不得超过 1 GiB。", uploadFailed: "上传失败", retryNeedsFile: "刷新后重试需要重新选择原文件。",
    pin: "固定", unpin: "取消固定", pinned: "已固定", pinnedOnly: "仅固定任务", filterStatus: "状态筛选", allStatuses: "全部状态", loadMore: "加载更多", deleteTask: "永久删除", deleteWarning: "此操作不可恢复，将永久删除任务记录、输入与产物。是否继续？",
    taskDetails: "任务详情", created: "创建时间", updated: "更新时间", on: "开启",
    cleanup: "立即清理", cleanupWarning: "将按 30 天和 10 GiB 保留策略永久删除符合条件的未固定已完成任务，且不可恢复。是否继续？", cleanupResult: "清理完成：删除 {tasks} 个任务，释放 {bytes} MiB。",
    primaryNavigation: "主要导航", history: "历史记录", recentHistory: "最近记录", viewAllHistory: "查看全部", conversionResult: "转换结果", currentBatch: "当前批次", close: "关闭",
    previewMode: "预览模式", renderedPreview: "阅读预览", markdownSource: "Markdown 源码", detailsAndResources: "详情与资源", batchResults: "批次结果", moreBatchResults: "更多批次结果", noMarkdownResult: "此任务没有 Markdown 结果",
    noResources: "没有提取资源", diagnostics: "诊断信息", noDiagnostics: "没有诊断信息", otherArtifacts: "其他产物", file: "文件", size: "产物", artifacts: "项产物", batchOf: "同批 {count} 个文件",
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
    workbench: "Conversion workbench", workbenchIntro: "Upload local documents in batches, set one-time conversion policy, and follow every task live.",
    convertDocuments: "Convert documents", convertDocumentsIntro: "Add files, recognize content automatically, and get clean, structured Markdown.",
    sourceFiles: "Source files", conversionSettings: "Conversion settings", outputFormat: "Output format", recognitionMode: "Recognition mode",
    smart: "Automatic", precise: "Force recognition", forceRecognition: "Force recognition", recommended: "Recommended", separateAssets: "Save separately", embedAssets: "Embed", omitAssets: "Omit", advancedSettings: "Advanced settings",
    documentParsing: "Document parsing", imageOcr: "Image OCR", audioTranscription: "Audio transcription", localReady: "Ready locally", automaticDetection: "Auto detect", enabled: "Enabled", disabled: "Disabled", enableInWorkbench: "Enable in workbench", audioReady: "Ready", audioNeedsSetup: "Setup needed", prepareDependencies: "Prepare", capabilities: "Local capabilities",
    systemReady: "System ready", systemNeedsAttention: "Needs attention", checkingSystem: "Checking", moreActions: "More actions", latestResult: "Latest result", loadingPreview: "Loading preview…",
    batchLimitSummary: "Up to 100 files", resultsAndHistory: "Results and history", manageHistory: "Manage history",
    capabilityCenter: "Local capabilities", capabilityCenterIntro: "Check whether the document conversion service is working normally.", allLocalServicesReady: "The local conversion service is ready. Return to the workbench to continue.", checkingSystemDetail: "Checking the local conversion service and task store.", audioEnvironment: "Audio transcription environment", audioEnvironmentReady: "The Whisper model and pinned LGPL FFmpeg runtime passed local verification.", audioEnvironmentSetup: "Audio transcription needs the Whisper model and pinned LGPL FFmpeg runtime matching this build. Restart the workbench after setup.",
    prepareAudioTitle: "Prepare audio transcription", installWhisperModel: "Install the Whisper model", prepareFfmpegRuntime: "Prepare the FFmpeg runtime", ffmpegRuntimeNote: "The full package includes the reviewed runtime; the core package needs the matching LGPL runtime.", copyCommand: "Copy command", done: "Done",
    backWorkbench: "Back to workbench", addDocuments: "Add documents", dropFiles: "Drop files here",
    chooseFiles: "Choose files", chooseFolder: "Choose folder", selectedFiles: "Selected", detectedFormat: "Format", remove: "Remove", options: "Batch conversion options",
    formatHint: "Format hint", automatic: "Automatic", ocr: "Local OCR", ocrConfidence: "Minimum OCR confidence", always: "Always", off: "Off",
    aiMode: "AI assistance", assetMode: "Asset output", maxInput: "File limit (MiB)", maxMemory: "Memory limit (MiB)", maxPages: "Page limit",
    networkAccess: "Allow network access", networkDisabledNote: "When off, conversions process local content only.", networkEnabledNote: "When on, conversions may access internet and local-network services.",
    authorizeProvider: "I authorize these uploads to use the configured provider", authorizationNote: "Provider authorization applies only to this upload and is not saved.", authorizationRequired: "Explicitly confirm the required provider authorization.",
    convert: "Start conversion", uploading: "Uploading…", tasks: "Tasks", refresh: "Refresh", noTasks: "No tasks yet",
    restoredTask: "Restored task", pending: "Queued", running: "Running", converted: "Publishing", succeeded: "Completed", failed: "Failed", interrupted: "Interrupted", cancelled: "Cancelled",
    cancel: "Cancel", downloadBundle: "Download ZIP", downloadMarkdown: "Download Markdown", streamError: "Live progress disconnected; refresh can recover it.", loadTasksError: "Could not restore the task list.",
    preview: "Preview", download: "Download", resources: "Resources", closePreview: "Close preview", previewFailed: "Could not load the preview.", previewTruncated: "Large preview truncated; download the artifact for complete content.",
    tooManyFiles: "A batch can contain at most 100 files.", fileTooLarge: "A file exceeds the selected per-file limit.", batchTooLarge: "A batch cannot exceed 1 GiB.", uploadFailed: "Upload failed", retryNeedsFile: "After refresh, select the original file again to retry.",
    pin: "Pin", unpin: "Unpin", pinned: "Pinned", pinnedOnly: "Pinned only", filterStatus: "Status filter", allStatuses: "All statuses", loadMore: "Load more", deleteTask: "Delete permanently", deleteWarning: "This cannot be undone. The task record, input, and artifacts will be permanently deleted. Continue?",
    taskDetails: "Task details", created: "Created", updated: "Updated", on: "On",
    cleanup: "Clean up now", cleanupWarning: "This permanently deletes eligible unpinned completed tasks under the 30-day and 10 GiB retention policy and cannot be undone. Continue?", cleanupResult: "Cleanup complete: removed {tasks} tasks and reclaimed {bytes} MiB.",
    primaryNavigation: "Primary navigation", history: "History", recentHistory: "Recent", viewAllHistory: "View all", conversionResult: "Conversion result", currentBatch: "Current batch", close: "Close",
    previewMode: "Preview mode", renderedPreview: "Reading view", markdownSource: "Markdown source", detailsAndResources: "Details and resources", batchResults: "Batch results", moreBatchResults: "More batch results", noMarkdownResult: "This task has no Markdown result",
    noResources: "No extracted resources", diagnostics: "Diagnostics", noDiagnostics: "No diagnostics", otherArtifacts: "Other artifacts", file: "File", size: "Artifacts", artifacts: "artifacts", batchOf: "{count} files in batch",
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
