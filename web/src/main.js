import Hls from "hls.js";
import { WhepPlayer } from "./whep.js";
import "../vendor/sarmg-design/reset.css";
import "../vendor/sarmg-design/accessibility.css";
import "./styles.css";

const state = {
  me: null,
  cameras: [],
  users: [],
  players: new Map(),
  gridPage: 0,
  gridSize: 9,
  drawerCamera: null,
  eventSource: null,
};

const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

async function api(path, options = {}) {
  const response = await fetch(path, {
    credentials: "same-origin",
    ...options,
    headers: {
      ...(options.body ? { "Content-Type": "application/json" } : {}),
      ...(options.headers || {}),
    },
  });
  if (response.status === 204) return null;
  const contentType = response.headers.get("content-type") || "";
  const payload = contentType.includes("json") ? await response.json() : null;
  if (!response.ok) {
    const error = new Error(payload?.error?.message || `请求失败 (${response.status})`);
    error.status = response.status;
    throw error;
  }
  return payload;
}

async function boot() {
  bindEvents();
  tickClock();
  setInterval(tickClock, 1000);
  try {
    state.me = await api("/api/me");
    await enterApp();
  } catch (_) {
    showLogin();
  }
}

function bindEvents() {
  $("#login-form").addEventListener("submit", login);
  $("#logout-button").addEventListener("click", logout);
  $$(".nav-item").forEach((button) => button.addEventListener("click", () => switchView(button.dataset.view)));
  $("#camera-search").addEventListener("input", () => { state.gridPage = 0; renderCameras(); });
  $("#page-prev").addEventListener("click", () => { state.gridPage = Math.max(0, state.gridPage - 1); renderCameras(); });
  $("#page-next").addEventListener("click", () => { state.gridPage += 1; renderCameras(); });
  $("#add-camera-button").addEventListener("click", () => openCameraDialog());
  $("#discover-button").addEventListener("click", discoverDevices);
  $("#camera-form").addEventListener("submit", saveCamera);
  $("#add-user-button").addEventListener("click", () => openUserDialog());
  $("#user-form").addEventListener("submit", saveUser);
  $$(".modal-close").forEach((button) => button.addEventListener("click", () => button.closest("dialog").close()));
  $("#drawer-close").addEventListener("click", closeDrawer);
  $("#drawer-scrim").addEventListener("click", closeDrawer);
  $("#record-search").addEventListener("click", searchRecordings);
  $("#refresh-events").addEventListener("click", loadEvents);
  $("#unacked-only").addEventListener("change", loadEvents);
  $("#refresh-audit").addEventListener("click", loadAudit);
  $$("[data-ptz]").forEach((button) => {
    const value = button.dataset.ptz;
    if (value === "stop") button.addEventListener("click", () => sendPtz("stop"));
    else {
      button.addEventListener("pointerdown", () => sendPtz("move", value));
      button.addEventListener("pointerup", () => sendPtz("stop"));
      button.addEventListener("pointerleave", () => sendPtz("stop"));
    }
  });
}

async function login(event) {
  event.preventDefault();
  $("#login-error").textContent = "";
  const data = Object.fromEntries(new FormData(event.currentTarget));
  try {
    state.me = await api("/api/auth/login", { method: "POST", body: JSON.stringify(data) });
    event.currentTarget.reset();
    await enterApp();
  } catch (error) {
    $("#login-error").textContent = error.message;
  }
}

async function logout() {
  closeAllPlayers();
  state.eventSource?.close();
  await api("/api/auth/logout", { method: "POST" }).catch(() => {});
  state.me = null;
  showLogin();
}

async function enterApp() {
  $("#login-screen").hidden = true;
  $("#app").hidden = false;
  $("#operator-email").textContent = state.me.email;
  $("#operator-role").textContent = roleLabel(state.me.role);
  document.body.dataset.role = state.me.role;
  await Promise.all([loadCameras(), loadEvents()]);
  connectEvents();
}

function showLogin() {
  $("#login-screen").hidden = false;
  $("#app").hidden = true;
  setTimeout(() => $("#login-form input")?.focus(), 50);
}

function switchView(view) {
  $$(".nav-item").forEach((item) => item.classList.toggle("active", item.dataset.view === view));
  $$("[data-view-panel]").forEach((panel) => panel.classList.toggle("active", panel.dataset.viewPanel === view));
  const labels = {
    cameras: ["LIVE OPERATIONS", "实时监控"], recordings: ["ARCHIVE SEARCH", "录像检索"],
    events: ["INCIDENT DESK", "事件中心"], system: ["SYSTEM CONTROL", "系统管理"],
  };
  $("#view-kicker").textContent = labels[view][0];
  $("#view-title").textContent = labels[view][1];
  if (view !== "cameras") closeGridPlayers();
  if (view === "cameras") renderCameras();
  if (view === "recordings") prepareRecordings();
  if (view === "events") loadEvents();
  if (view === "system") loadSystem();
}

async function loadCameras() {
  state.cameras = await api("/api/cameras");
  renderCameras();
  populateCameraSelect();
  updateSummary();
}

function filteredCameras() {
  const term = $("#camera-search").value.trim().toLowerCase();
  return state.cameras.filter((camera) => `${camera.name} ${camera.location}`.toLowerCase().includes(term));
}

function renderCameras() {
  closeGridPlayers();
  const filtered = filteredCameras();
  const pages = Math.max(1, Math.ceil(filtered.length / state.gridSize));
  state.gridPage = Math.min(state.gridPage, pages - 1);
  const cameras = filtered.slice(state.gridPage * state.gridSize, (state.gridPage + 1) * state.gridSize);
  const grid = $("#camera-grid");
  if (!cameras.length) {
    grid.innerHTML = `<div class="empty-state full-span">还没有匹配的摄像头。管理员可以从右上角添加设备。</div>`;
  } else {
    grid.innerHTML = cameras.map((camera, index) => `
      <article class="camera-card reveal" style="--delay:${index * 45}ms" data-camera-id="${camera.id}">
        <div class="video-shell"><video muted autoplay playsinline></video><div class="video-state">${camera.enabled ? "等待视频" : "设备已停用"}</div><div class="scanline"></div></div>
        <div class="camera-meta"><div><span class="status-dot ${camera.status}"></span><strong>${esc(camera.name)}</strong><small>${esc(camera.location || "未标注位置")}</small></div><span class="camera-status">${statusLabel(camera.status)}</span></div>
        <div class="camera-actions"><button data-action="detail">主码流</button>${state.me.role === "admin" ? `<button data-action="edit">配置</button><button data-action="delete" class="danger-link">删除</button>` : ""}</div>
      </article>`).join("");
    cameras.forEach((camera) => {
      const card = grid.querySelector(`[data-camera-id="${camera.id}"]`);
      card.querySelector('[data-action="detail"]').addEventListener("click", () => openDrawer(camera));
      card.querySelector('[data-action="edit"]')?.addEventListener("click", () => openCameraDialog(camera));
      card.querySelector('[data-action="delete"]')?.addEventListener("click", () => deleteCamera(camera));
      if (camera.enabled) startStream(camera, card.querySelector("video"), camera.has_sub_stream ? "sub" : "main", `grid:${camera.id}`);
    });
  }
  $("#page-label").textContent = `${state.gridPage + 1} / ${pages}`;
  $("#page-prev").disabled = state.gridPage === 0;
  $("#page-next").disabled = state.gridPage >= pages - 1;
}

async function startStream(camera, video, profile, key) {
  const stateLabel = video.parentElement.querySelector(".video-state");
  try {
    stateLabel.textContent = "正在连接";
    const ticket = await api(`/api/cameras/${camera.id}/stream-ticket?profile=${profile}`);
    const player = new WhepPlayer(video, ticket.whep_url, ticket.token);
    state.players.set(key, { close: () => player.close() });
    await player.start();
    stateLabel.classList.add("hidden");
  } catch (webrtcError) {
    try {
      const ticket = await api(`/api/cameras/${camera.id}/stream-ticket?profile=${profile}`);
      const hls = new Hls({
        lowLatencyMode: true,
        xhrSetup: (xhr) => xhr.setRequestHeader("Authorization", `Bearer ${ticket.token}`),
      });
      hls.loadSource(ticket.hls_url);
      hls.attachMedia(video);
      state.players.set(key, { close: () => { hls.destroy(); video.removeAttribute("src"); } });
      hls.on(Hls.Events.MANIFEST_PARSED, () => { video.play().catch(() => {}); stateLabel.classList.add("hidden"); });
      hls.on(Hls.Events.ERROR, (_, data) => { if (data.fatal) stateLabel.textContent = "视频暂不可用"; });
    } catch (_) {
      stateLabel.textContent = webrtcError.message || "视频暂不可用";
    }
  }
}

function closeGridPlayers() {
  [...state.players.entries()].filter(([key]) => key.startsWith("grid:")).forEach(([key, player]) => { player.close(); state.players.delete(key); });
}

function closeAllPlayers() {
  state.players.forEach((player) => player.close());
  state.players.clear();
}

function openCameraDialog(camera = null) {
  const form = $("#camera-form");
  form.reset();
  form.elements.id.value = camera?.id || "";
  form.elements.name.value = camera?.name || "";
  form.elements.location.value = camera?.location || "";
  form.elements.username.value = camera?.username || "";
  form.elements.enabled.checked = camera?.enabled ?? true;
  form.elements.record_enabled.checked = camera?.record_enabled ?? true;
  form.elements.main_stream_url.required = !camera;
  $("#camera-dialog-title").textContent = camera ? "编辑摄像头" : "添加摄像头";
  $("#secret-help").textContent = camera
    ? "流地址和密码留空会保持原值；凭据不会返回浏览器。"
    : "设备凭据会在服务端加密保存，不会返回浏览器。";
  $("#camera-dialog").showModal();
}

async function saveCamera(event) {
  event.preventDefault();
  const form = event.currentTarget;
  const values = Object.fromEntries(new FormData(form));
  const id = values.id;
  const payload = {
    name: values.name, location: values.location, username: values.username,
    enabled: form.elements.enabled.checked, record_enabled: form.elements.record_enabled.checked,
  };
  ["main_stream_url", "sub_stream_url", "onvif_url", "password"].forEach((key) => {
    if (values[key]) payload[key] = values[key];
  });
  try {
    const result = await api(id ? `/api/cameras/${id}` : "/api/cameras", { method: id ? "PUT" : "POST", body: JSON.stringify(payload) });
    $("#camera-dialog").close();
    toast(result.warning ? `设备已保存，但媒体同步失败：${result.warning}` : "摄像头已保存", result.warning ? "warning" : "success");
    await loadCameras();
  } catch (error) { toast(error.message, "error"); }
}

async function deleteCamera(camera) {
  if (!confirm(`确认删除“${camera.name}”？已有录像文件不会立即删除。`)) return;
  try { await api(`/api/cameras/${camera.id}`, { method: "DELETE" }); toast("摄像头已删除", "success"); await loadCameras(); }
  catch (error) { toast(error.message, "error"); }
}

async function discoverDevices() {
  const button = $("#discover-button");
  button.disabled = true; button.textContent = "正在扫描局域网…";
  try {
    const devices = await api("/api/discovery/onvif", { method: "POST" });
    if (!devices.length) toast("没有发现ONVIF设备；可改用手动添加", "warning");
    else {
      openCameraDialog();
      $("#camera-form").elements.onvif_url.value = devices[0].xaddrs[0] || "";
      toast(`发现 ${devices.length} 台设备，已填入第一台的ONVIF地址`, "success");
    }
  } catch (error) { toast(error.message, "error"); }
  finally { button.disabled = false; button.textContent = "发现ONVIF设备"; }
}

function openDrawer(camera) {
  state.drawerCamera = camera;
  $("#drawer-camera-name").textContent = camera.name;
  $("#drawer-camera-location").textContent = camera.location || "未标注位置";
  $("#camera-drawer").classList.add("open");
  $("#drawer-scrim").classList.add("visible");
  $("#camera-drawer").setAttribute("aria-hidden", "false");
  $("#detail-video-state").classList.remove("hidden");
  startStream(camera, $("#detail-video"), "main", "detail");
}

function closeDrawer() {
  state.players.get("detail")?.close(); state.players.delete("detail");
  $("#camera-drawer").classList.remove("open");
  $("#drawer-scrim").classList.remove("visible");
  $("#camera-drawer").setAttribute("aria-hidden", "true");
  state.drawerCamera = null;
}

async function sendPtz(action, vector = "0,0,0") {
  if (!state.drawerCamera) return;
  const [pan, tilt, zoom] = vector.split(",").map(Number);
  try { await api(`/api/cameras/${state.drawerCamera.id}/ptz`, { method: "POST", body: JSON.stringify({ action, pan, tilt, zoom }) }); }
  catch (error) { if (action !== "stop") toast(error.message, "error"); }
}

function populateCameraSelect() {
  const select = $("#record-camera");
  const previous = select.value;
  select.innerHTML = state.cameras.map((camera) => `<option value="${camera.id}">${esc(camera.name)}</option>`).join("");
  if (state.cameras.some((camera) => camera.id === previous)) select.value = previous;
}

function prepareRecordings() {
  populateCameraSelect();
  const now = new Date();
  const start = new Date(now.getTime() - 24 * 3600 * 1000);
  if (!$("#record-start").value) $("#record-start").value = localDateInput(start);
  if (!$("#record-end").value) $("#record-end").value = localDateInput(now);
}

async function searchRecordings() {
  const cameraId = $("#record-camera").value;
  if (!cameraId) return toast("请先添加摄像头", "warning");
  const params = new URLSearchParams({ camera_id: cameraId });
  if ($("#record-start").value) params.set("start", new Date($("#record-start").value).toISOString());
  if ($("#record-end").value) params.set("end", new Date($("#record-end").value).toISOString());
  const list = $("#record-list"); list.className = "record-list empty-state"; list.textContent = "正在检索录像…";
  try {
    const spans = await api(`/api/recordings?${params}`);
    $("#record-count").textContent = `${spans.length} 条`;
    if (!spans.length) { list.textContent = "所选时间范围内没有录像"; return; }
    list.className = "record-list";
    list.innerHTML = spans.map((span, index) => `<button class="record-item" data-index="${index}"><span>${formatDate(span.start)}</span><strong>${formatDuration(span.duration)}</strong><i>播放</i></button>`).join("");
    $$(".record-item", list).forEach((button) => button.addEventListener("click", () => playRecording(cameraId, spans[Number(button.dataset.index)])));
  } catch (error) { list.textContent = error.message; toast(error.message, "error"); }
}

function playRecording(cameraId, span) {
  const params = new URLSearchParams({ camera_id: cameraId, start: span.start, duration: String(span.duration), format: "mp4" });
  const video = $("#playback-video");
  video.src = `/api/recordings/play?${params}`;
  video.play().catch(() => {});
  $("#playback-caption").textContent = `${formatDate(span.start)} · ${formatDuration(span.duration)}`;
}

async function loadEvents() {
  if (!state.me) return;
  const query = $("#unacked-only").checked ? "?unacknowledged=true" : "";
  try {
    const events = await api(`/api/events${query}`);
    const cameraNames = new Map(state.cameras.map((camera) => [camera.id, camera.name]));
    $("#event-table").innerHTML = events.length ? events.map((event) => `
      <tr><td><span class="severity ${event.severity}">${severityLabel(event.severity)}</span></td><td><strong>${esc(event.message)}</strong><small>${esc(event.kind)}</small></td><td>${esc(cameraNames.get(event.camera_id) || "系统")}</td><td>${formatDate(event.created_at)}</td><td>${event.acknowledged_at ? "已确认" : (state.me.role !== "viewer" ? `<button class="text-button" data-ack="${event.id}">确认</button>` : "待确认")}</td></tr>`).join("") : `<tr><td colspan="5" class="empty-state">没有事件</td></tr>`;
    $$('[data-ack]').forEach((button) => button.addEventListener("click", async () => { await api(`/api/events/${button.dataset.ack}/ack`, { method: "POST" }); loadEvents(); }));
  } catch (error) { if (error.status !== 401) toast(error.message, "error"); }
}

function connectEvents() {
  state.eventSource?.close();
  state.eventSource = new EventSource("/api/events/stream");
  state.eventSource.addEventListener("system-event", (message) => {
    const event = JSON.parse(message.data);
    toast(event.message, event.severity === "critical" ? "error" : event.severity);
    loadCameras(); loadEvents();
  });
}

async function loadSystem() {
  try {
    const status = await api("/api/system/status");
    $("#system-cards").innerHTML = `
      <article><span>媒体服务</span><strong class="${status.media_service === "ok" ? "good" : "bad"}">${status.media_service === "ok" ? "运行正常" : "连接失败"}</strong><small>MediaMTX</small></article>
      <article><span>在线设备</span><strong>${status.cameras.online}<i> / ${status.cameras.total}</i></strong><small>当前主码流状态</small></article>
      <article><span>录像任务</span><strong>${status.cameras.recording}</strong><small>主码流持续录制</small></article>
      <article><span>控制面版本</span><strong>v${esc(status.version)}</strong><small>Rust / Axum</small></article>`;
    if (state.me.role === "admin") await Promise.all([loadUsers(), loadAudit()]);
  } catch (error) { toast(error.message, "error"); }
}

async function loadUsers() {
  state.users = await api("/api/users");
  $("#user-table").innerHTML = state.users.map((user) => `<tr><td><strong>${esc(user.email)}</strong></td><td>${roleLabel(user.role)}</td><td>${user.active ? "可用" : "已停用"}</td><td>${user.last_login_at ? formatDate(user.last_login_at) : "从未登录"}</td><td><button class="text-button" data-user-edit="${user.id}">编辑</button>${user.id !== state.me.id ? `<button class="text-button danger-link" data-user-delete="${user.id}">删除</button>` : ""}</td></tr>`).join("");
  $$('[data-user-edit]').forEach((button) => button.addEventListener("click", () => openUserDialog(state.users.find((user) => user.id === button.dataset.userEdit))));
  $$('[data-user-delete]').forEach((button) => button.addEventListener("click", () => deleteUser(state.users.find((user) => user.id === button.dataset.userDelete))));
}

function openUserDialog(user = null) {
  const form = $("#user-form"); form.reset();
  form.elements.id.value = user?.id || "";
  form.elements.email.value = user?.email || "";
  form.elements.email.disabled = Boolean(user);
  form.elements.role.value = user?.role || "viewer";
  form.elements.active.checked = user?.active ?? true;
  form.elements.password.required = !user;
  $("#user-dialog-title").textContent = user ? "编辑账号" : "添加账号";
  $("#user-dialog").showModal();
}

async function saveUser(event) {
  event.preventDefault();
  const form = event.currentTarget;
  const values = Object.fromEntries(new FormData(form));
  const id = values.id;
  const payload = { role: values.role, active: form.elements.active.checked };
  if (!id) payload.email = values.email;
  if (values.password) payload.password = values.password;
  try {
    await api(id ? `/api/users/${id}` : "/api/users", { method: id ? "PUT" : "POST", body: JSON.stringify(payload) });
    $("#user-dialog").close(); toast("账号已保存", "success"); await loadUsers();
  } catch (error) { toast(error.message, "error"); }
}

async function deleteUser(user) {
  if (!confirm(`确认删除账号 ${user.email}？`)) return;
  try { await api(`/api/users/${user.id}`, { method: "DELETE" }); toast("账号已删除", "success"); await loadUsers(); }
  catch (error) { toast(error.message, "error"); }
}

async function loadAudit() {
  try {
    const rows = await api("/api/audit?limit=30");
    $("#audit-list").innerHTML = rows.length ? rows.map((row) => `<div><span>${esc(row.action)}</span><small>${formatDate(row.created_at)}</small><code>${esc(row.entity_type)}${row.entity_id ? ` / ${row.entity_id.slice(0, 8)}` : ""}</code></div>`).join("") : `<div class="empty-state">暂无审计记录</div>`;
  } catch (error) { toast(error.message, "error"); }
}

function updateSummary() {
  const online = state.cameras.filter((camera) => camera.status === "online").length;
  $("#online-summary").textContent = `${online} / ${state.cameras.length} 在线`;
  $("#online-summary").classList.toggle("healthy", online > 0 || !state.cameras.length);
}

function tickClock() {
  $("#clock").textContent = new Intl.DateTimeFormat("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false }).format(new Date());
}

function toast(message, type = "info") {
  const item = document.createElement("div"); item.className = `toast ${type}`; item.textContent = message;
  $("#toast-stack").append(item);
  setTimeout(() => item.remove(), 5000);
}

function esc(value = "") { return String(value).replace(/[&<>'"]/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" })[char]); }
function roleLabel(role) { return ({ admin: "系统管理员", operator: "值守操作员", viewer: "只读观察员" })[role] || role; }
function statusLabel(status) { return ({ pending: "等待检测", online: "在线", offline: "离线", disabled: "已停用", error: "配置异常" })[status] || status; }
function severityLabel(level) { return ({ info: "信息", warning: "警告", critical: "严重" })[level] || level; }
function formatDate(value) { return new Intl.DateTimeFormat("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false }).format(new Date(value)); }
function formatDuration(seconds) { const total = Math.round(seconds); const h = Math.floor(total / 3600); const m = Math.floor((total % 3600) / 60); const s = total % 60; return `${h ? `${h}时` : ""}${m ? `${m}分` : ""}${s}秒`; }
function localDateInput(date) { const offset = date.getTimezoneOffset() * 60000; return new Date(date.getTime() - offset).toISOString().slice(0, 16); }

boot();
