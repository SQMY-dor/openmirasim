import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const REPO_ROOT = path.resolve(import.meta.dirname, "../..");
const SERVER_BIN =
  process.env.OM_SERVER_BIN ?? path.join(REPO_ROOT, "target/debug/om-server");
const STATIC_DIR = path.join(REPO_ROOT, "apps/web/dist");
const TOKEN = "phase0-fullstack-e2e-token";

async function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      if (!addr || typeof addr === "string") {
        srv.close(() => reject(new Error("failed to allocate port")));
        return;
      }
      const { port } = addr;
      srv.close(() => resolve(port));
    });
  });
}

async function waitForHealthz(baseUrl: string, timeoutMs = 15_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`${baseUrl}/healthz`);
      if (res.ok) return;
    } catch {
      // keep polling until listening
    }
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`om-server did not become healthy at ${baseUrl}/healthz`);
}

function startServer(port: number, dataDir: string): ChildProcess {
  const child = spawn(SERVER_BIN, [], {
    cwd: REPO_ROOT,
    env: {
      ...process.env,
      OM_BIND: `127.0.0.1:${port}`,
      OM_DATA_DIR: dataDir,
      OM_TOKEN: TOKEN,
      OM_ALLOW_DIRECT_SANDBOX: "1",
      OM_STATIC_DIR: STATIC_DIR,
      RUST_LOG: "info",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  return child;
}

async function stopServer(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null) return;
  const exited = new Promise<void>((resolve) => {
    child.once("exit", () => resolve());
  });
  child.kill("SIGTERM");
  await Promise.race([
    exited,
    new Promise<void>((resolve) =>
      setTimeout(() => {
        child.kill("SIGKILL");
        resolve();
      }, 5_000)
    ),
  ]);
}

test.describe("OpenMirasim Full-Stack Phase 0 E2E (real om-server + SQLite + Web)", () => {
  test("static token login, streaming, approval, reload catchup, server process restart recovery, and resume", async ({
    page,
  }) => {
    expect(fs.existsSync(SERVER_BIN)).toBe(true);
    expect(fs.existsSync(path.join(STATIC_DIR, "index.html"))).toBe(true);

    const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "om-fullstack-e2e-"));
    const port = await findFreePort();
    const baseUrl = `http://127.0.0.1:${port}`;
    let server = startServer(port, dataDir);

    const consoleErrors: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });
    page.on("pageerror", (err) => {
      consoleErrors.push(err.message);
    });

    try {
      await waitForHealthz(baseUrl);
      await page.goto(baseUrl);

      // 1. Login with wrong token fails cleanly, valid token succeeds
      await page.locator("#server-url-input").fill(baseUrl);
      await page.locator("#access-token-input").fill("wrong-token");
      await page.locator("#connect-submit-btn").click();
      await expect(page.locator("#connect-error")).toBeVisible();

      await page.locator("#access-token-input").fill(TOKEN);
      await page.locator("#connect-submit-btn").click();
      await expect(page.locator("#new-session-btn")).toBeVisible({
        timeout: 10_000,
      });

      // 2. Create a real `mock` session backed by Orchestrator + rusqlite WAL
      await page.locator("#new-session-btn").click();
      await page.locator("#new-session-title").fill("真实服务端全栈验收");
      await page.locator("#new-session-harness").selectOption("mock");
      await page.locator("#new-session-cwd").fill(dataDir);
      await page.locator("#confirm-create-session-btn").click();

      await expect(page.locator("#active-session-title")).toHaveText(
        "真实服务端全栈验收"
      );
      await expect(page.locator("#sync-status-chip")).toContainText("已同步", {
        timeout: 10_000,
      });
      await expect(page.locator("#prompt-textarea")).toBeEnabled({
        timeout: 10_000,
      });

      // 3. Send prompt -> real streaming Plan + ToolCall + MessageChunk over /ws/events
      await page.locator("#prompt-textarea").fill("验证真实服务端流式输出");
      await page.locator("#send-prompt-btn").click();

      const firstAssistant = page
        .locator('[data-turn-kind="assistant"]')
        .first();
      await expect(firstAssistant).toHaveAttribute("data-completed", "true", {
        timeout: 10_000,
      });
      await expect(firstAssistant).toContainText("mock echo: 验证真实服务端流式输出");
      await expect(
        firstAssistant.locator('[data-plan-strip="true"]')
      ).toContainText("2/2");

      // 4. Trigger real approval gate -> approve via REST POST /api/v1/approvals/:id/resolve
      await page.locator("#prompt-textarea").fill("please approve deploy");
      await page.locator("#send-prompt-btn").click();

      const approveBtn = page.locator('[data-approval-kind="allow_once"]');
      await expect(approveBtn).toBeVisible({ timeout: 10_000 });
      await approveBtn.click();

      const secondAssistant = page
        .locator('[data-turn-kind="assistant"]')
        .nth(1);
      await expect(secondAssistant).toHaveAttribute("data-completed", "true", {
        timeout: 10_000,
      });
      await expect(secondAssistant).toContainText("审批已通过");

      // 5. Browser reload restores full transcript from SQLite via Subscribe { last_seq: 0 }
      await page.reload();
      await expect(page.locator("#active-session-title")).toHaveText(
        "真实服务端全栈验收",
        { timeout: 10_000 }
      );
      await expect(page.locator("#session-stream")).toContainText(
        "mock echo: 验证真实服务端流式输出"
      );
      await expect(page.locator("#session-stream")).toContainText("审批已通过");

      // 6. Kill real `om-server` and restart a brand-new process against the SAME SQLite DB
      await stopServer(server);
      server = startServer(port, dataDir);
      await waitForHealthz(baseUrl);

      await page.reload();
      await expect(page.locator("#active-session-title")).toHaveText(
        "真实服务端全栈验收",
        { timeout: 10_000 }
      );
      await expect(page.locator("#session-stream")).toContainText("审批已通过");
      await expect(page.locator("#resume-session-btn")).toBeVisible({
        timeout: 10_000,
      });

      // 7. Click "恢复会话" -> POST /api/v1/sessions/:id/resume -> session returns to Idle and runs a new turn!
      await page.locator("#resume-session-btn").click();
      await expect(page.locator("#prompt-textarea")).toBeEnabled({
        timeout: 10_000,
      });

      await page.locator("#prompt-textarea").fill("重启恢复后的新回合");
      await page.locator("#send-prompt-btn").click();

      const thirdAssistant = page
        .locator('[data-turn-kind="assistant"]')
        .nth(2);
      await expect(thirdAssistant).toHaveAttribute("data-completed", "true", {
        timeout: 10_000,
      });
      await expect(thirdAssistant).toContainText("mock echo: 重启恢复后的新回合");

      // Ignore the intentional 401 console error from the wrong-token login check
      const unexpectedErrors = consoleErrors.filter(
        (e) => !e.includes("status of 401")
      );
      expect(unexpectedErrors).toEqual([]);
    } finally {
      await stopServer(server);
      fs.rmSync(dataDir, { recursive: true, force: true });
    }
  });

  test("Phase 1: multi-harness catalog, inline Git workspace registration, live worktree diff, file tree, and approval rules", async ({
    page,
  }) => {
    expect(fs.existsSync(SERVER_BIN)).toBe(true);
    expect(fs.existsSync(path.join(STATIC_DIR, "index.html"))).toBe(true);

    const { execFileSync } = await import("node:child_process");
    const rootTmp = fs.mkdtempSync(path.join(os.tmpdir(), "om-phase1-e2e-"));
    const dataDir = path.join(rootTmp, "data");
    const repoDir = path.join(rootTmp, "sample-repo");
    fs.mkdirSync(dataDir, { recursive: true });
    fs.mkdirSync(repoDir, { recursive: true });

    const git = (args: string[]) =>
      execFileSync("git", args, { cwd: repoDir, stdio: "pipe" });
    git(["init", "-b", "main"]);
    git(["config", "user.email", "e2e@openmirasim.dev"]);
    git(["config", "user.name", "OpenMirasim E2E"]);
    fs.writeFileSync(path.join(repoDir, "README.md"), "# Phase 1 Repo\n");
    git(["add", "README.md"]);
    git(["commit", "-m", "initial"]);

    const port = await findFreePort();
    const baseUrl = `http://127.0.0.1:${port}`;
    const server = startServer(port, dataDir);

    const consoleErrors: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });
    page.on("pageerror", (err) => {
      consoleErrors.push(err.message);
    });

    try {
      await waitForHealthz(baseUrl);
      await page.goto(baseUrl);

      await page.locator("#server-url-input").fill(baseUrl);
      await page.locator("#access-token-input").fill(TOKEN);
      await page.locator("#connect-submit-btn").click();
      await expect(page.locator("#new-session-btn")).toBeVisible({
        timeout: 10_000,
      });

      // 1. Open NewSessionModal and verify all 5 Phase 1 harnesses are registered
      await page.locator("#new-session-btn").click();
      const harnessValues = await page
        .locator("#new-session-harness option")
        .evaluateAll((opts) =>
          opts.map((o) => (o as HTMLOptionElement).value)
        );
      for (const expected of ["mock", "opencode", "dsh", "cc", "codexcli"]) {
        expect(harnessValues).toContain(expected);
      }

      // 2. Register the temporary Git repo inline & create a worktree-isolated session
      await page.locator("#toggle-register-workspace-btn").click();
      await page.locator("#register-ws-name").fill("phase1-demo");
      await page.locator("#register-ws-path").fill(repoDir);
      await page.locator("#confirm-register-ws-btn").click();
      await expect(page.locator("#new-session-workspace")).not.toHaveValue("");

      await page.locator("#new-session-title").fill("Phase 1 Worktree 会话");
      await page.locator("#new-session-harness").selectOption("mock");
      await page.locator("#confirm-create-session-btn").click();

      await expect(page.locator("#active-session-title")).toHaveText(
        "Phase 1 Worktree 会话",
        { timeout: 10_000 }
      );
      await expect(page.locator("#prompt-textarea")).toBeEnabled({
        timeout: 10_000,
      });

      // 3. Modify README.md inside the provisioned session worktree so Diff has real content
      const sessionsRes = await fetch(`${baseUrl}/api/v1/sessions`, {
        headers: { Authorization: `Bearer ${TOKEN}` },
      });
      const sessionsBody = (await sessionsRes.json()) as {
        sessions: Array<{ id: string }>;
      };
      const sid = sessionsBody.sessions[0].id;
      const wtRes = await fetch(`${baseUrl}/api/v1/sessions/${sid}/worktree`, {
        headers: { Authorization: `Bearer ${TOKEN}` },
      });
      const wt = (await wtRes.json()) as { path: string };
      fs.writeFileSync(
        path.join(wt.path, "README.md"),
        "# Phase 1 Repo\nphase1-e2e-live-diff\n"
      );

      // 4. Open WorktreeDrawer -> inspect Git Diff, File Tree + preview, Approval Rules
      await page.locator("#toggle-worktree-btn").click();
      await expect(page.locator("#worktree-drawer")).toBeVisible();
      await expect(page.locator("#worktree-drawer")).toContainText(
        "+phase1-e2e-live-diff",
        { timeout: 10_000 }
      );

      await page.locator("#tab-worktree-files").click();
      const readmeBtn = page
        .locator("#worktree-drawer")
        .getByRole("button", { name: /README\.md/ });
      await expect(readmeBtn).toBeVisible({ timeout: 10_000 });
      await readmeBtn.click();
      await expect(page.locator("#worktree-drawer")).toContainText(
        "phase1-e2e-live-diff",
        { timeout: 10_000 }
      );

      await page.locator("#tab-approval-rules").click();
      await expect(page.locator("#worktree-drawer")).toContainText(
        "0 条记忆规则",
        { timeout: 10_000 }
      );

      expect(consoleErrors).toEqual([]);
    } finally {
      await stopServer(server);
      fs.rmSync(rootTmp, { recursive: true, force: true });
    }
  });

  test("Phase 2: PWA manifest/SW, RBAC Team Console + Viewer device-token login + Audit Log + Antigravity GUI bridge noVNC takeover", async ({
    page,
  }) => {
    const GUI_BRIDGE_BIN =
      process.env.OM_GUI_BRIDGE_BIN ??
      path.join(REPO_ROOT, "target/debug/om-gui-bridge");
    expect(fs.existsSync(SERVER_BIN)).toBe(true);
    expect(fs.existsSync(GUI_BRIDGE_BIN)).toBe(true);
    expect(fs.existsSync(path.join(STATIC_DIR, "index.html"))).toBe(true);

    const rootTmp = fs.mkdtempSync(path.join(os.tmpdir(), "om-phase2-e2e-"));
    const dataDir = path.join(rootTmp, "data");
    const guiProfiles = path.join(rootTmp, "gui-profiles");
    fs.mkdirSync(dataDir, { recursive: true });
    fs.mkdirSync(guiProfiles, { recursive: true });

    const bridgePort = await findFreePort();
    const bridgeUrl = `http://127.0.0.1:${bridgePort}`;
    const bridge = spawn(
      GUI_BRIDGE_BIN,
      [
        "--bind",
        `127.0.0.1:${bridgePort}`,
        "--profile-root",
        guiProfiles,
        "--driver",
        "simulated",
        "--max-sessions",
        "2",
      ],
      {
        cwd: REPO_ROOT,
        stdio: ["ignore", "pipe", "pipe"],
      }
    );

    const port = await findFreePort();
    const baseUrl = `http://127.0.0.1:${port}`;
    const server = spawn(SERVER_BIN, [], {
      cwd: REPO_ROOT,
      env: {
        ...process.env,
        OM_BIND: `127.0.0.1:${port}`,
        OM_DATA_DIR: dataDir,
        OM_TOKEN: TOKEN,
        OM_ALLOW_DIRECT_SANDBOX: "1",
        OM_STATIC_DIR: STATIC_DIR,
        OM_GUI_BRIDGE_URL: bridgeUrl,
        RUST_LOG: "info",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });

    const consoleErrors: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });
    page.on("pageerror", (err) => {
      consoleErrors.push(err.message);
    });

    try {
      await waitForHealthz(bridgeUrl);
      await waitForHealthz(baseUrl);

      // 1. Verify PWA manifest and Service Worker are served cleanly
      const manifestRes = await fetch(`${baseUrl}/manifest.webmanifest`);
      expect(manifestRes.ok).toBe(true);
      const manifest = (await manifestRes.json()) as { short_name: string };
      expect(manifest.short_name).toBe("OpenMirasim");

      const swRes = await fetch(`${baseUrl}/sw.js`);
      expect(swRes.ok).toBe(true);
      expect(await swRes.text()).toContain("notificationclick");

      // 2. Owner logs in and sees OWNER role badge + Team Console button
      await page.goto(baseUrl);
      await page.locator("#server-url-input").fill(baseUrl);
      await page.locator("#access-token-input").fill(TOKEN);
      await page.locator("#connect-submit-btn").click();
      await expect(page.locator("#principal-role-badge")).toHaveText("OWNER", {
        timeout: 10_000,
      });

      // 3. Create an `antigravity-gui` session and verify message forwarding + noVNC takeover link
      await page.locator("#new-session-btn").click();
      await page.locator("#new-session-title").fill("Antigravity GUI 远程桥接");
      await page
        .locator("#new-session-harness")
        .selectOption("antigravity-gui");
      await page.locator("#new-session-cwd").fill(dataDir);
      await page.locator("#confirm-create-session-btn").click();

      await expect(page.locator("#open-remote-screen-btn")).toBeVisible({
        timeout: 10_000,
      });
      const takeoverHref = await page
        .locator("#open-remote-screen-btn")
        .getAttribute("href");
      expect(takeoverHref).toContain("/takeover/");
      const takeoverHtml = await (await fetch(takeoverHref!)).text();
      expect(takeoverHtml).toContain("Antigravity GUI 远程人工接管");

      await page.locator("#prompt-textarea").fill("请打开项目配置页");
      await page.locator("#send-prompt-btn").click();
      const guiReply = page.locator('[data-turn-kind="assistant"]').first();
      await expect(guiReply).toHaveAttribute("data-completed", "true", {
        timeout: 10_000,
      });
      await expect(guiReply).toContainText("请打开项目配置页");

      // 4. Open Team Console -> add a Viewer member -> issue `omt_...` device token -> check Audit Log
      await page.locator("#open-team-console-btn").click();
      await expect(page.locator("#team-console-modal")).toBeVisible();
      await page.locator("#add-member-name").fill("Alice Viewer");
      await page.locator("#add-member-role").selectOption("viewer");
      await page.locator("#confirm-add-member-btn").click();
      await expect(page.locator("#team-members-list")).toContainText(
        "Alice Viewer",
        { timeout: 10_000 }
      );

      await page.locator("#tab-team-tokens").click();
      await page.locator("#issue-token-label").fill("Alice Mobile PWA");
      await page.locator("#confirm-issue-token-btn").click();
      const secretEl = page.locator("#issued-token-secret");
      await expect(secretEl).toBeVisible({ timeout: 10_000 });
      const viewerSecret = (await secretEl.innerText()).trim();
      expect(viewerSecret.startsWith("omt_")).toBe(true);

      await page.locator("#tab-team-security").click();
      await expect(page.locator("#audit-log-list")).toContainText(
        "member.created",
        { timeout: 10_000 }
      );
      await expect(page.locator("#audit-log-list")).toContainText(
        "token.issued",
        { timeout: 10_000 }
      );

      // 5. Disconnect and log in as Alice Viewer with her `omt_...` device token -> read-only enforced
      await page.locator('#team-console-modal button[aria-label="关闭团队控制台"]').click();
      await page.locator("#disconnect-btn").click();
      await page.locator("#server-url-input").fill(baseUrl);
      await page.locator("#access-token-input").fill(viewerSecret);
      await page.locator("#connect-submit-btn").click();

      await expect(page.locator("#principal-role-badge")).toHaveText("VIEWER", {
        timeout: 10_000,
      });
      await expect(page.locator("#viewer-readonly-hint")).toBeVisible();
      await expect(page.locator("#viewer-composer-banner")).toBeVisible();
      await expect(page.locator("#new-session-btn")).toHaveCount(0);

      expect(consoleErrors).toEqual([]);
    } finally {
      await stopServer(server);
      await stopServer(bridge);
      fs.rmSync(rootTmp, { recursive: true, force: true });
    }
  });

  test("Phase 3: multi-server profiles, xterm.js binary /ws/pty, asciicast v2 recording & replay, and non-blocking /ws/events", async ({
    page,
  }) => {
    expect(fs.existsSync(SERVER_BIN)).toBe(true);
    expect(fs.existsSync(path.join(STATIC_DIR, "index.html"))).toBe(true);

    const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "om-phase3-e2e-"));
    const port = await findFreePort();
    const baseUrl = `http://127.0.0.1:${port}`;
    const server = startServer(port, dataDir);

    const consoleErrors: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });
    page.on("pageerror", (err) => {
      consoleErrors.push(err.message);
    });

    try {
      await waitForHealthz(baseUrl);
      await page.goto(baseUrl);

      // 1. Save a named server profile, reload, and switch via the profile chip
      await page.locator("#server-url-input").fill(baseUrl);
      await page.locator("#profile-name-input").fill("Team Linux Bare-Metal");
      await page.locator("#save-profile-btn").click();
      await expect(page.locator("#server-profiles-bar")).toContainText(
        "Team Linux Bare-Metal"
      );

      await page.reload();
      await expect(page.locator("#server-profiles-bar")).toContainText(
        "Team Linux Bare-Metal"
      );
      await page
        .locator("#server-profiles-bar .profile-chip-select")
        .first()
        .click();
      await expect(page.locator("#server-url-input")).toHaveValue(baseUrl);

      // 2. Connect & create a real session
      await page.locator("#access-token-input").fill(TOKEN);
      await page.locator("#connect-submit-btn").click();
      await expect(page.locator("#new-session-btn")).toBeVisible({
        timeout: 10_000,
      });

      await page.locator("#new-session-btn").click();
      await page.locator("#new-session-title").fill("Phase 3 PTY 终端验收");
      await page.locator("#new-session-harness").selectOption("mock");
      await page.locator("#new-session-cwd").fill(dataDir);
      await page.locator("#confirm-create-session-btn").click();

      await expect(page.locator("#active-session-title")).toHaveText(
        "Phase 3 PTY 终端验收",
        { timeout: 10_000 }
      );
      await expect(page.locator("#prompt-textarea")).toBeEnabled({
        timeout: 10_000,
      });

      const sessionsRes = await fetch(`${baseUrl}/api/v1/sessions`, {
        headers: { Authorization: `Bearer ${TOKEN}` },
      });
      const sessionsBody = (await sessionsRes.json()) as {
        sessions: Array<{ id: string }>;
      };
      const sid = sessionsBody.sessions[0].id;

      // 3. Switch to xterm.js PTY view and send a real shell command over /ws/pty
      await page.locator("#view-mode-terminal").click();
      await expect(page.locator("#pty-terminal-view")).toBeVisible();
      await expect(page.locator("#pty-status-chip")).toContainText("PTY 已连接", {
        timeout: 10_000,
      });

      const xtermInput = page.locator("#pty-terminal-view .xterm-helper-textarea");
      await xtermInput.focus();
      await page.keyboard.type("echo phase3-xterm-ok");
      await page.keyboard.press("Enter");

      // Poll .cast recording endpoint until the shell echoed our marker
      let castBody = "";
      const deadline = Date.now() + 10_000;
      while (Date.now() < deadline) {
        const castRes = await fetch(
          `${baseUrl}/api/v1/sessions/${sid}/pty/recording`,
          { headers: { Authorization: `Bearer ${TOKEN}` } }
        );
        if (castRes.ok) {
          expect(castRes.headers.get("content-type")).toContain(
            "application/x-asciicast"
          );
          castBody = await castRes.text();
          if (castBody.includes("phase3-xterm-ok")) break;
        }
        await new Promise((r) => setTimeout(r, 200));
      }
      expect(castBody).toContain("phase3-xterm-ok");
      const firstLine = JSON.parse(castBody.split("\n")[0]) as {
        version: number;
      };
      expect(firstLine.version).toBe(2);

      // 4. Replay the asciicast v2 recording inside xterm.js
      await page.locator("#replay-cast-btn").click();
      await expect(page.locator("#pty-status-note")).toContainText(
        "asciicast v2 回放完成",
        { timeout: 10_000 }
      );

      // 5. Switch back to Chat view -> /ws/events remains live and unblocked
      await page.locator("#view-mode-chat").click();
      await expect(page.locator("#prompt-textarea")).toBeEnabled({
        timeout: 10_000,
      });
      await page
        .locator("#prompt-textarea")
        .fill("PTY切换后事件流保持畅通");
      await page.locator("#send-prompt-btn").click();

      const assistant = page.locator('[data-turn-kind="assistant"]').first();
      await expect(assistant).toHaveAttribute("data-completed", "true", {
        timeout: 10_000,
      });
      await expect(assistant).toContainText(
        "mock echo: PTY切换后事件流保持畅通"
      );

      expect(consoleErrors).toEqual([]);
    } finally {
      await stopServer(server);
      fs.rmSync(dataDir, { recursive: true, force: true });
    }
  });
});

