import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  CheckCircle2, CircleAlert, FileAudio, LoaderCircle, Mic, Pause, Play, Radio,
  Settings2, Square, Trash2, Upload, Users, X,
} from "lucide-react";
import type { ApiClient, ComponentStatus, MeetingOptions, TaskRecord } from "./api";
import { ApiError, meetingOptionsForLocale } from "./api";
import { AudioSetupDialog } from "./conversion-controls";
import { useI18n, type MessageKey } from "./i18n";
import { ResultDialog } from "./result-page";
import {
  appendRecordingChunk, beginRecordingDraft, clearRecordingDraft, loadRecordingDraft,
  RecordingDraftLimitError, type RecordingDraft,
} from "./recording-store";
import {
  MEETING_FILE_ACCEPT, TERMINAL, bytesLabel, supportsMeetingFile, taskName,
} from "./task-ui";

type RecorderState = "idle" | "requesting" | "recording" | "paused" | "stopping" | "draft";
type RecordingSource = "microphone" | "system" | "mixed";
const MEDIA_REQUEST_TIMEOUT_MS = 30_000;
const NORMALIZED_SAMPLE_RATE = 16_000;

async function requestMedia(request: Promise<MediaStream>, timeoutMs: number,
  timeoutMessage: string): Promise<MediaStream> {
  let expired = false;
  let timer = 0;
  const timeout = new Promise<never>((_, reject) => {
    timer = window.setTimeout(() => {
      expired = true;
      reject(new DOMException(timeoutMessage, "TimeoutError"));
    }, timeoutMs);
  });
  void request.then((lateStream) => {
    if (expired) lateStream.getTracks().forEach((track) => track.stop());
  }, () => {});
  try { return await Promise.race([request, timeout]); }
  finally { window.clearTimeout(timer); }
}

export async function requestMicrophone(constraints: MediaStreamConstraints,
  timeoutMs = MEDIA_REQUEST_TIMEOUT_MS): Promise<MediaStream> {
  return requestMedia(navigator.mediaDevices.getUserMedia(constraints), timeoutMs,
    "microphone permission request timed out");
}

export async function requestSystemAudio(timeoutMs = MEDIA_REQUEST_TIMEOUT_MS): Promise<MediaStream> {
  if (!navigator.mediaDevices?.getDisplayMedia) {
    throw new DOMException("system audio capture is unsupported", "NotSupportedError");
  }
  const media = await requestMedia(navigator.mediaDevices.getDisplayMedia({ audio: true, video: true }),
    timeoutMs, "system audio permission request timed out");
  if (media.getAudioTracks().length === 0) {
    media.getTracks().forEach((track) => track.stop());
    throw new DOMException("the selected share does not include audio", "NotFoundError");
  }
  return media;
}

function recordingExtension(mimeType: string): string {
  return mimeType.includes("mp4") ? "m4a" : mimeType.includes("ogg") ? "ogg" : "webm";
}

function draftFile(draft: RecordingDraft): File {
  const stamp = new Date(draft.createdAtMs).toISOString().replaceAll(":", "-");
  return new File(draft.chunks, `meeting-${stamp}.${recordingExtension(draft.mimeType)}`, {
    type: draft.mimeType || "audio/webm", lastModified: draft.createdAtMs,
  });
}

function durationLabel(milliseconds: number): string {
  const total = Math.floor(milliseconds / 1000);
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor(total % 3600 / 60);
  const seconds = total % 60;
  return `${hours ? `${String(hours).padStart(2, "0")}:` : ""}${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
}

function preferredMimeType(): string {
  const candidates = ["audio/webm;codecs=opus", "audio/mp4", "audio/ogg;codecs=opus", "audio/webm"];
  return candidates.find((value) => MediaRecorder.isTypeSupported(value)) ?? "";
}

function progressMessage(stage: string | undefined, message: string | null | undefined): MessageKey {
  if (message?.startsWith("diarization.")) return "distinguishingSpeakers";
  if (message === "asr.normalize") return "preparingAudio";
  if (message?.startsWith("asr.")) return "recognizingContent";
  if (stage === "rendering" || stage === "completed") return "generatingTranscript";
  return "recognizingContent";
}

export function MeetingPage({ api, initialTaskId }: { api: ApiClient; initialTaskId?: string | undefined }) {
  const { locale, t } = useI18n();
  const input = useRef<HTMLInputElement>(null);
  const recorder = useRef<MediaRecorder | null>(null);
  const stream = useRef<MediaStream | null>(null);
  const sourceStreams = useRef<MediaStream[]>([]);
  const mixer = useRef<AudioContext | null>(null);
  const chunkIndex = useRef(0);
  const elapsedRef = useRef(0);
  const writes = useRef<Promise<void>>(Promise.resolve());
  const watcher = useRef<AbortController | null>(null);
  const diarizationTouched = useRef(false);
  const languageTouched = useRef(false);
  const recordingFailure = useRef<MessageKey | null>(null);
  const [state, setState] = useState<RecorderState>("idle");
  const [elapsed, setElapsed] = useState(0);
  const [file, setFile] = useState<File | null>(null);
  const [fromDraft, setFromDraft] = useState(false);
  const [message, setMessage] = useState("");
  const [options, setOptions] = useState<MeetingOptions>(() => meetingOptionsForLocale(locale));
  const [audioStatus, setAudioStatus] = useState<ComponentStatus>();
  const [diarizationStatus, setDiarizationStatus] = useState<ComponentStatus>();
  const [setup, setSetup] = useState(false);
  const [devices, setDevices] = useState<MediaDeviceInfo[]>([]);
  const [deviceId, setDeviceId] = useState("");
  const [recordingSource, setRecordingSource] = useState<RecordingSource>("microphone");
  const [task, setTask] = useState<TaskRecord | null>(null);
  const [recent, setRecent] = useState<TaskRecord[]>([]);
  const [stage, setStage] = useState("");
  const [activeTaskId, setActiveTaskId] = useState<string | undefined>(initialTaskId);

  useEffect(() => setActiveTaskId(initialTaskId), [initialTaskId]);

  useEffect(() => {
    if (!languageTouched.current) {
      setOptions((current) => ({
        ...current, transcriptLanguage: meetingOptionsForLocale(locale).transcriptLanguage,
      }));
    }
  }, [locale]);

  const refresh = useCallback(async (signal?: AbortSignal) => {
    const [status, page] = await Promise.all([api.status(signal), api.listTasks({ limit: 100 }, signal)]);
    setAudioStatus(status.audioTranscription);
    setDiarizationStatus(status.speakerDiarization);
    if (!diarizationTouched.current) {
      setOptions((current) => ({
        ...current, diarize: status.speakerDiarization?.available === true,
      }));
    }
    setRecent(page.tasks.filter((item) => item.workflow === "meetingTranscript"));
  }, [api]);

  useEffect(() => {
    const controller = new AbortController();
    void refresh(controller.signal).catch(() => { if (!controller.signal.aborted) setMessage(t("loadTasksError")); });
    return () => controller.abort();
  }, [refresh, t]);

  useEffect(() => {
    let cancelled = false;
    void loadRecordingDraft().then((draft) => {
      if (!draft || cancelled) return;
      setFile(draftFile(draft)); setElapsed(draft.elapsedMs); elapsedRef.current = draft.elapsedMs;
      setFromDraft(true); setState("draft"); setMessage(t("recordingRecovered"));
    }).catch(() => { if (!cancelled) setMessage(t("recordingStorageUnavailable")); });
    return () => { cancelled = true; };
    // A draft is restored once when the meeting route mounts. Locale changes
    // must not overwrite an imported file or a live recorder state.
  }, []);

  useEffect(() => {
    if (state !== "recording") return;
    const timer = window.setInterval(() => {
      elapsedRef.current += 1000; setElapsed(elapsedRef.current);
    }, 1000);
    return () => window.clearInterval(timer);
  }, [state]);

  useEffect(() => () => {
    watcher.current?.abort();
    if (recorder.current && recorder.current.state !== "inactive") recorder.current.stop();
    stream.current?.getTracks().forEach((track) => track.stop());
    sourceStreams.current.forEach((source) => source.getTracks().forEach((track) => track.stop()));
    void mixer.current?.close().catch(() => {});
  }, []);

  const loadDevices = async () => {
    const values = await navigator.mediaDevices.enumerateDevices();
    setDevices(values.filter((device) => device.kind === "audioinput"));
  };

  const finalizeRecording = async () => {
    try {
      await writes.current;
      const draft = await loadRecordingDraft();
      if (!draft) throw new Error("empty draft");
      const next = draftFile(draft);
      setFile(next); setFromDraft(true); setState("draft"); setMessage("");
    } catch {
      await clearRecordingDraft().catch(() => {});
      const failure = recordingFailure.current ?? "recordingSaveFailed";
      recordingFailure.current = null;
      setState("idle"); setMessage(t(failure));
    } finally {
      stream.current?.getTracks().forEach((track) => track.stop());
      sourceStreams.current.forEach((source) => source.getTracks().forEach((track) => track.stop()));
      sourceStreams.current = [];
      void mixer.current?.close().catch(() => {}); mixer.current = null;
      stream.current = null; recorder.current = null;
    }
  };

  const start = async () => {
    const needsMicrophone = recordingSource !== "system";
    const needsSystemAudio = recordingSource !== "microphone";
    if (!globalThis.MediaRecorder
      || needsMicrophone && !navigator.mediaDevices?.getUserMedia
      || needsSystemAudio && !navigator.mediaDevices?.getDisplayMedia
      || recordingSource === "mixed" && !globalThis.AudioContext) {
      setMessage(t("recordingUnsupported")); return;
    }
    setState("requesting"); setMessage("");
    const acquired: MediaStream[] = [];
    let mixing: AudioContext | null = null;
    let requesting: "microphone" | "system" = needsSystemAudio ? "system" : "microphone";
    let draftStarted = false;
    try {
      if (recordingSource === "mixed") {
        mixing = new AudioContext();
        await mixing.resume();
      }
      if (needsSystemAudio) acquired.push(await requestSystemAudio());
      if (needsMicrophone) {
        requesting = "microphone";
        acquired.push(await requestMicrophone({
          audio: deviceId ? { deviceId: { exact: deviceId } } : true, video: false,
        }));
      }
      const media = mixing ? (() => {
        const destination = mixing.createMediaStreamDestination();
        for (const source of acquired) {
          mixing.createMediaStreamSource(new MediaStream(source.getAudioTracks())).connect(destination);
        }
        return destination.stream;
      })() : new MediaStream(acquired[0]?.getAudioTracks() ?? []);
      if (media.getAudioTracks().length === 0) throw new DOMException("no audio track", "NotFoundError");
      const mimeType = preferredMimeType();
      const next = mimeType ? new MediaRecorder(media, { mimeType }) : new MediaRecorder(media);
      await beginRecordingDraft(next.mimeType || mimeType || "audio/webm");
      draftStarted = true;
      recordingFailure.current = null;
      chunkIndex.current = 0; elapsedRef.current = 0; setElapsed(0); setFile(null); setFromDraft(false);
      writes.current = Promise.resolve(); stream.current = media; sourceStreams.current = acquired;
      mixer.current = mixing; recorder.current = next;
      next.addEventListener("dataavailable", (event) => {
        if (event.data.size === 0) return;
        const index = chunkIndex.current++;
        writes.current = writes.current.then(() => appendRecordingChunk(index, event.data, elapsedRef.current));
        void writes.current.catch((error: unknown) => {
          if (recorder.current !== next || next.state === "inactive") return;
          recordingFailure.current = error instanceof RecordingDraftLimitError
            ? "fileTooLarge" : "recordingSaveFailed";
          setState("stopping");
          next.stop();
        });
      });
      next.addEventListener("error", () => {
        if (recorder.current !== next) return;
        recordingFailure.current = "recordingSaveFailed";
        setState("stopping");
        if (next.state !== "inactive") next.stop();
      }, { once: true });
      next.addEventListener("stop", () => { void finalizeRecording(); }, { once: true });
      for (const track of acquired.flatMap((source) => source.getTracks())) {
        track.addEventListener("ended", () => {
          if (recorder.current !== next || next.state === "inactive") return;
          setState("stopping"); next.stop();
        }, { once: true });
      }
      next.start(2_000); setState("recording");
      void loadDevices().catch(() => {});
    } catch (error) {
      if (draftStarted) await clearRecordingDraft().catch(() => {});
      acquired.forEach((source) => source.getTracks().forEach((track) => track.stop()));
      stream.current?.getTracks().forEach((track) => track.stop());
      sourceStreams.current = [];
      void mixing?.close().catch(() => {}); mixer.current = null;
      stream.current = null; recorder.current = null; setState("idle");
      setMessage(error instanceof DOMException && error.name === "NotAllowedError"
        ? t(requesting === "system" ? "systemAudioPermissionDenied" : "microphonePermissionDenied")
        : error instanceof DOMException && error.name === "TimeoutError"
          ? t(requesting === "system" ? "systemAudioPermissionTimedOut" : "microphonePermissionTimedOut")
          : error instanceof DOMException && error.name === "NotFoundError" && requesting === "system"
            ? t("systemAudioMissing")
            : requesting === "system" ? t("systemAudioUnavailable") : t("microphoneUnavailable"));
    }
  };

  const pause = () => { recorder.current?.pause(); setState("paused"); };
  const resume = () => { recorder.current?.resume(); setState("recording"); };
  const stop = () => {
    const current = recorder.current;
    if (!current || current.state === "inactive") return;
    setState("stopping"); current.stop();
  };

  const discard = async () => {
    if (fromDraft) await clearRecordingDraft().catch(() => {});
    setFile(null); setFromDraft(false); setElapsed(0); elapsedRef.current = 0; setState("idle"); setMessage("");
  };

  const chooseFile = async (next: File | undefined) => {
    if (!next) return;
    if (!supportsMeetingFile(next.name)) { setMessage(t("unsupportedRecording")); return; }
    if (next.size > options.maxInputMiB * 1024 * 1024) { setMessage(t("fileTooLarge")); return; }
    if (fromDraft) {
      try { await clearRecordingDraft(); }
      catch { setMessage(t("recordingStorageUnavailable")); return; }
    }
    setFile(next); setFromDraft(false); setState("idle"); setElapsed(0); setMessage("");
  };

  const watchTask = useCallback((taskId: string) => {
    watcher.current?.abort();
    const controller = new AbortController(); watcher.current = controller;
    void api.watchTask(taskId, (event) => {
      setTask((current) => current ? { ...current, status: event.status,
        progressMillionths: event.progressMillionths, updatedAtMs: Date.now() } : current);
      const progressKey = progressMessage(event.execution?.stage, event.execution?.message);
      const processedFrames = event.execution?.totalUnits === null
        && event.execution?.message?.endsWith(".normalize")
        ? event.execution.completedUnits : null;
      setStage(processedFrames === null || processedFrames === undefined
        ? t(progressKey)
        : `${t(progressKey)} · ${durationLabel(processedFrames * 1_000 / NORMALIZED_SAMPLE_RATE)}`);
      if (event.terminal) void api.getTask(taskId).then((record) => {
        setTask(record); setRecent((items) => [record, ...items.filter((item) => item.id !== record.id)]);
        if (record.status === "succeeded") setActiveTaskId(record.id);
      });
    }, controller.signal).catch(() => { if (!controller.signal.aborted) setMessage(t("streamError")); });
  }, [api, t]);

  const watchedTaskId = task?.id;
  const watchedTaskActive = task ? !TERMINAL.has(task.status) : false;
  useEffect(() => {
    if (!watchedTaskId || !watchedTaskActive) return;
    watchTask(watchedTaskId);
    return () => watcher.current?.abort();
  }, [watchedTaskActive, watchedTaskId, watchTask]);

  const submit = async () => {
    if (!file || task && !TERMINAL.has(task.status)) return;
    if (audioStatus?.available !== true) { setSetup(true); setMessage(t("audioNeedsSetupNearby")); return; }
    setMessage("");
    try {
      const next = await api.uploadMeeting(file, options); setTask(next); setStage(t("preparingAudio"));
      if (fromDraft) {
        try { await clearRecordingDraft(); }
        catch { setMessage(t("recordingStorageUnavailable")); }
      }
    } catch (error) {
      setMessage(`${t("uploadFailed")} (${error instanceof ApiError ? error.code : "unreachable"})`);
    }
  };

  const closeSetup = () => {
    setSetup(false);
    void refresh().catch(() => setMessage(t("loadTasksError")));
  };

  const installMedia = async () => {
    await api.installCapability("media");
    await refresh();
  };

  const transcriptHistory = useMemo(() => recent.filter((item) => item.id !== task?.id), [recent, task]);
  const updateVisibleTask = useCallback((record: TaskRecord) => {
    setTask((current) => current?.id === record.id ? record : current);
    setRecent((items) => [record, ...items.filter((item) => item.id !== record.id)]);
  }, []);
  const recording = state === "recording" || state === "paused" || state === "stopping";
  const capturing = state === "requesting" || recording;
  const sourceLocked = capturing || Boolean(file);
  const readyMessage: MessageKey = recordingSource === "system" ? "computerAudioReady"
    : recordingSource === "mixed" ? "mixedAudioReady" : "microphoneReady";
  const progress = task ? Math.round(task.progressMillionths / 10_000) : 0;

  return <section className="meeting-route" aria-labelledby="meeting-title">
    <div className="page-heading compact-heading"><div><p className="eyebrow">LOCAL MEETING</p><h1 id="meeting-title">{t("meetingNotes")}</h1></div><p>{t("meetingIntro")}</p></div>
    <div className="meeting-layout">
      <section className="card recording-card" aria-labelledby="recording-title">
        <div className="card-heading"><div><p className="section-kicker">{t("liveMeeting")}</p><h2 id="recording-title">{t("recordMeeting")}</h2></div><Radio size={21} aria-hidden="true" /></div>
        <div className={`recorder-console ${recording ? "active" : ""}`}>
          <div className="recording-status"><span className="recording-orb"><Mic size={28} aria-hidden="true" /></span><div><strong>{state === "recording" ? t("recordingNow") : state === "paused" ? t("recordingPaused") : state === "stopping" ? t("savingRecording") : state === "requesting" ? t("connectingAudioSource") : file ? t("recordingReady") : t(readyMessage)}</strong><time>{durationLabel(elapsed)}</time></div></div>
          <div className="recorder-actions">
            {!recording && !file && <button type="button" disabled={state === "requesting"} onClick={() => void start()}>{state === "requesting" ? <LoaderCircle className="spin" size={18} /> : <Mic size={18} />}{t("startRecording")}</button>}
            {state === "recording" && <button className="secondary" type="button" onClick={pause}><Pause size={17} />{t("pauseRecording")}</button>}
            {state === "paused" && <button className="secondary" type="button" onClick={resume}><Play size={17} />{t("resumeRecording")}</button>}
            {(state === "recording" || state === "paused") && <button type="button" onClick={stop}><Square size={16} />{t("endRecording")}</button>}
            {file && <button className="secondary" type="button" onClick={() => void discard()}><Trash2 size={16} />{t("discardRecording")}</button>}
          </div>
          <label className="recording-source"><span>{t("recordingSource")}</span><select disabled={sourceLocked} value={recordingSource} onChange={(event) => setRecordingSource(event.target.value as RecordingSource)}><option value="microphone">{t("microphoneOnly")}</option><option value="system">{t("computerAudioOnly")}</option><option value="mixed">{t("microphoneAndComputerAudio")}</option></select>{recordingSource !== "microphone" && <small>{t("computerAudioCaptureHelp")}</small>}</label>
          {devices.length > 0 && recordingSource !== "system" && !sourceLocked && <label className="microphone-select"><span>{t("microphone")}</span><select value={deviceId} onChange={(event) => setDeviceId(event.target.value)}><option value="">{t("systemDefaultMicrophone")}</option>{devices.map((device, index) => <option key={device.deviceId} value={device.deviceId}>{device.label || `${t("microphone")} ${index + 1}`}</option>)}</select></label>}
        </div>

        <div className="meeting-divider"><span>{t("orImportRecording")}</span></div>
        <button className="secondary import-recording" type="button" disabled={capturing} onClick={() => input.current?.click()}><Upload size={17} />{t("importRecording")}</button>
        <input ref={input} className="visually-hidden" type="file" disabled={capturing} accept={MEETING_FILE_ACCEPT} onChange={(event) => { void chooseFile(event.target.files?.[0]); event.currentTarget.value = ""; }} />
        {file && <div className="selected-recording"><FileAudio size={20} /><div><strong>{file.name}</strong><small>{bytesLabel(file.size)}{fromDraft ? ` · ${t("localDraft")}` : ""}</small></div><button className="icon-button neutral" type="button" aria-label={t("remove")} onClick={() => void discard()}><X size={17} /></button></div>}
        {message && <div className="meeting-feedback" role="status"><CircleAlert size={16} /><span>{message}</span></div>}
      </section>

      <section className="card transcript-card" aria-labelledby="transcript-options-title">
        <div className="card-heading"><div><p className="section-kicker">{t("transcript")}</p><h2 id="transcript-options-title">{t("transcriptSettings")}</h2></div><Settings2 size={20} /></div>
        <div className="meeting-options">
          <label className="transcript-language"><span>{t("transcriptLanguage")}</span><select value={options.transcriptLanguage} onChange={(event) => { languageTouched.current = true; setOptions((current) => ({ ...current, transcriptLanguage: event.target.value as MeetingOptions["transcriptLanguage"] })); }}><option value="auto">{t("automaticDetection")}</option><option value="zh-Hans">{t("simplifiedChinese")}</option><option value="zh-Hant">{t("traditionalChinese")}</option><option value="en">{t("english")}</option></select><small>{t("transcriptLanguageHelp")}</small></label>
          <div className="meeting-option-row"><span><Users size={18} /><span><strong>{t("distinguishSpeakers")}</strong><small>{diarizationStatus?.available ? t("anonymousSpeakerLabels") : t("speakerSetupNeeded")}</small></span></span><label className={`switch ${diarizationStatus?.available ? "" : "unavailable"}`}><span className="visually-hidden">{t("distinguishSpeakers")}</span><input type="checkbox" disabled={!diarizationStatus?.available} checked={options.diarize && diarizationStatus?.available === true} onChange={(event) => { diarizationTouched.current = true; setOptions((current) => ({ ...current, diarize: event.target.checked })); }} /><span /></label></div>
          {options.diarize && diarizationStatus?.available === true && <label className="expected-speakers"><span>{t("expectedSpeakers")}</span><select value={options.expectedSpeakers ?? ""} onChange={(event) => setOptions((current) => ({ ...current, expectedSpeakers: event.target.value ? Number(event.target.value) : null }))}><option value="">{t("automatic")}</option>{Array.from({ length: 16 }, (_, index) => index + 1).map((count) => <option key={count} value={count}>{count}</option>)}</select></label>}
          {(audioStatus?.available === false || diarizationStatus?.available === false) && <button className="prepare-media" type="button" onClick={() => setSetup(true)}><CircleAlert size={16} />{t("prepareAudioComponents")}</button>}
        </div>
        {task && <div className={`meeting-task ${task.status}`}><div><strong>{taskName(task, t("meetingTranscript"))}</strong><span>{t(task.status)}{stage ? ` · ${stage}` : ""}</span></div>{TERMINAL.has(task.status) ? <button className="secondary" type="button" onClick={() => setActiveTaskId(task.id)}>{task.status === "succeeded" ? <CheckCircle2 size={16} /> : <CircleAlert size={16} />}{t("viewTranscript")}</button> : <progress max="100" value={progress} aria-label={`${progress}%`} />}</div>}
        <button className="convert-button" type="button" disabled={!file || Boolean(task && !TERMINAL.has(task.status)) || state === "stopping"} onClick={() => void submit()}>{task && !TERMINAL.has(task.status) ? <LoaderCircle className="spin" size={19} /> : <FileAudio size={19} />}{task && !TERMINAL.has(task.status) ? t("transcribing") : t("generateTranscript")}</button>

        <div className="meeting-history"><div className="recent-history-title"><strong>{t("recentMeetings")}</strong><span>{transcriptHistory.length}</span></div>{transcriptHistory.length === 0 ? <p>{t("noMeetingHistory")}</p> : <ul>{transcriptHistory.slice(0, 8).map((item) => <li key={item.id}><button type="button" onClick={() => setActiveTaskId(item.id)}><span><strong>{taskName(item, t("meetingTranscript"))}</strong><small>{new Date(item.updatedAtMs).toLocaleString(locale)}</small></span><span className={`history-status ${item.status}`}>{t(item.status)}</span></button></li>)}</ul>}</div>
      </section>
    </div>
    <AudioSetupDialog status={audioStatus?.available === false ? audioStatus : diarizationStatus} open={setup} onClose={closeSetup} onInstall={installMedia} />
    {activeTaskId && <ResultDialog api={api} taskId={activeTaskId} onSelectTask={setActiveTaskId} onClose={() => setActiveTaskId(undefined)} onTaskRemoved={(id) => setRecent((items) => items.filter((item) => item.id !== id))} onTaskUpdated={updateVisibleTask} />}
  </section>;
}
