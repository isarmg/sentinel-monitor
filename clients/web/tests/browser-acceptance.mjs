import assert from "node:assert/strict";
import { chromium, firefox, expect } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
import { preview } from "vite";

const time = "2026-09-04T00:00:00Z";
const administratorId = "A".repeat(43);
const session = { authenticated: true, user_id: administratorId, username: "admin", role: "admin", csrf_token: "A".repeat(43) };
const camera = { id: "018f1f4b-7a5d-7b5f-8d31-123456789abc", name: "验收摄像头", location: "测试现场", has_sub_stream: false, onvif_configured: true, username: null, enabled: false, record_enabled: false, status: "disabled", last_seen_at: null, created_at: time, updated_at: time };
const server = await preview({ preview: { host: "127.0.0.1", port: 0, strictPort: true } });
const address = server.httpServer.address();
assert.ok(address && typeof address === "object");
try {
  for (const engine of [chromium, firefox]) {
    const browser = await engine.launch();
    try {
      const context = await browser.newContext({ viewport: { width: 360, height: 740 } });
      const page = await context.newPage();
      const errors = [], paths = [], ptz = [];
      let cameraDeleted = false, acknowledged = false, adminCreated = false, failStatus = false;
      page.on("pageerror", error => errors.push(error.message));
      await page.route("**/api/v2/**", async route => {
        const request = route.request(), url = new URL(request.url()), path = url.pathname;
        paths.push(path);
        if (path.endsWith("/auth/session")) return route.fulfill({ json: session });
        if (path.endsWith("/events/stream")) return route.fulfill({ status: 200, contentType: "text/event-stream", body: ": acceptance\n\n" });
        if (request.method() !== "GET") assert.equal(request.headers()["x-csrf-token"], session.csrf_token);
        if (path.endsWith("/ptz")) { ptz.push(request.postDataJSON().action); return route.fulfill({ status: 204 }); }
        if (path.endsWith("/cameras") && request.method() === "GET") return route.fulfill({ json: cameraDeleted ? [] : [camera] });
        if (path.endsWith(`/cameras/${camera.id}`) && request.method() === "DELETE") {
          cameraDeleted = true;
          return route.fulfill({ json: { id: "operation-1", camera_id: camera.id, generation: 1, kind: "delete", state: "Queued", reason: "administrator", requested_by: administratorId, attempt: 0, max_attempts: 1, created_at: time, started_at: null, finished_at: null, retry_at: null, error_code: null, error_message: null } });
        }
        if (path.endsWith("/events/event-1/ack")) { acknowledged = true; return route.fulfill({ status: 204 }); }
        if (path.endsWith("/events")) return route.fulfill({ json: [{ id: "event-1", camera_id: camera.id, kind: "camera.status", severity: "info", message: "验收事件", acknowledged_at: acknowledged ? time : null, created_at: time }] });
        if (path.endsWith("/system/status")) {
          if (failStatus) { failStatus = false; return route.fulfill({ status: 500, json: { code: "platform.internal", message: "SECRET database path", retryable: false, request_id: "system-failure-123" } }); }
          return route.fulfill({ json: { service: "sentinel-monitor", version: "0.2.0", database: "ok", media_service: "ok", cameras: { total: 1, online: 0, recording: 0 }, server_time: time } });
        }
        if (path.endsWith("/audit")) return route.fulfill({ json: [{ id: "audit-1", action: "camera.updated", entity_type: "camera", entity_id: camera.id, created_at: time }] });
        if (path.endsWith("/platform/administrators")) {
          if (request.method() === "POST") { assert.deepEqual(request.postDataJSON(), { username: "secondary", password: "replacement password" }); adminCreated = true; return route.fulfill({ status: 204 }); }
          const record = { administrator_id: administratorId, username: "admin", active: true, created_at_micros: 1, updated_at_micros: 2, last_login_at_micros: null };
          return route.fulfill({ json: adminCreated ? [record, { ...record, administrator_id: "B".repeat(43), username: "secondary" }] : [record] });
        }
        if (path.endsWith("/recordings")) { assert.equal(url.searchParams.get("camera_id"), camera.id); return route.fulfill({ json: [{ start: time, duration: 60 }] }); }
        throw new Error(`Unexpected API request ${request.method()} ${path}`);
      });
      await page.goto(`http://127.0.0.1:${address.port}/#cameras`);
      await expect(page.getByText(camera.name, { exact: true })).toBeVisible();
      await page.getByRole("button", { name: "添加摄像头", exact: true }).click();
      const editor = page.getByRole("dialog", { name: "添加摄像头", exact: true });
      await expect(editor).toBeVisible();
      for (let i = 0; i < 12; i++) { await page.keyboard.press("Tab"); assert.ok(await editor.evaluate(element => element.contains(document.activeElement))); }
      await page.keyboard.press("Escape");
      await expect(page.getByRole("button", { name: "添加摄像头", exact: true })).toBeFocused();
      await page.getByRole("button", { name: "主码流", exact: true }).click();
      const movement = page.getByRole("button", { name: "云台向上", exact: true });
      await movement.focus(); await page.keyboard.down("Space"); await page.keyboard.up("Space");
      await expect.poll(() => ptz.slice()).toEqual(["move", "stop"]);
      await movement.focus(); await page.keyboard.down("Enter");
      await page.evaluate(() => window.dispatchEvent(new Event("blur")));
      await page.keyboard.up("Enter");
      await expect.poll(() => ptz.slice()).toEqual(["move", "stop", "move", "stop"]);
      await page.keyboard.press("Escape");
      await page.getByRole("link", { name: "录像检索", exact: true }).click();
      await page.getByRole("button", { name: "查询录像", exact: true }).click();
      await expect(page.getByRole("button", { name: /1分0秒/ })).toBeVisible();
      await page.getByRole("link", { name: "事件中心", exact: true }).click();
      await page.getByRole("button", { name: "确认", exact: true }).click();
      await expect(page.getByRole("cell", { name: "已确认", exact: true })).toBeVisible();
      await page.getByRole("link", { name: "系统管理", exact: true }).click();
      await expect(page.getByText("运行正常", { exact: true })).toBeVisible();
      await expect(page.getByText("camera.updated", { exact: true })).toBeVisible();
      failStatus = true;
      await page.getByRole("button", { name: "刷新", exact: true }).click();
      await expect(page.getByRole("alert")).toContainText("system-failure-123");
      await expect(page.locator("body")).not.toContainText("SECRET");
      await expect(page.getByText("运行正常", { exact: true })).toHaveCount(0);
      await page.getByRole("alert").getByRole("button", { name: "Try again" }).click();
      await expect(page.getByText("运行正常", { exact: true })).toBeVisible();
      await page.getByRole("button", { name: "Create administrator", exact: true }).click();
      await page.getByLabel("Username", { exact: true }).fill("secondary");
      await page.getByLabel("New password", { exact: true }).fill("replacement password");
      await page.getByRole("button", { name: "Save administrator", exact: true }).click();
      await expect(page.getByRole("rowheader", { name: "secondary", exact: true })).toBeVisible();
      for (const theme of ["light", "dark"]) {
        await page.getByLabel("Theme").selectOption(theme);
        assert.deepEqual((await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21aa"]).analyze()).violations, []);
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
      }
      await page.getByRole("link", { name: "实时监控", exact: true }).click();
      await page.getByRole("button", { name: "删除", exact: true }).click();
      await expect(page.getByRole("button", { name: "Cancel", exact: true })).toBeFocused();
      await page.getByRole("button", { name: "Confirm", exact: true }).click();
      await expect(page.getByText("还没有匹配的摄像头。", { exact: true })).toBeVisible();
      assert.ok(!paths.some(path => path.includes("/users")));
      assert.deepEqual(errors, []);
      console.log(`${engine.name()}: current Sentinel system/admin/cameras/recordings/events, PTZ stop, modal focus and mobile WCAG AA passed`);
      await context.close();
    } finally { await browser.close(); }
  }
} finally { await new Promise((resolve, reject) => server.httpServer.close(error => error ? reject(error) : resolve())); }
