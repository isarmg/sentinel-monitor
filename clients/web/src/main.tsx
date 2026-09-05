import { StrictMode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import type { FormEvent } from "react";
import { AdministratorsPanel, createSarmgAdminApplication, errorRequestId, useAdminApplication } from "@sarmg/admin-shell";
import { Button, Checkbox, ConfirmDangerDialog, Dialog, ErrorState, LoadingState, PageHeader, Select, Table, TextField } from "@sarmg/admin-ui";
import product from "../package.json";

import "@sarmg/design-tokens/tokens.css";
import "@sarmg/design-tokens/tokens.dark.css";
import "@sarmg/web-fonts/fonts.css";
import "@sarmg/admin-ui/styles.css";
import "@sarmg/design-tokens/reset.css";
import "@sarmg/design-tokens/accessibility.css";
import "./styles.css";

import {
  administratorApi,
  apiPath,
  isAuditRows,
  isCameraMutation,
  isCameras,
  isDiscoveredDevices,
  isMonitorEvents,
  isOperation,
  isRecordingSpans,
  isStreamTicket,
  isSystemStatus,
  isUndefined,
  request,
  type AuditRow,
  type Camera,
  type MonitorEvent,
  type RecordingSpan,
  type SystemStatus,
} from "./api";
import { WhepPlayer } from "./whep";

type View = "cameras" | "recordings" | "events" | "system";
type CameraDraft = {
  id: string;
  name: string;
  location: string;
  main_stream_url: string;
  sub_stream_url: string;
  onvif_url: string;
  username: string;
  password: string;
  enabled: boolean;
  record_enabled: boolean;
};

const emptyCamera = (): CameraDraft => ({
  id: "",
  name: "",
  location: "",
  main_stream_url: "",
  sub_stream_url: "",
  onvif_url: "",
  username: "",
  password: "",
  enabled: true,
  record_enabled: true,
});
function Console() {
  const { notify } = useAdminApplication();
  const [view, setView] = useState<View>(currentView);
  useEffect(() => { const changed = () => setView(currentView()); window.addEventListener("hashchange", changed); return () => window.removeEventListener("hashchange", changed); }, []);
  const toast = useCallback((message: string, _type = "info") => notify(message), [notify]);
  const [cameras, setCameras] = useState<Camera[]>([]);
  const [events, setEvents] = useState<MonitorEvent[]>([]);
  const [audit, setAudit] = useState<AuditRow[]>([]);
  const [status, setStatus] = useState<SystemStatus | null>(null);
  const [systemFailure, setSystemFailure] = useState<{ requestId?: string } | null>(null);
  const [search, setSearch] = useState("");
  const [page, setPage] = useState(0);
  const [unacknowledgedOnly, setUnacknowledgedOnly] = useState(false);
  const [cameraDraft, setCameraDraft] = useState<CameraDraft | null>(null);
  const [drawerCamera, setDrawerCamera] = useState<Camera | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<Camera | null>(null);
  const [deletePending, setDeletePending] = useState(false);
  const deleteBusy = useRef(false);
  const [deleteFailure, setDeleteFailure] = useState<{ requestId?: string } | null>(null);
  const loadCameras = useCallback(async () => {
    setCameras(await request("/cameras", isCameras));
  }, []);
  const loadEvents = useCallback(async () => {
    const suffix = unacknowledgedOnly ? "?unacknowledged=true" : "";
    setEvents(await request(`/events${suffix}`, isMonitorEvents));
  }, [unacknowledgedOnly]);
  const loadSystem = useCallback(async () => {
    setSystemFailure(null);
    try {
      const [nextStatus, nextAudit] = await Promise.all([
        request("/system/status", isSystemStatus),
        request("/audit?limit=30", isAuditRows),
      ]);
      setStatus(nextStatus); setAudit(nextAudit);
    } catch (error) {
      setStatus(null); setAudit([]); setSystemFailure({ requestId: errorRequestId(error) });
      throw error;
    }
  }, []);

  useEffect(() => { void Promise.all([loadCameras(), loadEvents()]).catch((error) => toast(errorText(error), "error")); }, [loadCameras, loadEvents, toast]);
  useEffect(() => { if (view === "system") void loadSystem().catch((error) => toast(errorText(error), "error")); }, [loadSystem, toast, view]);
  useEffect(() => {
    const source = new EventSource(apiPath("/events/stream"));
    source.addEventListener("system-event", (message) => {
      try {
        const payload: unknown = JSON.parse((message as MessageEvent<string>).data);
        if (typeof payload === "object" && payload !== null && "message" in payload) {
          toast(String(payload.message), "severity" in payload ? String(payload.severity) : "info");
        }
      } catch {
        toast("收到无法解析的事件通知", "warning");
      }
      void Promise.all([loadCameras(), loadEvents()]).catch((error) => toast(errorText(error), "error"));
    });
    return () => source.close();
  }, [loadCameras, loadEvents, toast]);

  const online = cameras.filter((camera) => camera.status === "online").length;
  const filtered = useMemo(() => {
    const term = search.trim().toLowerCase();
    return cameras.filter((camera) => `${camera.name} ${camera.location}`.toLowerCase().includes(term));
  }, [cameras, search]);
  const pages = Math.max(1, Math.ceil(filtered.length / 9));
  const visible = filtered.slice(Math.min(page, pages - 1) * 9, (Math.min(page, pages - 1) + 1) * 9);

  const saveCamera = async (draft: CameraDraft) => {
    const payload: Record<string, unknown> = {
      name: draft.name,
      location: draft.location,
      username: draft.username,
      enabled: draft.enabled,
      record_enabled: draft.record_enabled,
    };
    for (const key of ["main_stream_url", "sub_stream_url", "onvif_url", "password"] as const) {
      if (draft[key] !== "") payload[key] = draft[key];
    }
    const result = await request(draft.id === "" ? "/cameras" : `/cameras/${draft.id}`, isCameraMutation, {
      method: draft.id === "" ? "POST" : "PUT", body: JSON.stringify(payload),
    });
    setCameraDraft(null);
    toast(result.warning ?? "摄像头已保存，媒体配置正在后台应用", result.warning === null ? "success" : "warning");
    await loadCameras();
  };
  const deleteCamera = async (camera: Camera) => {
    await request(`/cameras/${camera.id}`, isOperation, { method: "DELETE" });
    toast("摄像头已删除", "success"); await loadCameras();
  };
  const discover = async () => {
    const devices = await request("/discovery/onvif", isDiscoveredDevices, { method: "POST" });
    if (devices.length === 0) return toast("没有发现ONVIF设备；可改用手动添加", "warning");
    setCameraDraft({ ...emptyCamera(), onvif_url: devices[0]?.xaddrs[0] ?? "" });
    toast(`发现 ${devices.length} 台设备，已填入第一台的ONVIF地址`, "success");
  };
  const acknowledge = async (id: string) => {
    await request(`/events/${id}/ack`, isUndefined, { method: "POST" });
    await loadEvents();
  };

  return <div className="sentinel-business">
    <PageHeader><div><p className="eyebrow">{viewKicker(view)}</p><h1>{viewTitle(view)}</h1></div><span>{online} / {cameras.length} 在线</span><Clock /></PageHeader>
    {view === "cameras" && <CameraView cameras={visible} search={search} setSearch={(value) => { setSearch(value); setPage(0); }} page={Math.min(page, pages - 1)} pages={pages} setPage={setPage} edit={(camera) => setCameraDraft(toCameraDraft(camera))} remove={(camera) => { setDeleteFailure(null); setDeleteTarget(camera); }} inspect={setDrawerCamera} add={() => setCameraDraft(emptyCamera())} discover={() => void discover().catch((error) => toast(errorText(error), "error"))} />}
    {view === "recordings" && <RecordingsView cameras={cameras} toast={toast} />}
    {view === "events" && <EventsView events={events} cameras={cameras} unacknowledgedOnly={unacknowledgedOnly} setUnacknowledgedOnly={setUnacknowledgedOnly} refresh={() => void loadEvents().catch((error) => toast(errorText(error), "error"))} acknowledge={(id) => void acknowledge(id).catch((error) => toast(errorText(error), "error"))} />}
    {view === "system" && <SystemView status={status} failure={systemFailure} audit={audit} refresh={() => void loadSystem().catch((error) => toast(errorText(error), "error"))} />}
    {cameraDraft !== null && <CameraEditor draft={cameraDraft} setDraft={setCameraDraft} save={saveCamera} />}
    {drawerCamera !== null && <CameraDrawer camera={drawerCamera} close={() => setDrawerCamera(null)} toast={toast} />}
    {deleteTarget && <ConfirmDangerDialog title={"删除摄像头“" + deleteTarget.name + "”？"} description="已有录像文件不会立即删除。" pending={deletePending} onClose={() => { if (!deleteBusy.current) setDeleteTarget(null); }} onConfirm={() => {
      if (deleteBusy.current) return;
      deleteBusy.current = true; setDeletePending(true); setDeleteFailure(null);
      void deleteCamera(deleteTarget).then(() => setDeleteTarget(null))
        .catch(error => setDeleteFailure({ requestId: errorRequestId(error) }))
        .finally(() => { deleteBusy.current = false; setDeletePending(false); });
    }}>{deleteFailure && <ErrorState requestId={deleteFailure.requestId}>删除未能完成，请刷新摄像头列表确认状态。</ErrorState>}</ConfirmDangerDialog>}
  </div>;
}

function CameraView({ cameras, search, setSearch, page, pages, setPage, edit, remove, inspect, add, discover }: {
  cameras: Camera[]; search: string; setSearch(value: string): void; page: number; pages: number; setPage(value: number): void;
  edit(camera: Camera): void; remove(camera: Camera): void; inspect(camera: Camera): void; add(): void; discover(): void;
}) {
  return <section className="view active"><div className="command-bar"><div className="search-wrap"><span>⌕</span><TextField type="search" aria-label="搜索摄像头" value={search} onChange={(event) => setSearch(event.target.value)} placeholder="搜索名称或位置" /></div><div className="command-actions"><Button className="button button-quiet" onClick={discover}>发现ONVIF设备</Button><Button className="button button-primary" onClick={add}>添加摄像头</Button></div></div>
    <div className="camera-grid">{cameras.length === 0 ? <div className="empty-state full-span">还没有匹配的摄像头。</div> : cameras.map((camera, index) => <article key={camera.id} className="camera-card reveal" style={{ animationDelay: `${index * 45}ms` }}><LiveVideo camera={camera} profile={camera.has_sub_stream ? "sub" : "main"} /><div className="camera-meta"><div><span className={`status-dot ${camera.status}`} /><strong>{camera.name}</strong><small>{camera.location || "未标注位置"}</small></div><span className="camera-status">{statusLabel(camera.status)}</span></div><div className="camera-actions"><Button onClick={() => inspect(camera)}>主码流</Button><Button onClick={() => edit(camera)}>配置</Button><Button className="danger-link" onClick={() => remove(camera)}>删除</Button></div></article>)}</div>
    <div className="pager"><Button className="text-button" disabled={page === 0} onClick={() => setPage(Math.max(0, page - 1))}>上一页</Button><span>{page + 1} / {pages}</span><Button className="text-button" disabled={page >= pages - 1} onClick={() => setPage(page + 1)}>下一页</Button></div></section>;
}

function LiveVideo({ camera, profile, controls = false }: { camera: Camera; profile: string; controls?: boolean }) {
  const video = useRef<HTMLVideoElement>(null);
  const [label, setLabel] = useState(camera.enabled ? "正在连接" : "设备已停用");
  useEffect(() => {
    const element = video.current;
    if (element === null || !camera.enabled) return;
    let closed = false;
    let close: (() => void) | undefined;
    void request(`/cameras/${camera.id}/stream-ticket?profile=${profile}`, isStreamTicket).then(async (ticket) => {
      if (closed) return;
      const whep = new WhepPlayer(element, ticket.whep_url, ticket.token); close = () => whep.close();
      try { await whep.start(); if (!closed) setLabel(""); }
      catch {
        whep.close();
        const retry = await request(`/cameras/${camera.id}/stream-ticket?profile=${profile}`, isStreamTicket);
        if (closed) return;
        const { default: Hls } = await import("hls.js");
        if (closed) return;
        const hls = new Hls({ lowLatencyMode: true, xhrSetup: (xhr) => xhr.setRequestHeader("Authorization", `Bearer ${retry.token}`) });
        hls.loadSource(retry.hls_url); hls.attachMedia(element); close = () => hls.destroy();
        hls.on(Hls.Events.MANIFEST_PARSED, () => { void element.play().catch(() => { if (!closed) setLabel("请点击播放"); }); if (!closed) setLabel(""); });
        hls.on(Hls.Events.ERROR, (_event, data) => { if (data.fatal) setLabel("视频暂不可用"); });
      }
    }).catch((error) => { if (!closed) setLabel(errorText(error)); });
    return () => { closed = true; close?.(); element.removeAttribute("src"); element.srcObject = null; };
  }, [camera.enabled, camera.id, profile]);
  return <div className={controls ? "detail-video" : "video-shell"}><video ref={video} muted autoPlay playsInline controls={controls} />{label !== "" && <div className="video-state">{label}</div>}{!controls && <div className="scanline" />}</div>;
}

function RecordingsView({ cameras, toast }: { cameras: Camera[]; toast(message: string, type?: string): void }) {
  const [cameraId, setCameraId] = useState(cameras[0]?.id ?? "");
  const [start, setStart] = useState(() => localDateInput(new Date(Date.now() - 86_400_000)));
  const [end, setEnd] = useState(() => localDateInput(new Date()));
  const [spans, setSpans] = useState<RecordingSpan[]>([]);
  const [playing, setPlaying] = useState<RecordingSpan | null>(null);
  useEffect(() => { if (cameraId === "" && cameras[0] !== undefined) setCameraId(cameras[0].id); }, [cameraId, cameras]);
  const search = async () => {
    if (cameraId === "") return toast("请先添加摄像头", "warning");
    const query = new URLSearchParams({ camera_id: cameraId, start: new Date(start).toISOString(), end: new Date(end).toISOString() });
    setSpans(await request(`/recordings?${query}`, isRecordingSpans));
  };
  const playback = playing === null ? "" : apiPath(`/recordings/play?${new URLSearchParams({ camera_id: cameraId, start: playing.start, duration: String(playing.duration), format: "mp4" })}`);
  return <section className="view active"><div className="filter-panel"><label>摄像头<Select value={cameraId} onChange={(event) => setCameraId(event.target.value)}>{cameras.map((camera) => <option key={camera.id} value={camera.id}>{camera.name}</option>)}</Select></label><label>开始时间<TextField type="datetime-local" value={start} onChange={(event) => setStart(event.target.value)} /></label><label>结束时间<TextField type="datetime-local" value={end} onChange={(event) => setEnd(event.target.value)} /></label><Button className="button button-primary" onClick={() => void search().catch((error) => toast(errorText(error), "error"))}>查询录像</Button></div><div className="recording-layout"><div><div className="section-heading"><h3>录像时间段</h3><span>{spans.length} 条</span></div><div className="record-list">{spans.length === 0 ? <div className="empty-state">所选范围内没有录像</div> : spans.map((span) => <Button key={`${span.start}-${span.duration}`} className="record-item" onClick={() => setPlaying(span)}><span>{formatDate(span.start)}</span><strong>{formatDuration(span.duration)}</strong><i>播放</i></Button>)}</div></div><div className="playback-stage"><video src={playback || undefined} controls playsInline autoPlay /><div>{playing === null ? "尚未选择录像" : `${formatDate(playing.start)} · ${formatDuration(playing.duration)}`}</div></div></div></section>;
}

function EventsView({ events, cameras, unacknowledgedOnly, setUnacknowledgedOnly, refresh, acknowledge }: { events: MonitorEvent[]; cameras: Camera[]; unacknowledgedOnly: boolean; setUnacknowledgedOnly(value: boolean): void; refresh(): void; acknowledge(id: string): void }) {
  const names = new Map(cameras.map((camera) => [camera.id, camera.name]));
  return <section className="view active"><div className="command-bar"><label className="toggle-line"><TextField type="checkbox" checked={unacknowledgedOnly} onChange={(event) => setUnacknowledgedOnly(event.target.checked)} />仅显示未确认事件</label><Button className="button button-quiet" onClick={refresh}>刷新</Button></div><Table aria-label="监控事件"><thead><tr><th>等级</th><th>事件</th><th>摄像头</th><th>时间</th><th>状态</th></tr></thead><tbody>{events.length === 0 ? <tr><td colSpan={5} className="empty-state">没有事件</td></tr> : events.map((event) => <tr key={event.id}><td><span className={`severity ${event.severity}`}>{severityLabel(event.severity)}</span></td><td><strong>{event.message}</strong><small>{event.kind}</small></td><td>{event.camera_id === null ? "系统" : names.get(event.camera_id) ?? "系统"}</td><td>{formatDate(event.created_at)}</td><td>{event.acknowledged_at === null ? <Button className="text-button" onClick={() => acknowledge(event.id)}>确认</Button> : "已确认"}</td></tr>)}</tbody></Table></section>;
}

function SystemView({ status, failure, audit, refresh }: { status: SystemStatus | null; failure: { requestId?: string } | null; audit: AuditRow[]; refresh(): void }) {
  return <section className="view active">
    {failure ? <ErrorState requestId={failure.requestId} onRetry={refresh}>系统状态与业务审计暂不可用。</ErrorState>
      : status === null ? <LoadingState>正在加载系统状态…</LoadingState> : <div className="system-cards">
      <article><span>媒体服务</span><strong>{status.media_service === "ok" ? "运行正常" : "连接失败"}</strong><small>MediaMTX</small></article>
      <article><span>在线设备</span><strong>{status.cameras.online} / {status.cameras.total}</strong><small>当前主码流状态</small></article>
      <article><span>录像任务</span><strong>{status.cameras.recording}</strong><small>主码流持续录制</small></article>
      <article><span>控制面版本</span><strong>v{status.version}</strong><small>Rust / Axum</small></article>
    </div>}
    <div className="management-block"><AdministratorsPanel /></div>
    <section className="management-block"><div className="section-heading"><h2>最近业务审计记录</h2><Button onClick={refresh}>刷新</Button></div>
      <div className="audit-list">{audit.length === 0 ? <div className="empty-state">暂无审计记录</div> : audit.map((row) => <div key={row.id}><span>{row.action}</span><small>{formatDate(row.created_at)}</small><code>{row.entity_type}{row.entity_id === null ? "" : " / " + row.entity_id.slice(0, 8)}</code></div>)}</div>
    </section>
  </section>;
}

function CameraEditor({ draft, setDraft, save }: { draft: CameraDraft; setDraft(value: CameraDraft | null): void; save(value: CameraDraft): Promise<void> }) {
  const [pending, setPending] = useState(false);
  const busy = useRef(false);
  const [failure, setFailure] = useState<{ requestId?: string } | null>(null);
  const field = (key: keyof CameraDraft) => (event: React.ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, [key]: event.target.type === "checkbox" ? event.target.checked : event.target.value });
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); if (busy.current) return;
    busy.current = true; setPending(true); setFailure(null);
    try { await save(draft); } catch (error) { setFailure({ requestId: errorRequestId(error) }); setDraft({ ...draft, password: "" }); }
    finally { busy.current = false; setPending(false); }
  }
  return <Dialog title={draft.id === "" ? "添加摄像头" : "编辑摄像头"} onClose={() => { if (!busy.current) setDraft(null); }}>
    <form className="sentinel-business" onSubmit={event => void submit(event)} aria-busy={pending}>
      {failure && <ErrorState requestId={failure.requestId}>设备配置未能保存，请检查输入并重试。</ErrorState>}
      <fieldset disabled={pending}><div className="form-grid">
        <label>名称<TextField value={draft.name} onChange={field("name")} required /></label>
        <label>位置<TextField value={draft.location} onChange={field("location")} /></label>
        <label className="wide">主码流 RTSP<TextField value={draft.main_stream_url} onChange={field("main_stream_url")} required={draft.id === ""} /></label>
        <label className="wide">子码流 RTSP<TextField value={draft.sub_stream_url} onChange={field("sub_stream_url")} /></label>
        <label className="wide">ONVIF设备服务地址<TextField value={draft.onvif_url} onChange={field("onvif_url")} /></label>
        <label>设备用户名<TextField value={draft.username} onChange={field("username")} autoComplete="off" /></label>
        <label>设备密码<TextField type="password" value={draft.password} onChange={field("password")} autoComplete="new-password" /></label>
      </div><p className="field-help">编辑时流地址和密码留空会保持原值；凭据不会返回浏览器。</p>
      <div className="check-row"><label><Checkbox checked={draft.enabled} onChange={field("enabled")} />启用设备</label><label><Checkbox checked={draft.record_enabled} onChange={field("record_enabled")} />录制主码流</label></div>
      <div className="sarmg-actions"><Button onClick={() => setDraft(null)}>取消</Button><Button type="submit">{pending ? "正在保存…" : "保存设备"}</Button></div></fieldset>
    </form>
  </Dialog>;
}

function CameraDrawer({ camera, close, toast }: { camera: Camera; close(): void; toast(message: string, type?: string): void }) {
  const moving = useRef(false);
  const busy = useRef(false);
  const tail = useRef<Promise<void>>(Promise.resolve());
  const send = useCallback((action: "move" | "stop", vector = "0,0,0") => {
    const [pan = 0, tilt = 0, zoom = 0] = vector.split(",").map(Number);
    return request(`/cameras/${camera.id}/ptz`, isUndefined, { method: "POST", body: JSON.stringify({ action, pan, tilt, zoom }) });
  }, [camera.id]);
  const stop = useCallback(() => {
    if (!moving.current) return;
    moving.current = false;
    tail.current = tail.current.then(() => send("stop"))
      .catch(error => toast(errorText(error), "error"))
      .finally(() => { busy.current = false; });
  }, [send, toast]);
  const start = (vector: string) => {
    if (busy.current) return;
    busy.current = true; moving.current = true;
    tail.current = send("move", vector).catch(error => { toast(errorText(error), "error"); });
  };
  useEffect(() => {
    const hidden = () => { if (document.hidden) stop(); };
    window.addEventListener("blur", stop); document.addEventListener("visibilitychange", hidden);
    return () => { stop(); window.removeEventListener("blur", stop); document.removeEventListener("visibilitychange", hidden); };
  }, [stop]);
  const movement = (vector: string, label: string, glyph: string) => <Button aria-label={label}
    onPointerDown={event => { if (event.button !== 0) return; event.currentTarget.setPointerCapture(event.pointerId); start(vector); }}
    onPointerUp={stop} onPointerCancel={stop} onLostPointerCapture={stop} onBlur={stop}
    onKeyDown={event => { if (event.key === " " || event.key === "Enter") { event.preventDefault(); if (!event.repeat) start(vector); } }}
    onKeyUp={event => { if (event.key === " " || event.key === "Enter") { event.preventDefault(); stop(); } }}>{glyph}</Button>;
  return <Dialog title={camera.name} description={camera.location || "未标注位置"} onClose={() => { stop(); close(); }}>
    <div className="sentinel-business"><LiveVideo camera={camera} profile="main" controls />
      <section className="ptz-panel" aria-label="云台控制"><h3>云台控制</h3><p>按住方向键或用空格、回车启动移动，松开即停止。窗口失焦也会发送停止。</p>
        <div className="ptz-grid"><span />{movement("0,0.55,0", "云台向上", "↑")}<span />
          {movement("-0.55,0,0", "云台向左", "←")}<Button aria-label="停止云台" onClick={() => {
            if (moving.current) stop();
            else if (!busy.current) { busy.current = true; tail.current = send("stop").catch(error => toast(errorText(error), "error")).finally(() => { busy.current = false; }); }
          }}>■</Button>{movement("0.55,0,0", "云台向右", "→")}<span />{movement("0,-0.55,0", "云台向下", "↓")}<span />
        </div><div className="zoom-row">{movement("0,0,-0.45", "云台拉远", "− 拉远")}{movement("0,0,0.45", "云台拉近", "＋ 拉近")}</div>
      </section>
    </div>
  </Dialog>;
}

function Clock() { const [now, setNow] = useState(new Date()); useEffect(() => { const timer = window.setInterval(() => setNow(new Date()), 1_000); return () => window.clearInterval(timer); }, []); return <time>{new Intl.DateTimeFormat("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false }).format(now)}</time>; }

const viewTitle = (view: View) => ({ cameras: "实时监控", recordings: "录像检索", events: "事件中心", system: "系统管理" })[view];
const viewKicker = (view: View) => ({ cameras: "LIVE OPERATIONS", recordings: "ARCHIVE SEARCH", events: "INCIDENT DESK", system: "SYSTEM CONTROL" })[view];
const statusLabel = (status: string) => ({ pending: "等待检测", online: "在线", offline: "离线", disabled: "已停用", error: "配置异常" } as Record<string, string>)[status] ?? status;
const severityLabel = (severity: string) => ({ info: "信息", warning: "警告", critical: "严重" } as Record<string, string>)[severity] ?? severity;
const formatDate = (value: string) => new Intl.DateTimeFormat("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false }).format(new Date(value));
const formatDuration = (seconds: number) => { const total = Math.round(seconds); const hours = Math.floor(total / 3600); const minutes = Math.floor((total % 3600) / 60); return `${hours > 0 ? `${hours}时` : ""}${minutes > 0 ? `${minutes}分` : ""}${total % 60}秒`; };
const localDateInput = (date: Date) => new Date(date.getTime() - date.getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
const errorText = (error: unknown) => { const id = errorRequestId(error); return id ? `请求失败，请重试。Request ID: ${id}` : "请求失败，请重试。"; };
const toCameraDraft = (camera: Camera): CameraDraft => ({ ...emptyCamera(), id: camera.id, name: camera.name, location: camera.location, username: camera.username ?? "", enabled: camera.enabled, record_enabled: camera.record_enabled });

function currentView(): View {
  const hash = window.location.hash.slice(1);
  return hash === "recordings" || hash === "events" || hash === "system" ? hash : "cameras";
}
const Root = createSarmgAdminApplication({
  product: { name: "哨界 · Sentinel Monitor", version: product.version }, client: administratorApi,
  navigation: (["cameras", "recordings", "events", "system"] as const).map(view => ({ label: viewTitle(view), href: "#" + view })),
  routes: <Console />,
});
const root = document.getElementById("root");
if (root === null) throw new Error("缺少React根节点");
createRoot(root).render(<StrictMode><Root /></StrictMode>);
