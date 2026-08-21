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
    capabilityCenter: "本地能力", capabilityCenterIntro: "查看文档转换服务是否可以正常工作。", allLocalServicesReady: "本地转换服务已就绪，可以返回工作台继续。", checkingSystemDetail: "正在确认本地转换服务与任务存储。", audioEnvironment: "音频转写环境", audioEnvironmentReady: "Whisper、Silero VAD、3D-Speaker、ONNX Runtime 与固定 LGPL FFmpeg 已通过本机验证。", audioEnvironmentSetup: "会议逐字稿需要本地 Whisper、Silero VAD、3D-Speaker、ONNX Runtime 和与当前版本匹配的固定 LGPL FFmpeg；准备完成后关闭此窗口，工作台会重新检查。",
    prepareAudioTitle: "准备音频转写", installWhisperModel: "安装本地音频模型", prepareFfmpegRuntime: "准备隔离运行时", ffmpegRuntimeNote: "完整离线包随附全部组件；精简安装会由 setup media 下载并校验所需模型，FFmpeg 与 ONNX Runtime 必须与当前版本匹配。", copyCommand: "复制命令", done: "完成",
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
    tooManyFiles: "每批最多 100 个文件。", fileTooLarge: "文件超过当前单文件上限。", batchTooLarge: "每批文件总量不得超过 1 GiB。", uploadFailed: "上传失败", retryNeedsFile: "刷新后重试需要重新选择原文件。", unsupportedFiles: "已跳过不支持的文件：{files}",
    unsupportedFormatFailure: "不支持这种文件格式", malformedInputFailure: "文件内容损坏或格式不正确", encryptedInputFailure: "文件已加密或受密码保护", resourceLimitFailure: "文件超出当前资源限制", ocrFailure: "文字识别失败", aiFailure: "AI 处理失败", networkFailure: "网络访问失败", ioFailure: "无法读取或写入文件", componentUnavailableFailure: "所需本地依赖尚未准备", timeoutFailure: "转换超时", recoveryFailure: "无法恢复此前的转换任务", internalFailure: "转换服务遇到内部错误", conversionFailedReason: "文件内容无法转换，请检查文件是否完整", invalidOptionsFailure: "转换设置无效", unreachableFailure: "无法连接本地转换服务", failureDetails: "失败详情",
    pin: "固定", unpin: "取消固定", pinned: "已固定", pinnedOnly: "仅固定任务", filterStatus: "状态筛选", allStatuses: "全部状态", loadMore: "加载更多", deleteTask: "永久删除", deleteWarning: "此操作不可恢复，将永久删除任务记录、输入与产物。是否继续？",
    taskDetails: "任务详情", created: "创建时间", updated: "更新时间", on: "开启",
    cleanup: "立即清理", cleanupWarning: "将按 30 天和 10 GiB 保留策略永久删除符合条件的未固定已完成任务，且不可恢复。是否继续？", cleanupResult: "清理完成：删除 {tasks} 个任务，释放 {bytes} MiB。",
    primaryNavigation: "主要导航", history: "历史记录", recentHistory: "最近记录", viewAllHistory: "查看全部", conversionResult: "转换结果", currentBatch: "当前批次", close: "关闭",
    previewMode: "预览模式", renderedPreview: "阅读预览", markdownSource: "Markdown 源码", detailsAndResources: "详情与资源", batchResults: "批次结果", moreBatchResults: "更多批次结果", noMarkdownResult: "此任务没有 Markdown 结果",
    noResources: "没有提取资源", diagnostics: "诊断信息", noDiagnostics: "没有诊断信息", otherArtifacts: "其他产物", file: "文件", size: "产物", artifacts: "项产物", batchOf: "同批 {count} 个文件",
    meetingNotes: "会议纪要", meetingIntro: "现场录制或导入已有录音，在本机生成带时间与匿名说话人的忠实逐字稿。", liveMeeting: "现场会议", recordMeeting: "录制会议",
    readyToRecord: "已准备录制", microphoneReady: "麦克风已待命", computerAudioReady: "电脑声音已待命", mixedAudioReady: "麦克风与电脑声音已待命", connectingAudioSource: "正在连接录制来源", recordingNow: "正在录制", recordingPaused: "录制已暂停", savingRecording: "正在保存录音", recordingReady: "录音已就绪",
    startRecording: "开始录制", pauseRecording: "暂停", resumeRecording: "继续", endRecording: "结束录制", discardRecording: "丢弃录音",
    microphone: "麦克风", systemDefaultMicrophone: "系统默认麦克风", microphonePermissionDenied: "没有获得麦克风权限，请在浏览器地址栏允许后重试。", microphonePermissionTimedOut: "麦克风授权没有响应，请检查浏览器地址栏的权限提示后重试。", microphoneUnavailable: "无法连接所选麦克风，请检查设备后重试。", recordingUnsupported: "当前浏览器不支持现场录制。",
    recordingSource: "录制来源", microphoneOnly: "仅麦克风", computerAudioOnly: "仅电脑声音", microphoneAndComputerAudio: "麦克风 + 电脑声音", computerAudioCaptureHelp: "开始后请选择要共享的标签页、窗口或屏幕，并开启共享音频；只保存声音，不保存画面。", systemAudioPermissionDenied: "没有获得电脑声音共享权限，请重新选择共享来源。", systemAudioPermissionTimedOut: "电脑声音共享没有响应，请完成或取消浏览器中的共享选择。", systemAudioMissing: "所选共享来源没有音频，请选择支持音频的来源并开启共享音频。", systemAudioUnavailable: "当前浏览器或所选来源无法录制电脑声音。",
    recordingRecovered: "已恢复上次中断前保存的录音，可直接生成逐字稿。", recordingStorageUnavailable: "浏览器本地录音存储不可用；当前录音无法跨刷新恢复。", recordingSaveFailed: "录音保存失败，请检查浏览器存储空间。",
    orImportRecording: "或者", importRecording: "导入已有录音", unsupportedRecording: "请选择 MP3、M4A、WAV、FLAC、OGG 或常见视频录音。", localDraft: "本地恢复录音",
    transcript: "逐字稿", transcriptSettings: "逐字稿设置", transcriptLanguage: "转写语言", simplifiedChinese: "简体中文", traditionalChinese: "繁体中文", english: "英语", transcriptLanguageHelp: "中文界面默认输出简体；也可保留自动检测或指定繁体。", distinguishSpeakers: "区分说话人", anonymousSpeakerLabels: "使用 Speaker 1、Speaker 2 等匿名标签", speakerSetupNeeded: "说话人组件尚未准备", expectedSpeakers: "预计人数", prepareAudioComponents: "准备音频组件",
    audioNeedsSetupNearby: "开始转写前需要先准备本地音频组件。", meetingTranscript: "会议逐字稿", viewTranscript: "查看逐字稿", transcribing: "正在生成逐字稿", generateTranscript: "生成逐字稿", recentMeetings: "最近会议", noMeetingHistory: "还没有会议逐字稿。",
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
    workbench: "Conversion workbench", workbenchIntro: "Upload local documents in batches, set one-time conversion policy, and follow every task live.",
    convertDocuments: "Convert documents", convertDocumentsIntro: "Add files, recognize content automatically, and get clean, structured Markdown.",
    sourceFiles: "Source files", conversionSettings: "Conversion settings", outputFormat: "Output format", recognitionMode: "Recognition mode",
    smart: "Automatic", precise: "Force recognition", forceRecognition: "Force recognition", recommended: "Recommended", separateAssets: "Save separately", embedAssets: "Embed", omitAssets: "Omit", advancedSettings: "Advanced settings",
    documentParsing: "Document parsing", imageOcr: "Image OCR", audioTranscription: "Audio transcription", localReady: "Ready locally", automaticDetection: "Auto detect", enabled: "Enabled", disabled: "Disabled", enableInWorkbench: "Enable in workbench", audioReady: "Ready", audioNeedsSetup: "Setup needed", prepareDependencies: "Prepare", capabilities: "Local capabilities",
    systemReady: "System ready", systemNeedsAttention: "Needs attention", checkingSystem: "Checking", moreActions: "More actions", latestResult: "Latest result", loadingPreview: "Loading preview…",
    batchLimitSummary: "Up to 100 files", resultsAndHistory: "Results and history", manageHistory: "Manage history",
    capabilityCenter: "Local capabilities", capabilityCenterIntro: "Check whether the document conversion service is working normally.", allLocalServicesReady: "The local conversion service is ready. Return to the workbench to continue.", checkingSystemDetail: "Checking the local conversion service and task store.", audioEnvironment: "Audio transcription environment", audioEnvironmentReady: "Whisper, Silero VAD, 3D-Speaker, ONNX Runtime, and the pinned LGPL FFmpeg passed local verification.", audioEnvironmentSetup: "Meeting transcripts need local Whisper, Silero VAD, 3D-Speaker, ONNX Runtime, and the pinned LGPL FFmpeg matching this build. Close this dialog after setup and the workbench will check again.",
    prepareAudioTitle: "Prepare audio transcription", installWhisperModel: "Install local audio models", prepareFfmpegRuntime: "Prepare isolated runtimes", ffmpegRuntimeNote: "The full offline package includes every component. A compact install uses setup media to download and verify the models; FFmpeg and ONNX Runtime must match this build.", copyCommand: "Copy command", done: "Done",
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
    tooManyFiles: "A batch can contain at most 100 files.", fileTooLarge: "A file exceeds the selected per-file limit.", batchTooLarge: "A batch cannot exceed 1 GiB.", uploadFailed: "Upload failed", retryNeedsFile: "After refresh, select the original file again to retry.", unsupportedFiles: "Skipped unsupported files: {files}",
    unsupportedFormatFailure: "This file format is not supported", malformedInputFailure: "The file is damaged or its format is invalid", encryptedInputFailure: "The file is encrypted or password protected", resourceLimitFailure: "The file exceeds the current resource limits", ocrFailure: "Text recognition failed", aiFailure: "AI processing failed", networkFailure: "Network access failed", ioFailure: "The file could not be read or written", componentUnavailableFailure: "A required local dependency is not ready", timeoutFailure: "Conversion timed out", recoveryFailure: "The previous conversion task could not be restored", internalFailure: "The conversion service encountered an internal error", conversionFailedReason: "The file content could not be converted; check that the file is complete", invalidOptionsFailure: "The conversion settings are invalid", unreachableFailure: "The local conversion service is unavailable", failureDetails: "Failure details",
    pin: "Pin", unpin: "Unpin", pinned: "Pinned", pinnedOnly: "Pinned only", filterStatus: "Status filter", allStatuses: "All statuses", loadMore: "Load more", deleteTask: "Delete permanently", deleteWarning: "This cannot be undone. The task record, input, and artifacts will be permanently deleted. Continue?",
    taskDetails: "Task details", created: "Created", updated: "Updated", on: "On",
    cleanup: "Clean up now", cleanupWarning: "This permanently deletes eligible unpinned completed tasks under the 30-day and 10 GiB retention policy and cannot be undone. Continue?", cleanupResult: "Cleanup complete: removed {tasks} tasks and reclaimed {bytes} MiB.",
    primaryNavigation: "Primary navigation", history: "History", recentHistory: "Recent", viewAllHistory: "View all", conversionResult: "Conversion result", currentBatch: "Current batch", close: "Close",
    previewMode: "Preview mode", renderedPreview: "Reading view", markdownSource: "Markdown source", detailsAndResources: "Details and resources", batchResults: "Batch results", moreBatchResults: "More batch results", noMarkdownResult: "This task has no Markdown result",
    noResources: "No extracted resources", diagnostics: "Diagnostics", noDiagnostics: "No diagnostics", otherArtifacts: "Other artifacts", file: "File", size: "Artifacts", artifacts: "artifacts", batchOf: "{count} files in batch",
    meetingNotes: "Meeting notes", meetingIntro: "Record a meeting live or import an existing recording, then create a faithful local transcript with timestamps and anonymous speakers.", liveMeeting: "Live meeting", recordMeeting: "Record meeting",
    readyToRecord: "Ready to record", microphoneReady: "Microphone ready", computerAudioReady: "Computer audio ready", mixedAudioReady: "Microphone and computer audio ready", connectingAudioSource: "Connecting recording sources", recordingNow: "Recording", recordingPaused: "Recording paused", savingRecording: "Saving recording", recordingReady: "Recording ready",
    startRecording: "Start recording", pauseRecording: "Pause", resumeRecording: "Resume", endRecording: "End recording", discardRecording: "Discard recording",
    microphone: "Microphone", systemDefaultMicrophone: "System default microphone", microphonePermissionDenied: "Microphone permission was denied. Allow it in the browser address bar and retry.", microphonePermissionTimedOut: "The microphone permission request did not respond. Check the browser address bar and retry.", microphoneUnavailable: "The selected microphone is unavailable. Check the device and retry.", recordingUnsupported: "This browser cannot record meetings.",
    recordingSource: "Recording source", microphoneOnly: "Microphone only", computerAudioOnly: "Computer audio only", microphoneAndComputerAudio: "Microphone + computer audio", computerAudioCaptureHelp: "Choose a tab, window, or screen after starting and enable shared audio. Video is never saved.", systemAudioPermissionDenied: "Computer audio sharing was not allowed. Choose a share source again.", systemAudioPermissionTimedOut: "Computer audio sharing did not respond. Complete or cancel the browser share picker.", systemAudioMissing: "The selected share has no audio. Choose an audio-capable source and enable shared audio.", systemAudioUnavailable: "This browser or selected source cannot capture computer audio.",
    recordingRecovered: "Recovered the recording saved before the interruption. It is ready to transcribe.", recordingStorageUnavailable: "Local browser recording storage is unavailable; this recording cannot survive a refresh.", recordingSaveFailed: "The recording could not be saved. Check browser storage space.",
    orImportRecording: "or", importRecording: "Import recording", unsupportedRecording: "Choose an MP3, M4A, WAV, FLAC, OGG, or common video recording.", localDraft: "recovered local recording",
    transcript: "Transcript", transcriptSettings: "Transcript settings", transcriptLanguage: "Transcript language", simplifiedChinese: "Simplified Chinese", traditionalChinese: "Traditional Chinese", english: "English", transcriptLanguageHelp: "Sets the recognition hint and deterministic Chinese script output.", distinguishSpeakers: "Distinguish speakers", anonymousSpeakerLabels: "Uses anonymous labels such as Speaker 1 and Speaker 2", speakerSetupNeeded: "Speaker components need setup", expectedSpeakers: "Expected speakers", prepareAudioComponents: "Prepare audio components",
    audioNeedsSetupNearby: "Prepare the local audio components before transcription.", meetingTranscript: "Meeting transcript", viewTranscript: "View transcript", transcribing: "Creating transcript", generateTranscript: "Create transcript", recentMeetings: "Recent meetings", noMeetingHistory: "No meeting transcripts yet.",
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
