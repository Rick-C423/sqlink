import assert from "node:assert/strict";
import { test } from "node:test";
import { DatasetController } from "../src/lib/datasetController";
import type { QueryResult } from "../src/types/database";

function row(value: string): QueryResult {
  return { columns: [{ name: "payload", dataType: "text", unifiedType: "string", nullable: false }], rows: [{ values: [value] }], executionTimeMs: 1 };
}
function fixture(limits?: { pageBytes: number; cursorEntries: number }) {
  const retired: QueryResult[] = [];
  const pageCancels: string[] = [];
  const countCancels: string[] = [];
  const controller = new DatasetController({
    retire: (result) => retired.push(result),
    cancelPage: async (taskId) => { pageCancels.push(taskId); },
    cancelCount: async (taskId) => { countCancels.push(taskId); },
  }, limits);
  return { controller, retired, pageCancels, countCancels };
}

test("默认单页64MiB预算拒绝超限新结果并保留当前页", () => {
  const { controller, retired } = fixture();
  const first = row("当前页");
  controller.accept(controller.beginPage("A", 1), first, "cursor-2");
  const retained = controller.retainedBytes;
  const tooLarge = row("x".repeat(64 * 1024 * 1024));
  assert.throws(() => controller.accept(controller.beginPage("A", 2), tooLarge), /DATASET_PAGE_BUDGET_EXCEEDED/);
  assert.deepEqual(retired, [tooLarge]);
  assert.equal(controller.retainedBytes, retained);
  controller.dispose();
  assert.deepEqual(retired, [tooLarge, first]);
});

test("精确缓存预算允许边界值，后续翻页只保留单页及有界游标", () => {
  const seed = row("边界");
  const bytes = new TextEncoder().encode(JSON.stringify(seed)).byteLength;
  const { controller, retired } = fixture({ pageBytes: bytes, cursorEntries: 3 });
  assert.equal(controller.accept(controller.beginPage("A", 1), seed, "cursor-2"), true);
  assert.equal(controller.retainedBytes, bytes);
  for (let page = 2; page <= 20; page++) {
    const result = row(String(page));
    controller.accept(controller.beginPage("A", page), result, `cursor-${page + 1}`);
    assert.equal(controller.retainedBytes, new TextEncoder().encode(JSON.stringify(result)).byteLength);
  }
  assert.equal(retired.length, 19);
  // 检查缓存的空间边界，避免回归成保留每一页行数据的历史列表。
  const state = controller as unknown as { cursors: Map<number, string>; cached: QueryResult };
  assert.equal(state.cursors.size, 3);
  assert.ok([...state.cursors.values()].every((token) => typeof token === "string"));
  assert.deepEqual(state.cached.rows, [{ values: ["20"] }]);
  controller.dispose();
  assert.equal(controller.retainedBytes, 0);
  assert.equal(retired.length, 20);
});

test("游标仅相邻向后续页可用，返回旧页及任意跳页使用offset", () => {
  const { controller } = fixture();
  controller.accept(controller.beginPage("A", 1), row("1"), "cursor-2");
  const second = controller.beginPage("A", 2);
  assert.equal(second.cursor, "cursor-2");
  controller.accept(second, row("2"), "cursor-3");
  assert.equal(controller.beginPage("A", 1).cursor, undefined);
  assert.equal(controller.beginPage("A", 4).cursor, undefined);
  controller.dispose();
});

test("新请求胜出，旧页迟到只能回收，不能替换新页或保留额外行", () => {
  const { controller, retired, pageCancels } = fixture();
  const old = controller.beginPage("A", 1);
  const current = controller.beginPage("A", 2);
  const accepted = row("当前结果");
  controller.accept(current, accepted, "cursor-3");
  const retained = controller.retainedBytes;
  const stale = row("迟到结果");
  assert.equal(controller.accept(old, stale, "stale-cursor"), false);
  assert.deepEqual(retired, [stale]);
  assert.deepEqual(pageCancels, [old.taskId]);
  assert.equal(controller.retainedBytes, retained);
  assert.equal(controller.beginPage("A", 3).cursor, "cursor-3");
  controller.dispose();
});

test("完整来源、排序及epoch变化均清空旧游标并拒绝旧ticket", () => {
  const { controller, retired } = fixture();
  const sources = [
    ["A", "db", "one", "items", "id", "asc", 0],
    ["A", "db", "two", "items", "id", "asc", 0],
    ["A", "other", "two", "items", "id", "asc", 0],
    ["B", "other", "two", "items", "id", "asc", 0],
    ["B", "other", "two", "items", "id", "desc", 0],
    ["B", "other", "two", "items", "id", "desc", 1],
  ].map((source) => JSON.stringify(source));
  let previous = controller.beginPage(sources[0], 1);
  controller.accept(previous, row("原来源"), "old-cursor");
  for (const source of sources.slice(1)) {
    const current = controller.beginPage(source, 2);
    assert.equal(current.cursor, undefined);
    assert.equal(controller.isCurrent(previous), false);
    controller.accept(current, row(source), "new-cursor");
    previous = current;
  }
  assert.equal(retired.length, sources.length - 1);
  controller.dispose();
});

test("COUNT票据独立取消，迟到统计不能跨页或覆盖较新统计", async () => {
  const { controller, countCancels } = fixture();
  const page = controller.beginPage("A", 1);
  controller.accept(page, row("1"));
  controller.finishPage(page);
  const old = controller.beginCount();
  const current = controller.beginCount();
  assert.equal(controller.isCountCurrent(old), false);
  assert.equal(controller.isCountCurrent(current), true);
  controller.finishCount(old);
  await controller.requestCancel("count");
  assert.equal(controller.isCountCurrent(current), true, "取消请求不能冒充已取消终态");
  assert.deepEqual(countCancels, [old.taskId, current.taskId]);
  controller.beginPage("A", 2);
  assert.equal(controller.isCountCurrent(current), false);
  controller.dispose();
});

test("StrictMode式dispose后activate允许新请求，旧ticket仍永久失效", () => {
  const { controller, retired } = fixture();
  const old = controller.beginPage("A", 1);
  const count = controller.beginCount();
  controller.dispose();
  assert.throws(() => controller.beginPage("A", 1), /已关闭/);
  controller.activate();
  const current = controller.beginPage("A", 1);
  assert.equal(controller.isCurrent(old), false);
  assert.equal(controller.isCountCurrent(count), false);
  assert.equal(controller.accept(current, row("新生命周期")), true);
  const stale = row("旧生命周期迟到");
  assert.equal(controller.accept(old, stale), false);
  assert.deepEqual(retired, [stale]);
  controller.dispose();
});
