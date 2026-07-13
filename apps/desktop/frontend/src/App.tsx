import {
  startTransition,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  AlertCircle,
  ArrowLeft,
  CheckCircle2,
  FileUp,
  GripHorizontal,
  Monitor,
  Pin,
  RefreshCw,
  Save,
  Settings2,
  Trash2,
  X,
} from "lucide-react";

type TileCoord = { x: number; y: number };

type RoiBounds = {
  left: number;
  top: number;
  right: number;
  bottom: number;
};

type AppSettings = {
  config_path: string;
  skill_key: string;
  retreat_key: string;
  window_opacity: number;
  always_on_top: boolean;
  operation_prompt_enabled: boolean;
  operation_prompt_lead_frames: number;
  operator_name_roi: RoiBounds;
};

type UiOperationLine = {
  index: number;
  frames: number;
  time: string;
  code: string;
  operator_name: string;
  tile: TileCoord | null;
  tile_label: string;
  line_text: string;
};

type RecordSummary = {
  id: string;
  title: string;
  stage_name: string;
  created_at_ms: number;
  updated_at_ms: number;
  deleted: boolean;
  operation_count: number;
  duration_frames: number;
};

type CurrentBattleSnapshot = {
  id: string;
  title: string;
  stage_name: string;
  created_at_ms: number;
  elapsed_frames: number;
  battle_state: string;
  raw_content: string;
  operations: UiOperationLine[];
};

type SetupPhase = "ready" | "needs_target" | "needs_calibration" | "invalid_config";

type SetupTarget = {
  fingerprint: string;
  name: string;
  detail: string;
};

type SetupSnapshot = {
  phase: SetupPhase;
  message: string;
  config_path: string;
  calibration_path: string | null;
  selected_target_fingerprint: string | null;
  targets: SetupTarget[];
};

type CalibrationPhase =
  | "idle"
  | "running"
  | "cancelling"
  | "succeeded"
  | "failed"
  | "cancelled";

type CalibrationSnapshot = {
  phase: CalibrationPhase;
  progress_percent: number;
  message: string;
  session_dir: string | null;
};

type CaptureState = "initializing" | "active" | "window_missing" | "error";

type DetailPreviewSnapshot = {
  elapsed_frames: number;
  width: number;
  height: number;
  data_url: string;
};

type UiSnapshot = {
  settings: AppSettings;
  setup: SetupSnapshot;
  calibration: CalibrationSnapshot;
  records: RecordSummary[];
  current_battle: CurrentBattleSnapshot | null;
  capture_state: CaptureState;
  logs_dir: string;
  recorder_error: string | null;
};

type AppBootstrap<T> = {
  protocol_version: number;
  revision: number;
  session_id: string | null;
  state: T;
};

type StatusChanged<T> = {
  revision: number;
  session_id: string | null;
  state: T;
};

type TimelineUpdate = {
  session_id: string;
  sequence: number;
  observed_at_ns: number;
  elapsed_frames: number;
  logical_frame: number | null;
  battle_state: string;
  capture_state: CaptureState;
};

type RecordDocument = {
  meta: RecordSummary;
  raw_content: string;
  operations: UiOperationLine[];
};

type ImportRecordResponse = {
  snapshot: UiSnapshot;
  document: RecordDocument;
};

const defaultSettings: AppSettings = {
  config_path: "config.json",
  skill_key: "SPACE",
  retreat_key: "2",
  window_opacity: 1,
  always_on_top: false,
  operation_prompt_enabled: false,
  operation_prompt_lead_frames: 90,
  operator_name_roi: {
    left: 0,
    top: 260 / 1068,
    right: 1 - 300 / 1907,
    bottom: 1 - (260 / 1068 + 80 / 1068),
  },
};

const tauriRuntime = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
const mockOperations = [
  createOperation(0, "00:00:00", "DP", "伊内丝", { x: 8, y: 6 }),
  createOperation(1, "00:00:08", "US", "伊内丝", { x: 8, y: 6 }),
  createOperation(2, "00:00:17", "RT", "伊内丝", { x: 8, y: 6 }),
  createOperation(3, "00:00:26", "DP", "玛恩纳", { x: 10, y: 4 }),
];

const mockRecord: RecordDocument = {
  meta: {
    id: "mock-record-1",
    title: "7.2 03:38:00 26s00f",
    stage_name: "墨染书阁",
    created_at_ms: Date.now() - 86_400_000,
    updated_at_ms: Date.now() - 86_400_000,
    deleted: false,
    operation_count: mockOperations.length,
    duration_frames: mockOperations[mockOperations.length - 1]?.frames ?? 0,
  },
  raw_content: `${mockOperations.map((operation) => operation.line_text).join("\n")}\n`,
  operations: mockOperations,
};

let mockSnapshot: UiSnapshot = {
  settings: defaultSettings,
  setup: {
    phase: "ready",
    message: "基础配置已就绪。",
    config_path: defaultSettings.config_path,
    calibration_path: "calibration/profile_preview.json",
    selected_target_fingerprint: "mock-target-1",
    targets: [
      {
        fingerprint: "mock-target-1",
        name: "Windows 明日方舟",
        detail: "明日方舟 [UnityWndClass]",
      },
    ],
  },
  calibration: {
    phase: "idle",
    progress_percent: 0,
    message: "尚未开始校准",
    session_dir: null,
  },
  records: [mockRecord.meta],
  current_battle: {
    id: "mock-current",
    title: "7.3 03:38:48",
    stage_name: "墨染书阁",
    created_at_ms: Date.now(),
    elapsed_frames: 0,
    battle_state: "2x_running",
    raw_content: buildOperationsText(mockOperations.slice(0, 2)),
    operations: mockOperations.slice(0, 2),
  },
  capture_state: "active",
  logs_dir: "logs/session-preview",
  recorder_error: null,
};

const mockDocuments = new Map<string, RecordDocument>([[mockRecord.meta.id, mockRecord]]);
const mockListeners = new Set<(snapshot: UiSnapshot) => void>();
let mockTickerStarted = false;
const MIN_EDITOR_PANE_HEIGHT_PX = 72;
const SPLITTER_HEIGHT_PX = 16;

export function App() {
  const [snapshot, setSnapshot] = useState<UiSnapshot>({
    settings: defaultSettings,
    setup: {
      phase: "needs_target",
      message: "",
      config_path: defaultSettings.config_path,
      calibration_path: null,
      selected_target_fingerprint: null,
      targets: [],
    },
    calibration: {
      phase: "idle",
      progress_percent: 0,
      message: "尚未开始校准",
      session_dir: null,
    },
    records: [],
    current_battle: null,
    capture_state: "initializing",
    logs_dir: "",
    recorder_error: null,
  });
  const [selectedTargetFingerprint, setSelectedTargetFingerprint] = useState("");
  const [selectedRecordId, setSelectedRecordId] = useState<string>("");
  const [selectedRecord, setSelectedRecord] = useState<RecordDocument | null>(null);
  const [editorTitle, setEditorTitle] = useState("");
  const [editorContent, setEditorContent] = useState("");
  const [currentEditorContent, setCurrentEditorContent] = useState("");
  const [editingHistoryTitle, setEditingHistoryTitle] = useState(false);
  const [showSettings, setShowSettings] = useState(false);
  const [showImportDialog, setShowImportDialog] = useState(false);
  const [importTitle, setImportTitle] = useState("");
  const [importContent, setImportContent] = useState("");
  const [settingsDraft, setSettingsDraft] = useState<AppSettings>(defaultSettings);
  const [detailPreview, setDetailPreview] = useState<DetailPreviewSnapshot | null>(null);
  const [historyPaneHeightPx, setHistoryPaneHeightPx] = useState<number | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const editorLayoutRef = useRef<HTMLElement | null>(null);
  const historyEditorRef = useRef<HTMLTextAreaElement | null>(null);
  const currentEditorRef = useRef<HTMLTextAreaElement | null>(null);
  const historyTitleInputRef = useRef<HTMLInputElement | null>(null);
  const currentBattleSyncRef = useRef({ battleId: "", snapshotText: "" });
  const currentSaveTimerRef = useRef<number | null>(null);
  const settingsSaveTimerRef = useRef<number | null>(null);
  const settingsSaveSeqRef = useRef(0);
  const showSettingsRef = useRef(showSettings);
  const settingsDraftRef = useRef(settingsDraft);
  const snapshotRef = useRef(snapshot);
  const stateRevisionRef = useRef(0);
  const sessionIdRef = useRef<string | null>(null);
  const timelineSequenceRef = useRef(0);
  const promptStateRef = useRef({
    battleId: "",
    recordId: "",
    leadFrames: -1,
    enabled: false,
    lastPromptedNextIndex: -1,
  });
  const audioContextRef = useRef<AudioContext | null>(null);

  useEffect(() => {
    showSettingsRef.current = showSettings;
  }, [showSettings]);

  useEffect(() => {
    settingsDraftRef.current = settingsDraft;
  }, [settingsDraft]);

  useEffect(() => {
    snapshotRef.current = snapshot;
  }, [snapshot]);

  useEffect(() => {
    let cancelled = false;
    let stopStatus: (() => void) | undefined;
    let stopTimeline: (() => void) | undefined;

    const applySnapshot = (next: UiSnapshot, revision?: number) => {
      if (revision !== undefined) {
        if (revision < stateRevisionRef.current) {
          return;
        }
        stateRevisionRef.current = revision;
      }
      setSnapshot(next);
      setSettingsDraft((current) => (showSettingsRef.current ? current : next.settings));
      setError(null);
    };

    const subscribe = async () => {
      try {
        stopStatus = await listenStatus((event) => {
          if (!cancelled) {
            if (event.session_id !== sessionIdRef.current) {
              sessionIdRef.current = event.session_id;
              timelineSequenceRef.current = 0;
            }
            applySnapshot(event.state, event.revision);
          }
        });
        stopTimeline = await listenTimeline((event) => {
          if (
            cancelled ||
            event.session_id !== sessionIdRef.current ||
            event.sequence <= timelineSequenceRef.current
          ) {
            return;
          }
          timelineSequenceRef.current = event.sequence;
          // The timeline is intentionally tiny and can be rendered at a lower
          // priority than command responses and editor input.
          startTransition(() => {
            setSnapshot((current) => ({
              ...current,
              capture_state: event.capture_state,
              current_battle: current.current_battle
                ? {
                    ...current.current_battle,
                    elapsed_frames: event.elapsed_frames,
                    battle_state: event.battle_state,
                  }
                : null,
            }));
          });
        });
        const bootstrap = await backendInvoke<AppBootstrap<UiSnapshot>>("get_bootstrap_snapshot");
        if (bootstrap.protocol_version !== 1) {
          throw new Error(`unsupported backend protocol ${bootstrap.protocol_version}`);
        }
        if (!cancelled) {
          sessionIdRef.current = bootstrap.session_id;
          timelineSequenceRef.current = 0;
          applySnapshot(bootstrap.state, bootstrap.revision);
        }
      } catch (reason) {
        if (!cancelled) {
          setError(stringifyError(reason));
        }
      }
    };

    void subscribe();
    return () => {
      cancelled = true;
      stopStatus?.();
      stopTimeline?.();
    };
  }, []);

  useEffect(() => {
    document.documentElement.style.setProperty(
      "--window-opacity",
      String(snapshot.settings.window_opacity.toFixed(2)),
    );
  }, [snapshot.settings.window_opacity]);

  useEffect(() => {
    if (!showSettings) {
      setDetailPreview(null);
      return;
    }
    let cancelled = false;
    let inFlight = false;
    const tick = async () => {
      if (cancelled || inFlight) {
        return;
      }
      inFlight = true;
      try {
        const next = await backendInvoke<DetailPreviewSnapshot | null>("get_detail_preview", {
          roi: settingsDraft.operator_name_roi,
        });
        if (!cancelled) {
          setDetailPreview(next);
        }
      } catch {
        if (!cancelled) {
          setDetailPreview(null);
        }
      } finally {
        inFlight = false;
        if (!cancelled) {
          window.setTimeout(tick, 120);
        }
      }
    };
    void tick();
    return () => {
      cancelled = true;
    };
  }, [showSettings, settingsDraft.operator_name_roi]);

  useEffect(() => {
    if (!selectedRecordId && snapshot.records.length > 0) {
      setSelectedRecordId(snapshot.records[0].id);
      return;
    }
    if (selectedRecordId && !snapshot.records.some((record) => record.id === selectedRecordId)) {
      setSelectedRecordId(snapshot.records[0]?.id ?? "");
      setSelectedRecord(null);
    }
  }, [selectedRecordId, snapshot.records]);

  useEffect(() => {
    const preferred =
      snapshot.setup.selected_target_fingerprint ?? snapshot.setup.targets[0]?.fingerprint ?? "";
    if (!selectedTargetFingerprint && preferred) {
      setSelectedTargetFingerprint(preferred);
      return;
    }
    if (
      selectedTargetFingerprint &&
      !snapshot.setup.targets.some((target) => target.fingerprint === selectedTargetFingerprint)
    ) {
      setSelectedTargetFingerprint(preferred);
    }
  }, [
    selectedTargetFingerprint,
    snapshot.setup.selected_target_fingerprint,
    snapshot.setup.targets,
  ]);

  useEffect(() => {
    if (!selectedRecordId) {
      setSelectedRecord(null);
      setEditorTitle("");
      setEditorContent("");
      setEditingHistoryTitle(false);
      return;
    }
    void loadSelectedRecord(selectedRecordId);
  }, [selectedRecordId]);

  useEffect(() => {
    if (!selectedRecord) {
      setEditorTitle("");
      setEditorContent("");
      setEditingHistoryTitle(false);
      return;
    }
    setEditorTitle(selectedRecord.meta.title);
    setEditorContent(selectedRecord.raw_content);
    setEditingHistoryTitle(false);
  }, [selectedRecord]);

  useEffect(() => {
    if (!editingHistoryTitle) {
      return;
    }
    historyTitleInputRef.current?.focus();
    historyTitleInputRef.current?.select();
  }, [editingHistoryTitle]);

  useEffect(() => {
    const battle = snapshot.current_battle;
    const nextBattleId = battle?.id ?? "";
    const nextText = battle?.raw_content ?? "";
    const sync = currentBattleSyncRef.current;

    if (!battle) {
      currentBattleSyncRef.current = { battleId: "", snapshotText: "" };
      setCurrentEditorContent("");
      return;
    }

    if (sync.battleId !== nextBattleId) {
      currentBattleSyncRef.current = { battleId: nextBattleId, snapshotText: nextText };
      setCurrentEditorContent(nextText);
      return;
    }

    if (nextText === sync.snapshotText) {
      return;
    }

    setCurrentEditorContent((current) => {
      if (nextText === current) {
        return current;
      }
      if (current === sync.snapshotText) {
        return nextText;
      }
      if (nextText.startsWith(sync.snapshotText)) {
        return `${current}${nextText.slice(sync.snapshotText.length)}`;
      }
      return nextText;
    });
    currentBattleSyncRef.current = { battleId: nextBattleId, snapshotText: nextText };
  }, [snapshot.current_battle?.id, snapshot.current_battle?.raw_content]);

  useEffect(() => {
    const battle = snapshot.current_battle;
    if (!battle) {
      if (currentSaveTimerRef.current !== null) {
        window.clearTimeout(currentSaveTimerRef.current);
        currentSaveTimerRef.current = null;
      }
      return;
    }

    const sync = currentBattleSyncRef.current;
    if (currentEditorContent === sync.snapshotText) {
      if (currentSaveTimerRef.current !== null) {
        window.clearTimeout(currentSaveTimerRef.current);
        currentSaveTimerRef.current = null;
      }
      return;
    }

    if (currentSaveTimerRef.current !== null) {
      window.clearTimeout(currentSaveTimerRef.current);
    }
    currentSaveTimerRef.current = window.setTimeout(() => {
      currentSaveTimerRef.current = null;
      void saveCurrentBattleText(battle.id, currentEditorContent, sync.snapshotText);
    }, 120);

    return () => {
      if (currentSaveTimerRef.current !== null) {
        window.clearTimeout(currentSaveTimerRef.current);
        currentSaveTimerRef.current = null;
      }
    };
  }, [currentEditorContent, snapshot.current_battle?.id]);

  const settingsDirty = useMemo(
    () => JSON.stringify(settingsDraft) !== JSON.stringify(snapshot.settings),
    [settingsDraft, snapshot.settings],
  );

  useEffect(() => {
    if (!showSettings || !settingsDirty) {
      if (settingsSaveTimerRef.current !== null) {
        window.clearTimeout(settingsSaveTimerRef.current);
        settingsSaveTimerRef.current = null;
      }
      return;
    }

    const draft = settingsDraft;
    if (settingsSaveTimerRef.current !== null) {
      window.clearTimeout(settingsSaveTimerRef.current);
    }
    settingsSaveTimerRef.current = window.setTimeout(() => {
      settingsSaveTimerRef.current = null;
      void persistSettingsDraft(draft);
    }, 180);

    return () => {
      if (settingsSaveTimerRef.current !== null) {
        window.clearTimeout(settingsSaveTimerRef.current);
        settingsSaveTimerRef.current = null;
      }
    };
  }, [settingsDirty, settingsDraft, showSettings]);

  const selectedRecordSummary = snapshot.records.find((record) => record.id === selectedRecordId) ?? null;
  const comparisonOperations = useMemo(
    () => parseOperationsText(selectedRecord ? editorContent : ""),
    [editorContent, selectedRecord],
  );

  const comparisonHighlight = useMemo(() => {
    if (!selectedRecord || !snapshot.current_battle) {
      return -1;
    }
    let index = -1;
    for (const operation of comparisonOperations) {
      if (operation.frames <= snapshot.current_battle.elapsed_frames) {
        index = operation.index;
      } else {
        break;
      }
    }
    return index;
  }, [comparisonOperations, selectedRecord, snapshot.current_battle]);

  const historyDirty = useMemo(() => {
    if (!selectedRecord) {
      return false;
    }
    return (
      editorTitle !== selectedRecord.meta.title || editorContent !== selectedRecord.raw_content
    );
  }, [editorContent, editorTitle, selectedRecord]);

  const historyContentDirty = useMemo(() => {
    if (!selectedRecord) {
      return false;
    }
    return editorContent !== selectedRecord.raw_content;
  }, [editorContent, selectedRecord]);

  const currentBattleText = currentEditorContent;

  const currentBattleLabel = useMemo(
    () => snapshot.current_battle?.title ?? "当前场次",
    [snapshot.current_battle],
  );

  const currentBattlePlaceholder = useMemo(() => {
    if (snapshot.capture_state === "window_missing") {
      return "窗口消失";
    }
    if (!snapshot.current_battle) {
      return "等待进入关卡";
    }
    return "本场尚未记录到操作";
  }, [snapshot.capture_state, snapshot.current_battle]);

  const historyLabel = useMemo(() => {
    if (!selectedRecordSummary) {
      return "无对照";
    }
    const title = editorTitle.trim() || selectedRecordSummary.title;
    return historyDirty ? `${title} · 未保存` : title;
  }, [editorTitle, historyDirty, selectedRecordSummary]);

  useEffect(() => {
    const textarea = historyEditorRef.current;
    if (!textarea || !selectedRecord || historyContentDirty) {
      return;
    }
    scrollTextAreaToLine(textarea, Math.max(comparisonHighlight, 0));
  }, [comparisonHighlight, historyContentDirty, selectedRecord]);

  useEffect(() => {
    const textarea = currentEditorRef.current;
    if (!textarea) {
      return;
    }
    textarea.scrollTop = textarea.scrollHeight;
  }, [snapshot.current_battle?.raw_content]);

  useEffect(() => {
    const battle = snapshot.current_battle;
    const enabled = snapshot.settings.operation_prompt_enabled;
    const leadFrames = Math.max(0, Math.floor(snapshot.settings.operation_prompt_lead_frames));
    const battleId = battle?.id ?? "";
    const recordId = selectedRecord?.meta.id ?? "";
    const promptState = promptStateRef.current;
    const contextChanged =
      promptState.battleId !== battleId ||
      promptState.recordId !== recordId ||
      promptState.leadFrames !== leadFrames ||
      promptState.enabled !== enabled;

    if (!enabled || !battle || !selectedRecord || comparisonOperations.length === 0) {
      promptStateRef.current = {
        battleId,
        recordId,
        leadFrames,
        enabled,
        lastPromptedNextIndex: -1,
      };
      return;
    }

    if (contextChanged) {
      promptState.battleId = battleId;
      promptState.recordId = recordId;
      promptState.leadFrames = leadFrames;
      promptState.enabled = enabled;
      promptState.lastPromptedNextIndex = -1;
    }

    const currentFrame = battle.elapsed_frames;
    const nextIndex = comparisonOperations.findIndex((operation) => operation.frames >= currentFrame);
    if (nextIndex < 0) {
      return;
    }

    const nextOperation = comparisonOperations[nextIndex];
    if (nextOperation.frames - currentFrame > leadFrames) {
      return;
    }
    if (promptState.lastPromptedNextIndex === nextIndex) {
      return;
    }

    promptState.lastPromptedNextIndex = nextIndex;
    void playPromptBeep(audioContextRef);
  }, [
    comparisonOperations,
    selectedRecord,
    snapshot.current_battle,
    snapshot.settings.operation_prompt_enabled,
    snapshot.settings.operation_prompt_lead_frames,
  ]);

  useEffect(() => {
    const container = editorLayoutRef.current;
    if (!container || typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(() => {
      const nextAvailableHeight = Math.max(
        container.getBoundingClientRect().height - SPLITTER_HEIGHT_PX,
        MIN_EDITOR_PANE_HEIGHT_PX * 2,
      );
      const nextMaxHeight = Math.max(
        MIN_EDITOR_PANE_HEIGHT_PX,
        nextAvailableHeight - MIN_EDITOR_PANE_HEIGHT_PX,
      );
      setHistoryPaneHeightPx((current) => {
        if (current === null) {
          return clampHistoryPaneHeight(nextAvailableHeight * 0.52, nextAvailableHeight);
        }
        return Math.max(MIN_EDITOR_PANE_HEIGHT_PX, Math.min(nextMaxHeight, current));
      });
    });
    observer.observe(container);
    return () => observer.disconnect();
  }, []);

  async function refreshSnapshot() {
    try {
      const bootstrap = await backendInvoke<AppBootstrap<UiSnapshot>>("get_bootstrap_snapshot");
      sessionIdRef.current = bootstrap.session_id;
      timelineSequenceRef.current = 0;
      stateRevisionRef.current = bootstrap.revision;
      setSnapshot(bootstrap.state);
      setSettingsDraft((current) => (showSettingsRef.current ? current : bootstrap.state.settings));
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    }
  }

  async function loadSelectedRecord(recordId: string) {
    try {
      const document = await backendInvoke<RecordDocument>("load_record", { recordId });
      setSelectedRecord(document);
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
      setSelectedRecord(null);
    }
  }

  async function handleSaveRecord() {
    if (!selectedRecord) {
      return;
    }
    setBusy("保存中");
    try {
      const next = await backendInvoke<UiSnapshot>("save_record", {
        recordId: selectedRecord.meta.id,
        title: editorTitle,
        rawContent: editorContent,
      });
      setSnapshot(next);
      setSettingsDraft((current) => (showSettings ? current : next.settings));
      await loadSelectedRecord(selectedRecord.meta.id);
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    } finally {
      setBusy(null);
    }
  }

  async function saveCurrentBattleText(
    battleId: string,
    rawContent: string,
    baseSnapshotText: string,
  ) {
    try {
      await backendInvoke<UiSnapshot>("save_current_battle_text", {
        battleId,
        rawContent,
        baseSnapshotText,
      });
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    }
  }

  async function commitHistoryTitleEdit() {
    if (!selectedRecord) {
      setEditingHistoryTitle(false);
      return;
    }
    const normalizedTitle = editorTitle.trim() || selectedRecord.meta.title;
    setEditingHistoryTitle(false);
    if (
      normalizedTitle === selectedRecord.meta.title &&
      editorContent === selectedRecord.raw_content
    ) {
      setEditorTitle(selectedRecord.meta.title);
      return;
    }
    setEditorTitle(normalizedTitle);
    setBusy("保存中");
    try {
      const next = await backendInvoke<UiSnapshot>("save_record", {
        recordId: selectedRecord.meta.id,
        title: normalizedTitle,
        rawContent: editorContent,
      });
      setSnapshot(next);
      setSettingsDraft((current) => (showSettings ? current : next.settings));
      await loadSelectedRecord(selectedRecord.meta.id);
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    } finally {
      setBusy(null);
    }
  }

  async function handleDeleteRecord() {
    if (!selectedRecordSummary) {
      return;
    }
    setBusy("删除中");
    try {
      const next = await backendInvoke<UiSnapshot>("set_record_deleted", {
        recordId: selectedRecordSummary.id,
        deleted: true,
      });
      setSnapshot(next);
      setSettingsDraft((current) => (showSettings ? current : next.settings));
      setSelectedRecordId(next.records[0]?.id ?? "");
      setSelectedRecord(null);
      setEditorTitle("");
      setEditorContent("");
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    } finally {
      setBusy(null);
    }
  }

  async function handleImportRecord() {
    if (!importTitle.trim()) {
      setError("请输入名称");
      return;
    }
    setBusy("导入中");
    try {
      const response = await backendInvoke<ImportRecordResponse>("import_record", {
        title: importTitle,
        rawContent: importContent,
      });
      setSnapshot(response.snapshot);
      setSettingsDraft((current) => (showSettings ? current : response.snapshot.settings));
      setSelectedRecordId(response.document.meta.id);
      setSelectedRecord(response.document);
      setEditorTitle(response.document.meta.title);
      setEditorContent(response.document.raw_content);
      setEditingHistoryTitle(false);
      setShowImportDialog(false);
      setImportTitle("");
      setImportContent("");
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    } finally {
      setBusy(null);
    }
  }

  async function handleTogglePinned() {
    setBusy("切换置顶");
    try {
      const next = await backendInvoke<UiSnapshot>("set_window_pinned", {
        pinned: !snapshot.settings.always_on_top,
      });
      setSnapshot(next);
      setSettingsDraft(next.settings);
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    } finally {
      setBusy(null);
    }
  }

  async function handleCloseSettings() {
    try {
      await flushSettingsDraft();
      setShowSettings(false);
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    }
  }

  async function handleRefreshSetup() {
    setBusy("检查配置");
    try {
      if (settingsDirty) {
        const updated = await applySettingsIfDirty();
        setSnapshot(updated);
        setSettingsDraft(updated.settings);
      }
      const next = await backendInvoke<UiSnapshot>("refresh_setup");
      setSnapshot(next);
      setSettingsDraft(next.settings);
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    } finally {
      setBusy(null);
    }
  }

  async function handleSaveSelectedTarget() {
    if (!selectedTargetFingerprint) {
      return;
    }
    setBusy("写入目标");
    try {
      if (settingsDirty) {
        const updated = await applySettingsIfDirty();
        setSnapshot(updated);
        setSettingsDraft(updated.settings);
      }
      const next = await backendInvoke<UiSnapshot>("save_selected_target", {
        fingerprint: selectedTargetFingerprint,
      });
      setSnapshot(next);
      setSettingsDraft(next.settings);
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    } finally {
      setBusy(null);
    }
  }

  async function handleStartCalibration() {
    setBusy("准备校准");
    try {
      if (settingsDirty) {
        const updated = await applySettingsIfDirty();
        setSnapshot(updated);
        setSettingsDraft(updated.settings);
      }
      const next = await backendInvoke<UiSnapshot>("start_calibration");
      setSnapshot(next);
      setSettingsDraft(next.settings);
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    } finally {
      setBusy(null);
    }
  }

  async function handleCancelCalibration() {
    setBusy("取消校准");
    try {
      const next = await backendInvoke<UiSnapshot>("cancel_calibration");
      setSnapshot(next);
      setSettingsDraft(next.settings);
      setError(null);
    } catch (reason) {
      setError(stringifyError(reason));
    } finally {
      setBusy(null);
    }
  }

  async function applySettingsIfDirty(): Promise<UiSnapshot> {
    return flushSettingsDraft();
  }

  async function persistSettingsDraft(draft: AppSettings): Promise<UiSnapshot> {
    const requestSeq = settingsSaveSeqRef.current + 1;
    settingsSaveSeqRef.current = requestSeq;
    try {
      const next = await backendInvoke<UiSnapshot>("save_app_settings", {
        settings: draft,
      });
      if (requestSeq === settingsSaveSeqRef.current) {
        setSnapshot(next);
        setSettingsDraft((current) =>
          !showSettingsRef.current || JSON.stringify(current) === JSON.stringify(draft)
            ? next.settings
            : current,
        );
      }
      setError(null);
      return next;
    } catch (reason) {
      setError(stringifyError(reason));
      throw reason;
    }
  }

  async function flushSettingsDraft(): Promise<UiSnapshot> {
    if (settingsSaveTimerRef.current !== null) {
      window.clearTimeout(settingsSaveTimerRef.current);
      settingsSaveTimerRef.current = null;
    }
    const liveSnapshot = snapshotRef.current;
    const liveDraft = settingsDraftRef.current;
    if (JSON.stringify(liveDraft) === JSON.stringify(liveSnapshot.settings)) {
      return liveSnapshot;
    }
    return persistSettingsDraft(liveDraft);
  }

  function startResize(event: ReactPointerEvent<HTMLButtonElement>) {
    const container = editorLayoutRef.current;
    if (!container) {
      return;
    }
    event.preventDefault();
    const totalHeight = container.getBoundingClientRect().height;
    const availableHeight = Math.max(
      totalHeight - SPLITTER_HEIGHT_PX,
      MIN_EDITOR_PANE_HEIGHT_PX * 2,
    );
    const startY = event.clientY;
    const startHeight = clampHistoryPaneHeight(
      historyPaneHeightPx ?? availableHeight * 0.52,
      availableHeight,
    );
    const onMove = (moveEvent: PointerEvent) => {
      const delta = moveEvent.clientY - startY;
      setHistoryPaneHeightPx(clampHistoryPaneHeight(startHeight + delta, availableHeight));
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  const historyPaneStyle = useMemo(() => {
    if (historyPaneHeightPx === null) {
      return {
        flex: "0 0 calc(52% - 8px)",
      } as const;
    }
    return {
      flex: "0 0 auto",
      height: `${historyPaneHeightPx}px`,
    } as const;
  }, [historyPaneHeightPx]);

  return (
    <div className="app-shell">
      {showSettings ? (
        <SettingsView
          busy={busy}
          calibration={snapshot.calibration}
          captureState={snapshot.capture_state}
          detailPreview={detailPreview}
          error={error ?? snapshot.recorder_error}
          onBack={() => void handleCloseSettings()}
          onCancelCalibration={handleCancelCalibration}
          onChange={setSettingsDraft}
          onRefreshSetup={handleRefreshSetup}
          onSaveTarget={handleSaveSelectedTarget}
          settings={settingsDraft}
          setup={snapshot.setup}
          selectedTargetFingerprint={selectedTargetFingerprint}
          onSelectTarget={setSelectedTargetFingerprint}
          onStartCalibration={handleStartCalibration}
        />
      ) : snapshot.setup.phase !== "ready" ? (
        <SetupView
          busy={busy}
          calibration={snapshot.calibration}
          error={error ?? snapshot.recorder_error}
          onOpenSettings={() => setShowSettings(true)}
          onRefresh={handleRefreshSetup}
          setup={snapshot.setup}
        />
      ) : (
        <>
          <header className="app-header">
            <div className="title-block">
              <h1>{formatFrameStamp(snapshot.current_battle?.elapsed_frames ?? 0)}</h1>
              <span>
                {snapshot.capture_state === "window_missing"
                  ? "窗口消失"
                  : snapshot.current_battle
                  ? `${snapshot.current_battle.stage_name} ${formatBattleStateLabel(
                      snapshot.current_battle.battle_state,
                    )}`
                  : "等待进入关卡"}
              </span>
            </div>
            <div className="header-actions">
              <IconButton
                active={snapshot.settings.always_on_top}
                icon={<Pin size={18} />}
                label="窗口置顶"
                onClick={handleTogglePinned}
              />
              <IconButton
                icon={<Settings2 size={18} />}
                label="设置"
                onClick={() => setShowSettings(true)}
              />
            </div>
          </header>

          <section className="toolbar-row">
            <div className="toolbar-group toolbar-group-compact">
              <select
                aria-label="选择过往记录"
                className="record-select"
                onChange={(event) => setSelectedRecordId(event.target.value)}
                value={selectedRecordId}
              >
                <option value="">无对照</option>
                {snapshot.records.map((record) => (
                  <option key={record.id} value={record.id}>
                    {record.title}
                  </option>
                ))}
              </select>
            </div>
            <div className="toolbar-actions">
              <IconButton
                className="primary-icon-button"
                disabled={!selectedRecord || !historyDirty}
                icon={<Save size={16} />}
                label="保存"
                onClick={handleSaveRecord}
              />
              <IconButton
                icon={<FileUp size={18} />}
                label="导入"
                onClick={() => {
                  setImportTitle("");
                  setImportContent("");
                  setShowImportDialog(true);
                }}
              />
              <IconButton
                disabled={!selectedRecordSummary}
                icon={<Trash2 size={18} />}
                label="逻辑删除"
                onClick={handleDeleteRecord}
              />
            </div>
          </section>

          {(error || snapshot.recorder_error || busy) && (
            <div className="status-banner">
              <AlertCircle size={16} />
              <span>{busy ?? error ?? snapshot.recorder_error}</span>
            </div>
          )}

          <main className="editor-layout" ref={editorLayoutRef}>
            <div className="editor-panel-slot" style={historyPaneStyle}>
                <section className="editor-pane">
                  <div className="pane-header">
                    {editingHistoryTitle && selectedRecord ? (
                      <input
                        aria-label="编辑记录名"
                        className="inline-title-input"
                        onBlur={() => void commitHistoryTitleEdit()}
                        onChange={(event) => setEditorTitle(event.target.value)}
                        onKeyDown={(event) => {
                          if (event.key === "Enter") {
                            event.preventDefault();
                            void commitHistoryTitleEdit();
                          } else if (event.key === "Escape") {
                            setEditorTitle(selectedRecord.meta.title);
                            setEditingHistoryTitle(false);
                          }
                        }}
                        ref={historyTitleInputRef}
                        value={editorTitle}
                      />
                    ) : (
                      <span
                        className="pane-meta pane-caption pane-caption-editable"
                        onDoubleClick={() => {
                          if (selectedRecord) {
                            setEditingHistoryTitle(true);
                          }
                        }}
                        title={selectedRecord ? "双击改名" : historyLabel}
                      >
                        {historyLabel}
                      </span>
                    )}
                  </div>
                  <textarea
                    aria-label="过往对照内容"
                    className="record-editor"
                    onChange={(event) => setEditorContent(event.target.value)}
                    placeholder="暂无对照"
                    readOnly={!selectedRecord}
                    ref={historyEditorRef}
                    spellCheck={false}
                    value={selectedRecord ? editorContent : ""}
                    wrap="off"
                  />
                </section>
            </div>

            <button className="splitter" onPointerDown={startResize} type="button">
              <GripHorizontal size={16} />
            </button>

            <div className="editor-panel-slot editor-panel-slot-fill">
                <section className="editor-pane">
                  <div className="pane-header">
                    <span className="pane-meta pane-caption">{currentBattleLabel}</span>
                  </div>
                  <textarea
                    aria-label="当前场次内容"
                    className="record-editor current-editor"
                    onChange={(event) => setCurrentEditorContent(event.target.value)}
                    placeholder={currentBattlePlaceholder}
                    ref={currentEditorRef}
                    spellCheck={false}
                    value={currentBattleText}
                    wrap="off"
                  />
                </section>
            </div>
          </main>

          {showImportDialog ? (
            <ImportDialog
              content={importContent}
              onCancel={() => {
                if (busy === "导入中") {
                  return;
                }
                setShowImportDialog(false);
              }}
              onChangeContent={setImportContent}
              onChangeTitle={setImportTitle}
              onSubmit={() => void handleImportRecord()}
              title={importTitle}
            />
          ) : null}
        </>
      )}
    </div>
  );
}

function SetupView(props: {
  setup: SetupSnapshot;
  calibration: CalibrationSnapshot;
  busy: string | null;
  error: string | null;
  onRefresh: () => void;
  onOpenSettings: () => void;
}) {
  const { setup, calibration, busy, error, onRefresh, onOpenSettings } = props;
  const waitingCalibration = setup.phase === "needs_calibration" || calibration.phase === "running";

  return (
    <div className="setup-page">
      <header className="app-header">
        <div className="title-block">
          <h1>TIME</h1>
        </div>
        <div className="header-actions">
          <IconButton icon={<Settings2 size={18} />} label="设置" onClick={onOpenSettings} />
        </div>
      </header>

      {(busy || error) && (
        <div className="status-banner">
          <AlertCircle size={16} />
          <span>{busy ?? error}</span>
        </div>
      )}

      <section className="setup-section">
        <div className="pane-header">
          <div className="pane-title">
            <span className="pane-label">
              {waitingCalibration ? "等待校准" : "首次配置"}
            </span>
          </div>
          <span className="setup-inline-icon">
            {waitingCalibration ? <CheckCircle2 size={18} /> : <Monitor size={18} />}
          </span>
        </div>
        <div className="setup-stack">
          <p className="setup-copy">{setup.message}</p>
          <div className="setup-callout">
            <span>{calibrationPhaseLabel(calibration.phase)}</span>
            <strong>{calibration.message}</strong>
            <p>目标选择、校准启动、取消和重试都统一放在设置界面。</p>
          </div>

          <div className="editor-actions">
            <button className="command-button" onClick={onOpenSettings} type="button">
              <Settings2 size={16} />
              打开设置
            </button>
            <button className="ghost-button" onClick={onRefresh} type="button">
              <RefreshCw size={16} />
              重新检查
            </button>
          </div>
        </div>
      </section>
    </div>
  );
}

function SettingsView(props: {
  settings: AppSettings;
  setup: SetupSnapshot;
  calibration: CalibrationSnapshot;
  captureState: CaptureState;
  detailPreview: DetailPreviewSnapshot | null;
  selectedTargetFingerprint: string;
  busy: string | null;
  error: string | null;
  onBack: () => void;
  onRefreshSetup: () => void;
  onSaveTarget: () => void;
  onStartCalibration: () => void;
  onCancelCalibration: () => void;
  onSelectTarget: (fingerprint: string) => void;
  onChange: (settings: AppSettings) => void;
}) {
  const {
    settings,
    setup,
    calibration,
    captureState,
    detailPreview,
    selectedTargetFingerprint,
    busy,
    error,
    onBack,
    onRefreshSetup,
    onSaveTarget,
    onStartCalibration,
    onCancelCalibration,
    onSelectTarget,
    onChange,
  } = props;
  const calibrationActive =
    calibration.phase === "running" || calibration.phase === "cancelling";
  const canStartCalibration =
    !calibrationActive &&
    setup.phase !== "needs_target" &&
    setup.phase !== "invalid_config";
  return (
    <div className="settings-page">
      <header className="settings-header">
        <button className="icon-button" onClick={onBack} type="button">
          <ArrowLeft size={18} />
        </button>
        <div className="settings-title">
          <h2>设置</h2>
        </div>
      </header>

      {(busy || error) && (
        <div className="status-banner">
          <AlertCircle size={16} />
          <span>{busy ?? error}</span>
        </div>
      )}

      <div className="settings-sections">
        <section className="settings-section">
          <h3>引导</h3>
          <div className="setup-paths">
            <span>当前状态</span>
            <code>{setup.message}</code>
          </div>
          <p className="section-copy">
            设置会自动保存；目标切换和校准启动前也会再同步一次。
          </p>
        </section>

        <section className="settings-section">
          <h3>目标窗口</h3>
          <div className="target-list">
            {setup.targets.map((target) => {
              const selected = target.fingerprint === selectedTargetFingerprint;
              const active = target.fingerprint === setup.selected_target_fingerprint;
              return (
                <button
                  className={`target-row${selected ? " selected" : ""}`}
                  key={target.fingerprint}
                  onClick={() => onSelectTarget(target.fingerprint)}
                  type="button"
                >
                  <div className="target-row-head">
                    <span className="target-name">{target.name}</span>
                    {active ? <span className="target-badge">当前</span> : null}
                  </div>
                  <span className="target-detail">{target.detail}</span>
                </button>
              );
            })}
            {setup.targets.length === 0 ? <EmptyState label="没有找到明日方舟窗口" /> : null}
          </div>
          <div className="editor-actions">
            <button
              className="command-button"
              disabled={!selectedTargetFingerprint || calibrationActive}
              onClick={onSaveTarget}
              type="button"
            >
              <Save size={16} />
              保存目标
            </button>
            <button
              className="ghost-button"
              disabled={calibrationActive}
              onClick={onRefreshSetup}
              type="button"
            >
              <RefreshCw size={16} />
              重新扫描
            </button>
          </div>
        </section>

        <section className="settings-section">
          <h3>校准</h3>
          <div className="calibration-card">
            <div className="calibration-head">
              <div>
                <span className={`calibration-pill phase-${calibration.phase}`}>
                  {calibrationPhaseLabel(calibration.phase)}
                </span>
                <p className="calibration-message">{calibration.message}</p>
              </div>
              <span className="calibration-progress-text">
                {Math.round(calibration.progress_percent)}%
              </span>
            </div>
            <div className="calibration-progress-track">
              <div
                className={`calibration-progress-fill phase-${calibration.phase}`}
                style={{ width: `${Math.max(0, Math.min(100, calibration.progress_percent))}%` }}
              />
            </div>
            <p className="section-copy">
              进入关卡后启动校准。系统会等待费用条完成两轮采样，然后自动保存并切换到新的
              profile。
            </p>
            <div className="editor-actions">
              <button
                className="command-button"
                disabled={!canStartCalibration}
                onClick={onStartCalibration}
                type="button"
              >
                <CheckCircle2 size={16} />
                开始校准
              </button>
              <button
                className="ghost-button"
                disabled={!calibrationActive}
                onClick={onCancelCalibration}
                type="button"
              >
                <X size={16} />
                取消
              </button>
            </div>
          </div>
        </section>

        <section className="settings-section">
          <h3>快捷键</h3>
          <div className="two-col">
            <SettingField label="技能键">
              <input
                className="text-input"
                onChange={(event) => onChange({ ...settings, skill_key: event.target.value })}
                value={settings.skill_key}
              />
            </SettingField>
            <SettingField label="撤退键">
              <input
                className="text-input"
                onChange={(event) => onChange({ ...settings, retreat_key: event.target.value })}
                value={settings.retreat_key}
              />
            </SettingField>
          </div>
        </section>

        <section className="settings-section">
          <h3>窗口</h3>
          <SettingField label={`透明度 ${Math.round(settings.window_opacity * 100)}%`}>
            <input
              className="range-input"
              max={1}
              min={0.2}
              onChange={(event) =>
                onChange({ ...settings, window_opacity: Number(event.target.value) })
              }
              step={0.01}
              type="range"
              value={settings.window_opacity}
            />
          </SettingField>
          <label className="toggle-field">
            <input
              checked={settings.always_on_top}
              onChange={(event) =>
                onChange({ ...settings, always_on_top: event.target.checked })
              }
              type="checkbox"
            />
            <span>窗口置顶</span>
          </label>
        </section>

        <section className="settings-section">
          <h3>操作提示</h3>
          <label className="toggle-field">
            <input
              checked={settings.operation_prompt_enabled}
              onChange={(event) =>
                onChange({ ...settings, operation_prompt_enabled: event.target.checked })
              }
              type="checkbox"
            />
            <span>启用提示音</span>
          </label>
          <SettingField label="提前量（帧）">
            <input
              className="text-input"
              min={0}
              onChange={(event) =>
                onChange({
                  ...settings,
                  operation_prompt_lead_frames: Math.max(0, Math.floor(Number(event.target.value) || 0)),
                })
              }
              step={1}
              type="number"
              value={settings.operation_prompt_lead_frames}
            />
          </SettingField>
        </section>

        <section className="settings-section">
          <h3>名称截图框</h3>
          <div className="two-col">
            <RoiField
              label="左"
              value={settings.operator_name_roi.left}
              onChange={(value) =>
                onChange({
                  ...settings,
                  operator_name_roi: { ...settings.operator_name_roi, left: value },
                })
              }
            />
            <RoiField
              label="上"
              value={settings.operator_name_roi.top}
              onChange={(value) =>
                onChange({
                  ...settings,
                  operator_name_roi: { ...settings.operator_name_roi, top: value },
                })
              }
            />
            <RoiField
              label="右"
              value={settings.operator_name_roi.right}
              onChange={(value) =>
                onChange({
                  ...settings,
                  operator_name_roi: { ...settings.operator_name_roi, right: value },
                })
              }
            />
            <RoiField
              label="下"
              value={settings.operator_name_roi.bottom}
              onChange={(value) =>
                onChange({
                  ...settings,
                  operator_name_roi: { ...settings.operator_name_roi, bottom: value },
                })
              }
            />
          </div>
          <div className="preview-panel">
            <div className="preview-head">
              <span>实时预览</span>
              <span>
                {detailPreview
                  ? `${formatFrameStamp(detailPreview.elapsed_frames)} · ${detailPreview.width}x${detailPreview.height}`
                  : captureState === "window_missing"
                    ? "窗口消失"
                    : "等待截图"}
              </span>
            </div>
            <div className="preview-frame">
              {detailPreview ? (
                <img alt="名称截图预览" className="preview-image" src={detailPreview.data_url} />
              ) : (
                <div className="preview-empty">
                  {captureState === "window_missing"
                    ? "目标窗口当前不可见"
                    : "进入关卡并选中单位后，这里会显示实时截图"}
                </div>
              )}
            </div>
          </div>
        </section>
      </div>

    </div>
  );
}

function SettingField(props: { label: string; children: ReactNode }) {
  return (
    <label className="setting-field">
      <span>{props.label}</span>
      {props.children}
    </label>
  );
}

function RoiField(props: { label: string; value: number; onChange: (value: number) => void }) {
  return (
    <SettingField label={`${props.label} ${props.value.toFixed(3)}`}>
      <input
        className="range-input"
        max={0.95}
        min={0}
        onChange={(event) => props.onChange(Number(event.target.value))}
        step={0.001}
        type="range"
        value={props.value}
      />
    </SettingField>
  );
}

function EmptyState(props: { label: string }) {
  return <div className="empty-state">{props.label}</div>;
}

function IconButton(props: {
  icon: ReactNode;
  label: string;
  onClick: () => void;
  active?: boolean;
  disabled?: boolean;
  className?: string;
}) {
  return (
    <button
      className={`icon-button${props.active ? " active" : ""}${props.className ? ` ${props.className}` : ""}`}
      disabled={props.disabled}
      onClick={props.onClick}
      title={props.label}
      type="button"
    >
      {props.icon}
    </button>
  );
}

function ImportDialog(props: {
  title: string;
  content: string;
  onChangeTitle: (value: string) => void;
  onChangeContent: (value: string) => void;
  onCancel: () => void;
  onSubmit: () => void;
}) {
  return (
    <div className="modal-backdrop" onClick={props.onCancel} role="presentation">
      <div
        aria-modal="true"
        className="modal-panel"
        onClick={(event) => event.stopPropagation()}
        role="dialog"
      >
        <div className="pane-header modal-header">
          <span className="pane-label">导入记录</span>
        </div>
        <div className="modal-body">
          <SettingField label="名称">
            <input
              className="text-input"
              onChange={(event) => props.onChangeTitle(event.target.value)}
              value={props.title}
            />
          </SettingField>
          <SettingField label="内容">
            <textarea
              className="record-editor modal-editor"
              onChange={(event) => props.onChangeContent(event.target.value)}
              spellCheck={false}
              value={props.content}
              wrap="off"
            />
          </SettingField>
        </div>
        <div className="settings-footer">
          <button className="ghost-button" onClick={props.onCancel} type="button">
            取消
          </button>
          <button className="command-button" onClick={props.onSubmit} type="button">
            导入
          </button>
        </div>
      </div>
    </div>
  );
}

function calibrationPhaseLabel(phase: CalibrationPhase): string {
  switch (phase) {
    case "running":
      return "校准中";
    case "cancelling":
      return "取消中";
    case "succeeded":
      return "已完成";
    case "failed":
      return "失败";
    case "cancelled":
      return "已取消";
    default:
      return "未开始";
  }
}

function formatMeta(record: RecordSummary): string {
  const date = new Date(record.updated_at_ms);
  return `${date.toLocaleDateString("zh-CN")} ${date.toLocaleTimeString("zh-CN", {
    hour12: false,
  })}`;
}

function stringifyError(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }
  if (value instanceof Error) {
    return value.message;
  }
  return JSON.stringify(value);
}

function createOperation(
  index: number,
  time: string,
  code: string,
  operatorName: string,
  tile: TileCoord | null,
): UiOperationLine {
  const frames = parseFrameStamp(time);
  const tileLabel = tile ? `(${tile.x}, ${tile.y})` : "-";
  const lineText = tile
    ? `${time} ${code} ${operatorName} (${tile.x}, ${tile.y})`
    : `${time} ${code} ${operatorName}`;
  return {
    index,
    frames,
    time,
    code,
    operator_name: operatorName,
    tile,
    tile_label: tileLabel,
    line_text: lineText,
  };
}

function formatFrameStamp(totalFrames: number): string {
  const safeFrames = Math.max(0, Math.floor(totalFrames));
  const frames = safeFrames % 30;
  const totalSeconds = Math.floor(safeFrames / 30);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}:${frames
    .toString()
    .padStart(2, "0")}`;
}

function parseFrameStamp(time: string): number {
  const [minutes, seconds, frames] = time.split(":").map(Number);
  return (minutes * 60 + seconds) * 30 + frames;
}

function buildOperationsText(operations: UiOperationLine[]): string {
  if (operations.length === 0) {
    return "";
  }
  return `${operations.map((operation) => operation.line_text).join("\n")}\n`;
}

function parseOperationsText(rawContent: string): UiOperationLine[] {
  if (!rawContent.trim()) {
    return [];
  }
  return rawContent
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line, index) => {
      const match = line.match(
        /^(\d{2}:\d{2}:\d{2})\s+(DP|US|RT)(?:\s+(.+?))?(?:\s+\(([-]?\d+)\s*,\s*([-]?\d+)\))?$/,
      );
      if (!match) {
        return null;
      }
      const tile =
        match[4] && match[5]
          ? { x: Number(match[4]), y: Number(match[5]) }
          : null;
      return createOperation(index, match[1], match[2], (match[3] ?? "?").trim() || "?", tile);
    })
    .filter((operation): operation is UiOperationLine => Boolean(operation));
}

function scrollTextAreaToLine(textarea: HTMLTextAreaElement, lineIndex: number) {
  const safeIndex = Math.max(0, lineIndex);
  const lineHeight = measureTextAreaLineHeight(textarea);
  const top = Math.max(0, safeIndex * lineHeight - textarea.clientHeight * 0.42);
  textarea.scrollTop = top;
}

function measureTextAreaLineHeight(textarea: HTMLTextAreaElement): number {
  const computed = window.getComputedStyle(textarea);
  const lineHeight = Number.parseFloat(computed.lineHeight);
  if (Number.isFinite(lineHeight)) {
    return lineHeight;
  }
  const fontSize = Number.parseFloat(computed.fontSize);
  return Number.isFinite(fontSize) ? fontSize * 1.55 : 22;
}

function clampHistoryPaneHeight(nextHeight: number, availableHeight: number): number {
  const maxHeight = Math.max(MIN_EDITOR_PANE_HEIGHT_PX, availableHeight - MIN_EDITOR_PANE_HEIGHT_PX);
  return Math.max(MIN_EDITOR_PANE_HEIGHT_PX, Math.min(maxHeight, nextHeight));
}

async function playPromptBeep(audioContextRef: { current: AudioContext | null }) {
  if (typeof window === "undefined") {
    return;
  }
  const audioContextCtor =
    window.AudioContext ??
    (window as Window & typeof globalThis & { webkitAudioContext?: typeof AudioContext })
      .webkitAudioContext;
  if (!audioContextCtor) {
    return;
  }
  let context = audioContextRef.current;
  if (!context) {
    context = new audioContextCtor();
    audioContextRef.current = context;
  }
  if (context.state === "suspended") {
    try {
      await context.resume();
    } catch {
      return;
    }
  }
  const now = context.currentTime;
  const oscillator = context.createOscillator();
  const gain = context.createGain();
  oscillator.type = "sine";
  oscillator.frequency.setValueAtTime(880, now);
  gain.gain.setValueAtTime(0.0001, now);
  gain.gain.exponentialRampToValueAtTime(0.12, now + 0.01);
  gain.gain.exponentialRampToValueAtTime(0.0001, now + 0.16);
  oscillator.connect(gain);
  gain.connect(context.destination);
  oscillator.start(now);
  oscillator.stop(now + 0.18);
}

function formatBattleStateLabel(state: string): string {
  switch (state) {
    case "0.2x_running":
      return "0.2x";
    case "1x_running":
      return "1x";
    case "2x_running":
      return "2x";
    case "0.2x_paused":
    case "1x_paused":
    case "2x_paused":
      return "已暂停";
    case "deploying_operator":
      return "部署中";
    case "adjusting_operator_facing":
      return "调整朝向";
    case "battle_begin":
      return "开始作战";
    case "before_or_after_battle":
      return "战斗外";
    case "not_in_battle":
      return "未在关卡内";
    default:
      return "等待中";
  }
}

async function backendInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (tauriRuntime) {
    return invoke<T>(command, args);
  }
  return mockInvoke<T>(command, args);
}

async function listenStatus(callback: (status: StatusChanged<UiSnapshot>) => void): Promise<() => void> {
  if (tauriRuntime) {
    const unlisten = await listen<StatusChanged<UiSnapshot>>("app://status", (event) => callback(event.payload));
    return () => unlisten();
  }
  ensureMockTicker();
  const listener = (snapshot: UiSnapshot) =>
    callback({ revision: Date.now(), session_id: "mock-runtime", state: snapshot });
  mockListeners.add(listener);
  listener(mockSnapshot);
  return () => {
    mockListeners.delete(listener);
  };
}

async function listenTimeline(callback: (timeline: TimelineUpdate) => void): Promise<() => void> {
  if (!tauriRuntime) {
    return () => {};
  }
  const unlisten = await listen<TimelineUpdate>("app://timeline", (event) => callback(event.payload));
  return () => unlisten();
}

function emitMockSnapshot() {
  for (const listener of mockListeners) {
    listener(mockSnapshot);
  }
}

function ensureMockTicker() {
  if (mockTickerStarted) {
    return;
  }
  mockTickerStarted = true;
  window.setInterval(() => {
    if (!mockSnapshot.current_battle) {
      return;
    }
    const nextFrames =
      mockSnapshot.current_battle.elapsed_frames >= mockOperations[mockOperations.length - 1].frames
        ? 0
        : mockSnapshot.current_battle.elapsed_frames + 30;
    mockSnapshot = {
      ...mockSnapshot,
      current_battle: {
        ...mockSnapshot.current_battle,
        elapsed_frames: nextFrames,
        operations: mockOperations.filter((operation) => operation.frames <= nextFrames),
        raw_content: buildOperationsText(
          mockOperations.filter((operation) => operation.frames <= nextFrames),
        ),
      },
    };
    emitMockSnapshot();
  }, 1000);
}

async function mockInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  switch (command) {
    case "get_snapshot":
      return structuredClone(mockSnapshot) as T;
    case "get_bootstrap_snapshot":
      return {
        protocol_version: 1,
        revision: Date.now(),
        session_id: "mock-runtime",
        state: structuredClone(mockSnapshot),
      } as T;
    case "get_detail_preview":
      return null as T;
    case "load_record": {
      const recordId = String(args?.recordId ?? args?.record_id ?? "");
      const document = mockDocuments.get(recordId);
      if (!document) {
        throw new Error(`record '${recordId}' not found`);
      }
      return structuredClone(document) as T;
    }
    case "save_record": {
      const recordId = String(args?.recordId ?? args?.record_id ?? "");
      const document = mockDocuments.get(recordId);
      if (!document) {
        throw new Error(`record '${recordId}' not found`);
      }
      const title = String(args?.title ?? "");
      const rawContent = String(args?.rawContent ?? args?.raw_content ?? "");
      const operations = rawContent
        .trim()
        .split("\n")
        .filter(Boolean)
        .map((line, index) => {
          const match = line.match(/^(\d{2}:\d{2}:\d{2})\s+(DP|US|RT)\s+(.+?)(?:\s+\(([-\d]+),\s*([-\d]+)\))?$/);
          if (!match) {
            return null;
          }
          const tile =
            match[4] && match[5]
              ? { x: Number(match[4]), y: Number(match[5]) }
              : null;
          return createOperation(index, match[1], match[2], match[3], tile);
        })
        .filter((value): value is UiOperationLine => Boolean(value));
      const nextDocument: RecordDocument = {
        meta: {
          ...document.meta,
          title,
          updated_at_ms: Date.now(),
          operation_count: operations.length,
          duration_frames: operations[operations.length - 1]?.frames ?? 0,
        },
        raw_content: rawContent,
        operations,
      };
      mockDocuments.set(recordId, nextDocument);
      mockSnapshot = {
        ...mockSnapshot,
        records: mockSnapshot.records.map((record) =>
          record.id === recordId ? nextDocument.meta : record,
        ),
      };
      emitMockSnapshot();
      return structuredClone(mockSnapshot) as T;
    }
    case "import_record": {
      const title = String(args?.title ?? "").trim() || `导入 ${Date.now()}`;
      const rawContent = String(args?.rawContent ?? args?.raw_content ?? "");
      const operations = parseOperationsText(rawContent);
      const now = Date.now();
      const document: RecordDocument = {
        meta: {
          id: `mock-import-${now}`,
          title,
          stage_name: "墨染书阁",
          created_at_ms: now,
          updated_at_ms: now,
          deleted: false,
          operation_count: operations.length,
          duration_frames: operations[operations.length - 1]?.frames ?? 0,
        },
        raw_content: rawContent,
        operations,
      };
      mockDocuments.set(document.meta.id, document);
      mockSnapshot = {
        ...mockSnapshot,
        records: [document.meta, ...mockSnapshot.records],
      };
      emitMockSnapshot();
      return structuredClone({
        snapshot: mockSnapshot,
        document,
      }) as T;
    }
    case "save_current_battle_text": {
      const battleId = String(args?.battleId ?? args?.battle_id ?? "");
      const rawContent = String(args?.rawContent ?? args?.raw_content ?? "");
      const baseSnapshotText = String(
        args?.baseSnapshotText ?? args?.base_snapshot_text ?? "",
      );
      const battle = mockSnapshot.current_battle;
      if (!battle || battle.id !== battleId) {
        throw new Error(`current battle '${battleId}' not found`);
      }
      const nextText = battle.raw_content.startsWith(baseSnapshotText)
        ? `${rawContent}${battle.raw_content.slice(baseSnapshotText.length)}`
        : rawContent;
      mockSnapshot = {
        ...mockSnapshot,
        current_battle: {
          ...battle,
          raw_content: nextText,
        },
      };
      emitMockSnapshot();
      return structuredClone(mockSnapshot) as T;
    }
    case "set_record_deleted": {
      const recordId = String(args?.recordId ?? args?.record_id ?? "");
      mockSnapshot = {
        ...mockSnapshot,
        records: mockSnapshot.records.filter((record) => record.id !== recordId),
      };
      emitMockSnapshot();
      return structuredClone(mockSnapshot) as T;
    }
    case "set_window_pinned": {
      const pinned = Boolean(args?.pinned);
      mockSnapshot = {
        ...mockSnapshot,
        settings: { ...mockSnapshot.settings, always_on_top: pinned },
      };
      emitMockSnapshot();
      return structuredClone(mockSnapshot) as T;
    }
    case "save_app_settings": {
      const settings = structuredClone(args?.settings as AppSettings);
      mockSnapshot = {
        ...mockSnapshot,
        settings,
      };
      emitMockSnapshot();
      return structuredClone(mockSnapshot) as T;
    }
    case "refresh_setup":
      return structuredClone(mockSnapshot) as T;
    case "save_selected_target": {
      const fingerprint = String(args?.fingerprint ?? "mock-target-1");
      mockSnapshot = {
        ...mockSnapshot,
        setup: {
          ...mockSnapshot.setup,
          phase: "needs_calibration",
          message: "目标窗口已保存。下一步需要完成一次 calibration。",
          selected_target_fingerprint: fingerprint,
          calibration_path: "calibration/profile_preview.json",
        },
      };
      emitMockSnapshot();
      return structuredClone(mockSnapshot) as T;
    }
    case "start_calibration": {
      mockSnapshot = {
        ...mockSnapshot,
        calibration: {
          phase: "running",
          progress_percent: 42,
          message: "正在采样费用条，第 1/2 圈",
          session_dir: "logs/session-preview-calibration",
        },
      };
      emitMockSnapshot();
      return structuredClone(mockSnapshot) as T;
    }
    case "cancel_calibration": {
      mockSnapshot = {
        ...mockSnapshot,
        calibration: {
          ...mockSnapshot.calibration,
          phase: "cancelled",
          message: "校准已取消",
        },
      };
      emitMockSnapshot();
      return structuredClone(mockSnapshot) as T;
    }
    default:
      throw new Error(`mock command '${command}' is not implemented`);
  }
}
