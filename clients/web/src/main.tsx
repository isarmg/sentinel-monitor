import { StrictMode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import type { FormEvent, PointerEvent as ReactPointerEvent } from "react";
import type { AdministratorSession } from "@sarmg/contracts";
import { useAdministratorSession } from "@sarmg/admin-web/react";
import { isApiClientError } from "@sarmg/http-client";
import Hls from "hls.js";

import "@sarmg/design-tokens/tokens.css";
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
  isManagedUserResponse,
  isManagedUsers,
  isMonitorEvents,
  isOperation,
  isRecordingSpans,
  isStreamTicket,
  isSystemStatus,
  isUndefined,
  request,
  type AuditRow,
  type Camera,
  type ManagedUser,
  type MonitorEvent,
  type RecordingSpan,
  type SystemStatus,
} from "./api";
import { WhepPlayer } from "./whep";

type View = "cameras" | "recordings" | "events" | "system";
type Toast = { id: number; message: string; type: string };
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
type UserDraft = { id: string; username: string; password: string; active: boolean };

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
const emptyUser = (): UserDraft => ({ id: "", username: "", password: "", active: true });

function Root() {
  const auth = useAdministratorSession(administratorApi);

  if (auth.phase === "loading") {
    return <main className="loading-screen" aria-busy="true">正在验证管理员会话…</main>;
  }
  if (auth.phase !== "authenticated") {
    return <LoginScreen busy={false} onLogin={auth.login} restoreError={auth.phase === "error"} />;
  }
  return <Console session={auth.session} onLogout={auth.logout} />;
}

function LoginScreen({
  busy,
  onLogin,
  restoreError,
}: {
  busy: boolean;
  onLogin(username: string, password: string): Promise<void>;
  restoreError: boolean;
}) {
  const [error, setError] = useState(restoreError ? "会话检查失败，请重新登录" : "");
  const [submitting, setSubmitting] = useState(busy);
  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = event.currentTarget;
    const data = new FormData(form);
    setError("");
    setSubmitting(true);
    try {
      await onLogin(String(data.get("username") ?? ""), String(data.get("password") ?? ""));
      form.reset();
    } catch (caught) {
      setError(errorText(caught));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <section className="login-screen">
      <div className="login-atmosphere" aria-hidden="true">
        <div className="radar-ring ring-one" /><div className="radar-ring ring-two" /><div className="radar-sweep" />
      </div>
      <form className="login-card" onSubmit={(event) => void submit(event)}>
        <p className="eyebrow">SENTINEL / RUST</p><h1>哨界</h1>
        <p className="login-copy">把分散的现场，收拢到一个清醒的视野。</p>
        <label>管理员用户名<input name="username" type="text" autoComplete="username" minLength={3} maxLength={64} required autoFocus /></label>
        <label>密码<input name="password" type="password" autoComplete="current-password" required /></label>
        <button className="button button-primary" type="submit" disabled={submitting}>{submitting ? "正在登录…" : "进入控制台"}</button>
        <p className="form-error" role="alert">{error}</p>
      </form>
    </section>
  );
}

function Console({ session, onLogout }: { session: AdministratorSession; onLogout(): Promise<void> }) {
  const [view, setView] = useState<View>("cameras");
  const [cameras, setCameras] = useState<Camera[]>([]);
  const [events, setEvents] = useState<MonitorEvent[]>([]);
  const [users, setUsers] = useState<ManagedUser[]>([]);
  const [audit, setAudit] = useState<AuditRow[]>([]);
  const [status, setStatus] = useState<SystemStatus | null>(null);
  const [search, setSearch] = useState("");
  const [page, setPage] = useState(0);
  const [unacknowledgedOnly, setUnacknowledgedOnly] = useState(false);
  const [cameraDraft, setCameraDraft] = useState<CameraDraft | null>(null);
  const [userDraft, setUserDraft] = useState<UserDraft | null>(null);
  const [drawerCamera, setDrawerCamera] = useState<Camera | null>(null);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const toastSequence = useRef(0);

  const toast = useCallback((message: string, type = "info") => {
    const id = ++toastSequence.current;
    setToasts((current) => [...current, { id, message, type }]);
    window.setTimeout(() => setToasts((current) => current.filter((item) => item.id !== id)), 5_000);
  }, []);

  const loadCameras = useCallback(async () => {
    setCameras(await request("/cameras", isCameras));
  }, []);
  const loadEvents = useCallback(async () => {
    const suffix = unacknowledgedOnly ? "?unacknowledged=true" : "";
    setEvents(await request(`/events${suffix}`, isMonitorEvents));
  }, [unacknowledgedOnly]);
  const loadSystem = useCallback(async () => {
    const [nextStatus, nextUsers, nextAudit] = await Promise.all([
      request("/system/status", isSystemStatus),
      request("/users", isManagedUsers),
      request("/audit?limit=30", isAuditRows),
    ]);
    setStatus(nextStatus); setUsers(nextUsers); setAudit(nextAudit);
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
      void Promise.all([loadCameras(), loadEvents()]);
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
    if (!window.confirm(`确认删除“${camera.name}”？已有录像文件不会立即删除。`)) return;
    await request(`/cameras/${camera.id}`, isOperation, { method: "DELETE" });
    toast("摄像头已删除", "success"); await loadCameras();
  };
  const discover = async () => {
    const devices = await request("/discovery/onvif", isDiscoveredDevices, { method: "POST" });
    if (devices.length === 0) return toast("没有发现ONVIF设备；可改用手动添加", "warning");
    setCameraDraft({ ...emptyCamera(), onvif_url: devices[0]?.xaddrs[0] ?? "" });
    toast(`发现 ${devices.length} 台设备，已填入第一台的ONVIF地址`, "success");
  };
  const saveUser = async (draft: UserDraft) => {
    const payload: Record<string, unknown> = draft.id === ""
      ? { username: draft.username, password: draft.password }
      : { active: draft.active };
    if (draft.id !== "" && draft.password !== "") payload.password = draft.password;
    await request(draft.id === "" ? "/users" : `/users/${draft.id}`, isManagedUserResponse, {
      method: draft.id === "" ? "POST" : "PUT", body: JSON.stringify(payload),
    });
    setUserDraft(null); toast("管理员账号已保存", "success"); await loadSystem();
  };
  const deleteUser = async (user: ManagedUser) => {
    if (!window.confirm(`确认删除管理员账号 ${user.username}？`)) return;
    await request(`/users/${user.id}`, isUndefined, { method: "DELETE" });
    toast("管理员账号已删除", "success"); await loadSystem();
  };
  const acknowledge = async (id: string) => {
    await request(`/events/${id}/ack`, isUndefined, { method: "POST" });
    await loadEvents();
  };

  return (
    <div className="app-shell">
      <aside className="side-rail">
        <button className="brand brand-button" onClick={() => setView("cameras")}><span className="brand-mark" /><span>哨界</span></button>
        <nav className="primary-nav" aria-label="主导航">
          {(["cameras", "recordings", "events", "system"] as const).map((item, index) => (
            <button key={item} className={`nav-item ${view === item ? "active" : ""}`} onClick={() => setView(item)}>
              <span>0{index + 1}</span>{viewTitle(item)}
            </button>
          ))}
        </nav>
        <div className="rail-footer">
          <div className="administrator-card"><small>当前管理员</small><strong>{session.username}</strong><span>系统管理员</span></div>
          <button className="text-button" onClick={() => void onLogout()}>退出登录</button>
        </div>
      </aside>
      <main className="workspace">
        <header className="topbar"><div><p className="eyebrow">{viewKicker(view)}</p><h2>{viewTitle(view)}</h2></div><div className="topbar-status"><span className={`status-chip ${online > 0 || cameras.length === 0 ? "healthy" : ""}`}>{online} / {cameras.length} 在线</span><Clock /></div></header>
        {view === "cameras" && <CameraView cameras={visible} search={search} setSearch={(value) => { setSearch(value); setPage(0); }} page={Math.min(page, pages - 1)} pages={pages} setPage={setPage} edit={(camera) => setCameraDraft(toCameraDraft(camera))} remove={(camera) => void deleteCamera(camera).catch((error) => toast(errorText(error), "error"))} inspect={setDrawerCamera} add={() => setCameraDraft(emptyCamera())} discover={() => void discover().catch((error) => toast(errorText(error), "error"))} />}
        {view === "recordings" && <RecordingsView cameras={cameras} toast={toast} />}
        {view === "events" && <EventsView events={events} cameras={cameras} unacknowledgedOnly={unacknowledgedOnly} setUnacknowledgedOnly={setUnacknowledgedOnly} refresh={() => void loadEvents().catch((error) => toast(errorText(error), "error"))} acknowledge={(id) => void acknowledge(id).catch((error) => toast(errorText(error), "error"))} />}
        {view === "system" && <SystemView status={status} users={users} audit={audit} currentId={session.user_id} add={() => setUserDraft(emptyUser())} edit={(user) => setUserDraft({ id: user.id, username: user.username, password: "", active: user.active })} remove={(user) => void deleteUser(user).catch((error) => toast(errorText(error), "error"))} refresh={() => void loadSystem().catch((error) => toast(errorText(error), "error"))} />}
      </main>
      {cameraDraft !== null && <CameraEditor draft={cameraDraft} setDraft={setCameraDraft} save={(draft) => void saveCamera(draft).catch((error) => toast(errorText(error), "error"))} />}
      {userDraft !== null && <UserEditor draft={userDraft} setDraft={setUserDraft} save={(draft) => void saveUser(draft).catch((error) => toast(errorText(error), "error"))} />}
      {drawerCamera !== null && <CameraDrawer camera={drawerCamera} close={() => setDrawerCamera(null)} toast={toast} />}
      <div className="toast-stack" aria-live="polite">{toasts.map((item) => <div key={item.id} className={`toast ${item.type}`}>{item.message}</div>)}</div>
    </div>
  );
}

function CameraView({ cameras, search, setSearch, page, pages, setPage, edit, remove, inspect, add, discover }: {
  cameras: Camera[]; search: string; setSearch(value: string): void; page: number; pages: number; setPage(value: number): void;
  edit(camera: Camera): void; remove(camera: Camera): void; inspect(camera: Camera): void; add(): void; discover(): void;
}) {
  return <section className="view active"><div className="command-bar"><div className="search-wrap"><span>⌕</span><input type="search" value={search} onChange={(event) => setSearch(event.target.value)} placeholder="搜索名称或位置" /></div><div className="command-actions"><button className="button button-quiet" onClick={discover}>发现ONVIF设备</button><button className="button button-primary" onClick={add}>添加摄像头</button></div></div>
    <div className="camera-grid">{cameras.length === 0 ? <div className="empty-state full-span">还没有匹配的摄像头。</div> : cameras.map((camera, index) => <article key={camera.id} className="camera-card reveal" style={{ animationDelay: `${index * 45}ms` }}><LiveVideo camera={camera} profile={camera.has_sub_stream ? "sub" : "main"} /><div className="camera-meta"><div><span className={`status-dot ${camera.status}`} /><strong>{camera.name}</strong><small>{camera.location || "未标注位置"}</small></div><span className="camera-status">{statusLabel(camera.status)}</span></div><div className="camera-actions"><button onClick={() => inspect(camera)}>主码流</button><button onClick={() => edit(camera)}>配置</button><button className="danger-link" onClick={() => remove(camera)}>删除</button></div></article>)}</div>
    <div className="pager"><button className="text-button" disabled={page === 0} onClick={() => setPage(Math.max(0, page - 1))}>上一页</button><span>{page + 1} / {pages}</span><button className="text-button" disabled={page >= pages - 1} onClick={() => setPage(page + 1)}>下一页</button></div></section>;
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
      const whep = new WhepPlayer(element, ticket.whep_url, ticket.token); close = () => whep.close();
      try { await whep.start(); if (!closed) setLabel(""); }
      catch {
        whep.close();
        const retry = await request(`/cameras/${camera.id}/stream-ticket?profile=${profile}`, isStreamTicket);
        if (closed) return;
        const hls = new Hls({ lowLatencyMode: true, xhrSetup: (xhr) => xhr.setRequestHeader("Authorization", `Bearer ${retry.token}`) });
        hls.loadSource(retry.hls_url); hls.attachMedia(element); close = () => hls.destroy();
        hls.on(Hls.Events.MANIFEST_PARSED, () => { void element.play(); setLabel(""); });
        hls.on(Hls.Events.ERROR, (_event, data) => { if (data.fatal) setLabel("视频暂不可用"); });
      }
    }).catch((error) => setLabel(errorText(error)));
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
  return <section className="view active"><div className="filter-panel"><label>摄像头<select value={cameraId} onChange={(event) => setCameraId(event.target.value)}>{cameras.map((camera) => <option key={camera.id} value={camera.id}>{camera.name}</option>)}</select></label><label>开始时间<input type="datetime-local" value={start} onChange={(event) => setStart(event.target.value)} /></label><label>结束时间<input type="datetime-local" value={end} onChange={(event) => setEnd(event.target.value)} /></label><button className="button button-primary" onClick={() => void search().catch((error) => toast(errorText(error), "error"))}>查询录像</button></div><div className="recording-layout"><div><div className="section-heading"><h3>录像时间段</h3><span>{spans.length} 条</span></div><div className="record-list">{spans.length === 0 ? <div className="empty-state">所选范围内没有录像</div> : spans.map((span) => <button key={`${span.start}-${span.duration}`} className="record-item" onClick={() => setPlaying(span)}><span>{formatDate(span.start)}</span><strong>{formatDuration(span.duration)}</strong><i>播放</i></button>)}</div></div><div className="playback-stage"><video src={playback || undefined} controls playsInline autoPlay /><div>{playing === null ? "尚未选择录像" : `${formatDate(playing.start)} · ${formatDuration(playing.duration)}`}</div></div></div></section>;
}

function EventsView({ events, cameras, unacknowledgedOnly, setUnacknowledgedOnly, refresh, acknowledge }: { events: MonitorEvent[]; cameras: Camera[]; unacknowledgedOnly: boolean; setUnacknowledgedOnly(value: boolean): void; refresh(): void; acknowledge(id: string): void }) {
  const names = new Map(cameras.map((camera) => [camera.id, camera.name]));
  return <section className="view active"><div className="command-bar"><label className="toggle-line"><input type="checkbox" checked={unacknowledgedOnly} onChange={(event) => setUnacknowledgedOnly(event.target.checked)} />仅显示未确认事件</label><button className="button button-quiet" onClick={refresh}>刷新</button></div><div className="table-frame"><table><thead><tr><th>等级</th><th>事件</th><th>摄像头</th><th>时间</th><th>状态</th></tr></thead><tbody>{events.length === 0 ? <tr><td colSpan={5} className="empty-state">没有事件</td></tr> : events.map((event) => <tr key={event.id}><td><span className={`severity ${event.severity}`}>{severityLabel(event.severity)}</span></td><td><strong>{event.message}</strong><small>{event.kind}</small></td><td>{event.camera_id === null ? "系统" : names.get(event.camera_id) ?? "系统"}</td><td>{formatDate(event.created_at)}</td><td>{event.acknowledged_at === null ? <button className="text-button" onClick={() => acknowledge(event.id)}>确认</button> : "已确认"}</td></tr>)}</tbody></table></div></section>;
}

function SystemView({ status, users, audit, currentId, add, edit, remove, refresh }: { status: SystemStatus | null; users: ManagedUser[]; audit: AuditRow[]; currentId: string; add(): void; edit(user: ManagedUser): void; remove(user: ManagedUser): void; refresh(): void }) {
  return <section className="view active"><div className="system-cards"><article><span>媒体服务</span><strong className={status?.media_service === "ok" ? "good" : "bad"}>{status?.media_service === "ok" ? "运行正常" : "连接失败"}</strong><small>MediaMTX</small></article><article><span>在线设备</span><strong>{status?.cameras.online ?? 0}<i> / {status?.cameras.total ?? 0}</i></strong><small>当前主码流状态</small></article><article><span>录像任务</span><strong>{status?.cameras.recording ?? 0}</strong><small>主码流持续录制</small></article><article><span>控制面版本</span><strong>v{status?.version ?? "-"}</strong><small>Rust / Axum</small></article></div><section className="management-block"><div className="section-heading"><h3>管理员账号</h3><button className="button button-primary" onClick={add}>添加管理员</button></div><div className="table-frame"><table><thead><tr><th>用户名</th><th>身份</th><th>状态</th><th>最近登录</th><th /></tr></thead><tbody>{users.map((user) => <tr key={user.id}><td><strong>{user.username}</strong></td><td>系统管理员</td><td>{user.active ? "可用" : "已停用"}</td><td>{user.last_login_at === null ? "从未登录" : formatDate(user.last_login_at)}</td><td><button className="text-button" onClick={() => edit(user)}>编辑</button>{user.id !== currentId && <button className="text-button danger-link" onClick={() => remove(user)}>删除</button>}</td></tr>)}</tbody></table></div></section><section className="management-block"><div className="section-heading"><h3>最近审计记录</h3><button className="text-button" onClick={refresh}>刷新</button></div><div className="audit-list">{audit.length === 0 ? <div className="empty-state">暂无审计记录</div> : audit.map((row) => <div key={row.id}><span>{row.action}</span><small>{formatDate(row.created_at)}</small><code>{row.entity_type}{row.entity_id === null ? "" : ` / ${row.entity_id.slice(0, 8)}`}</code></div>)}</div></section></section>;
}

function CameraEditor({ draft, setDraft, save }: { draft: CameraDraft; setDraft(value: CameraDraft | null): void; save(value: CameraDraft): void }) {
  const field = (key: keyof CameraDraft) => (event: React.ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, [key]: event.target.type === "checkbox" ? event.target.checked : event.target.value });
  return <div className="modal-layer" role="presentation"><section className="modal" role="dialog" aria-modal="true"><form onSubmit={(event) => { event.preventDefault(); save(draft); }}><div className="modal-heading"><div><p className="eyebrow">DEVICE CONFIG</p><h3>{draft.id === "" ? "添加摄像头" : "编辑摄像头"}</h3></div><button type="button" className="icon-button" onClick={() => setDraft(null)}>×</button></div><div className="form-grid"><label>名称<input value={draft.name} onChange={field("name")} required /></label><label>位置<input value={draft.location} onChange={field("location")} /></label><label className="wide">主码流 RTSP<input value={draft.main_stream_url} onChange={field("main_stream_url")} required={draft.id === ""} /></label><label className="wide">子码流 RTSP<input value={draft.sub_stream_url} onChange={field("sub_stream_url")} /></label><label className="wide">ONVIF设备服务地址<input value={draft.onvif_url} onChange={field("onvif_url")} /></label><label>设备用户名<input value={draft.username} onChange={field("username")} /></label><label>设备密码<input type="password" value={draft.password} onChange={field("password")} /></label></div><p className="field-help">编辑时流地址和密码留空会保持原值；凭据不会返回浏览器。</p><div className="check-row"><label><input type="checkbox" checked={draft.enabled} onChange={field("enabled")} />启用设备</label><label><input type="checkbox" checked={draft.record_enabled} onChange={field("record_enabled")} />录制主码流</label></div><div className="modal-actions"><button type="button" className="button button-quiet" onClick={() => setDraft(null)}>取消</button><button className="button button-primary" type="submit">保存设备</button></div></form></section></div>;
}

function UserEditor({ draft, setDraft, save }: { draft: UserDraft; setDraft(value: UserDraft | null): void; save(value: UserDraft): void }) {
  return <div className="modal-layer" role="presentation"><section className="modal modal-small" role="dialog" aria-modal="true"><form onSubmit={(event) => { event.preventDefault(); save(draft); }}><div className="modal-heading"><div><p className="eyebrow">ADMINISTRATOR</p><h3>{draft.id === "" ? "添加管理员" : "编辑管理员"}</h3></div><button type="button" className="icon-button" onClick={() => setDraft(null)}>×</button></div><label>用户名<input type="text" minLength={3} maxLength={64} value={draft.username} disabled={draft.id !== ""} onChange={(event) => setDraft({ ...draft, username: event.target.value })} required /></label><label>密码<input type="password" minLength={12} value={draft.password} required={draft.id === ""} onChange={(event) => setDraft({ ...draft, password: event.target.value })} /></label><label className="toggle-line"><input type="checkbox" checked={draft.active} onChange={(event) => setDraft({ ...draft, active: event.target.checked })} />账号可用</label><div className="modal-actions"><button type="button" className="button button-quiet" onClick={() => setDraft(null)}>取消</button><button className="button button-primary" type="submit">保存管理员</button></div></form></section></div>;
}

function CameraDrawer({ camera, close, toast }: { camera: Camera; close(): void; toast(message: string, type?: string): void }) {
  const sendPtz = async (action: "move" | "stop", vector = "0,0,0") => {
    const [pan = 0, tilt = 0, zoom = 0] = vector.split(",").map(Number);
    await request(`/cameras/${camera.id}/ptz`, isUndefined, { method: "POST", body: JSON.stringify({ action, pan, tilt, zoom }) });
  };
  const move = (event: ReactPointerEvent<HTMLButtonElement>, vector: string) => { event.currentTarget.setPointerCapture(event.pointerId); void sendPtz("move", vector).catch((error) => toast(errorText(error), "error")); };
  return <><aside className="camera-drawer open" aria-hidden="false"><div className="drawer-header"><div><p className="eyebrow">PRIMARY STREAM</p><h3>{camera.name}</h3><span>{camera.location || "未标注位置"}</span></div><button className="icon-button" onClick={close}>×</button></div><LiveVideo camera={camera} profile="main" controls /><div className="ptz-panel"><div><p className="eyebrow">ONVIF PTZ</p><h4>云台控制</h4></div><div className="ptz-grid"><span /><button onPointerDown={(event) => move(event, "0,0.55,0")} onPointerUp={() => void sendPtz("stop")}>↑</button><span /><button onPointerDown={(event) => move(event, "-0.55,0,0")} onPointerUp={() => void sendPtz("stop")}>←</button><button onClick={() => void sendPtz("stop")}>■</button><button onPointerDown={(event) => move(event, "0.55,0,0")} onPointerUp={() => void sendPtz("stop")}>→</button><span /><button onPointerDown={(event) => move(event, "0,-0.55,0")} onPointerUp={() => void sendPtz("stop")}>↓</button><span /></div><div className="zoom-row"><button onPointerDown={(event) => move(event, "0,0,-0.45")} onPointerUp={() => void sendPtz("stop")}>− 拉远</button><button onPointerDown={(event) => move(event, "0,0,0.45")} onPointerUp={() => void sendPtz("stop")}>＋ 拉近</button></div></div></aside><button className="drawer-scrim visible" aria-label="关闭摄像头详情" onClick={close} /></>;
}

function Clock() { const [now, setNow] = useState(new Date()); useEffect(() => { const timer = window.setInterval(() => setNow(new Date()), 1_000); return () => window.clearInterval(timer); }, []); return <time>{new Intl.DateTimeFormat("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false }).format(now)}</time>; }

const viewTitle = (view: View) => ({ cameras: "实时监控", recordings: "录像检索", events: "事件中心", system: "系统管理" })[view];
const viewKicker = (view: View) => ({ cameras: "LIVE OPERATIONS", recordings: "ARCHIVE SEARCH", events: "INCIDENT DESK", system: "SYSTEM CONTROL" })[view];
const statusLabel = (status: string) => ({ pending: "等待检测", online: "在线", offline: "离线", disabled: "已停用", error: "配置异常" } as Record<string, string>)[status] ?? status;
const severityLabel = (severity: string) => ({ info: "信息", warning: "警告", critical: "严重" } as Record<string, string>)[severity] ?? severity;
const formatDate = (value: string) => new Intl.DateTimeFormat("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false }).format(new Date(value));
const formatDuration = (seconds: number) => { const total = Math.round(seconds); const hours = Math.floor(total / 3600); const minutes = Math.floor((total % 3600) / 60); return `${hours > 0 ? `${hours}时` : ""}${minutes > 0 ? `${minutes}分` : ""}${total % 60}秒`; };
const localDateInput = (date: Date) => new Date(date.getTime() - date.getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
const errorText = (error: unknown) => isApiClientError(error)
  ? error.message
  : error instanceof Error ? error.message : "请求失败";
const toCameraDraft = (camera: Camera): CameraDraft => ({ ...emptyCamera(), id: camera.id, name: camera.name, location: camera.location, username: camera.username ?? "", enabled: camera.enabled, record_enabled: camera.record_enabled });

const root = document.getElementById("root");
if (root === null) throw new Error("缺少React根节点");
createRoot(root).render(<StrictMode><Root /></StrictMode>);
