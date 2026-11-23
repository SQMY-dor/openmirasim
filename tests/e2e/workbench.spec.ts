import { expect, test } from "@playwright/test";

test.describe("OpenMirasim Web Workbench — Phase 0 Acceptance", () => {
  test("login, create session, stream reply + plan + tool call, approve ACP option, catch up after disconnect, and restore after reload", async ({
    page,
  }) => {
    const consoleErrors: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });
    page.on("pageerror", (err) => {
      consoleErrors.push(err.message);
    });

    await page.goto("/");

    // 1. Enter Local Mock preview (Phase 0 acceptance flow without needing external API keys)
    await expect(page.locator("#connect-heading")).toHaveText("OpenMirasim");
    await page.locator("#enter-demo-btn").click();

    await expect(page.locator("#active-session-title")).toContainText(
      "Phase 0 架构核对"
    );
    await expect(page.locator("#sync-status-chip")).toContainText("已同步");

    // 2. Create a fresh OpenCode ACP session
    await page.locator("#new-session-btn").click();
    await page.locator("#new-session-title").fill("E2E 验收会话");
    await page.locator("#new-session-harness").selectOption("opencode");
    await page.locator("#confirm-create-session-btn").click();

    await expect(page.locator("#active-session-title")).toHaveText(
      "E2E 验收会话"
    );

    // 3. Send a regular prompt and verify streaming output + Plan strip + ToolCall card
    await page
      .locator("#prompt-textarea")
      .fill("检查 crates/om-core 契约并汇报状态");
    await page.locator("#send-prompt-btn").click();

    const firstAssistant = page
      .locator('[data-turn-kind="assistant"]')
      .first();
    await expect(firstAssistant).toHaveAttribute("data-completed", "true", {
      timeout: 10_000,
    });
    await expect(firstAssistant).toContainText("Local Mock 演示模式");
    await expect(firstAssistant.locator('[data-plan-strip="true"]')).toContainText(
      "2/2"
    );
    await expect(firstAssistant.locator("[data-tool-call-id]")).toContainText(
      "Search crates/ workspace"
    );

    // 4. Trigger an ACP approval request and approve via `allow_once` option
    await page
      .locator("#prompt-textarea")
      .fill("执行审批验证 deploy reload");
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

    // 5. Simulate network drop and verify gapless `last_seq` catchup
    await page.locator("#toggle-inspector-btn").click();
    await expect(page.locator("#event-inspector")).toBeVisible();

    await page.locator("#simulate-drop-btn").click();
    await expect(page.locator("#session-stream")).toContainText(
      "重连后已通过",
      { timeout: 10_000 }
    );
    await expect(page.locator("#sync-status-chip")).toContainText("已同步");
    await expect(page.locator("#inspector-reconnects")).toContainText("1 / 1");

    // Capture applied seq before browser refresh
    const seqStat = (await page.locator("#inspector-last-seq").textContent()) ?? "";
    const seqMatch = seqStat.match(/^#(\d+)\s*\/\s*#(\d+)$/);
    expect(seqMatch).not.toBeNull();
    const seqBeforeReload = seqMatch![1];
    expect(seqMatch![2]).toBe(seqBeforeReload);

    // 6. Reload browser and verify full persisted session + transcript recovery
    await page.reload();
    await expect(page.locator("#active-session-title")).toHaveText(
      "E2E 验收会话"
    );
    await expect(page.locator("#sync-status-chip")).toContainText(
      `seq #${seqBeforeReload} · 已同步`
    );
    await expect(page.locator("#session-stream")).toContainText("审批已通过");
    await expect(page.locator("#session-stream")).toContainText(
      "重连后已通过"
    );

    expect(consoleErrors).toEqual([]);
  });
});
