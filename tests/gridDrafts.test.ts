import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import { guardGridDraft, guardGridDrafts, isGridDraftDirty, materializeGridDraft, queryDraftKey, tableDraftKey, useGridDraftStore } from "../src/stores/gridDraftStore";
import type { EditContext, QueryResult } from "../src/types/database";

const original: QueryResult = { columns: [{ name: "id", dataType: "int", unifiedType: "integer", nullable: false }], rows: [{ values: [1] }], executionTimeMs: 1 };
const context: EditContext = { connectionId: "A", database: "db", table: "items", primaryKeys: ["id"], driverType: "postgres" };
const state = () => useGridDraftStore.getState();
beforeEach(() => useGridDraftStore.setState({ drafts: {}, navigation: null }));

function dirty(save = async () => {}) {
  state().ensureDraft("tab", original, context, save);
  state().updateDraft("tab", () => ({ edits: { "0:0": "2" } }));
}

test("稳定键使用结构化数组，特殊字符不会碰撞", () => {
  assert.equal(queryDraftKey("a:b"), '["query","a:b"]');
  assert.notEqual(tableDraftKey("a:b", "c", "t"), tableDraftKey("a", "b:c", "t"));
});

test("干净页面不显示对话框；取消不运行后续动作且保留全部草稿", async () => {
  assert.equal(await guardGridDraft("tab"), true);
  assert.equal(state().navigation, null);
  let saves = 0; let navigations = 0;
  dirty(async () => { saves++; });
  const before = state().drafts.tab;
  const decision = guardGridDraft("tab").then((allowed) => { if (allowed) navigations++; return allowed; });
  assert.deepEqual(state().navigation?.keys, ["tab"]);
  await state().chooseNavigation("cancel");
  assert.equal(await decision, false);
  assert.equal(state().drafts.tab, before);
  assert.equal(saves, 0);
  assert.equal(navigations, 0);
});

test("保存失败阻止导航且保留草稿及错误", async () => {
  dirty(async () => { throw new Error("连接已断开"); });
  let navigations = 0;
  const decision = guardGridDraft("tab").then((allowed) => { if (allowed) navigations++; return allowed; });
  await state().chooseNavigation("save");
  assert.equal(await decision, false);
  assert.equal(navigations, 0);
  assert.equal(isGridDraftDirty(state().drafts.tab), true);
  assert.equal(state().drafts.tab.edits["0:0"], "2");
  assert.match(state().drafts.tab.error!, /连接已断开/);
  assert.equal(state().drafts.tab.submitting, false);
});

test("正在输入但尚未失焦的值参加保存，成功后清理草稿", async () => {
  let saved = "";
  state().ensureDraft("tab", original, context, async (draft) => { saved = draft.edits["0:0"]; });
  state().updateDraft("tab", () => ({ editingCell: { key: "0:0", value: "输入中" } }));
  assert.equal(isGridDraftDirty(state().drafts.tab), true);
  assert.equal(materializeGridDraft(state().drafts.tab).edits["0:0"], "输入中");
  const decision = guardGridDraft("tab");
  await state().chooseNavigation("save");
  assert.equal(await decision, true);
  assert.equal(saved, "输入中");
  assert.equal(isGridDraftDirty(state().drafts.tab), false);
  assert.equal(state().drafts.tab.editingCell, null);
});

test("脏草稿跨卸载保留且新结果不能替换其来源与保存操作", async () => {
  let savedOld = 0; let savedNew = 0;
  dirty(async () => { savedOld++; });
  const next = { ...original, rows: [{ values: [99] }] };
  state().ensureDraft("tab", next, { ...context, connectionId: "B" }, async () => { savedNew++; });
  assert.equal(state().drafts.tab.sourceResult, original);
  assert.equal(state().drafts.tab.sourceContext, context);
  assert.equal(await state().saveDraft("tab"), true);
  assert.equal(savedOld, 1);
  assert.equal(savedNew, 0);
  state().ensureDraft("tab", next, context);
  assert.equal(state().drafts.tab.sourceResult, next);
});

test("放弃允许导航但不调用保存；多个草稿统一守卫", async () => {
  let saves = 0;
  dirty(async () => { saves++; });
  state().ensureDraft("other", original, context);
  state().updateDraft("other", () => ({ newRows: [{}] }));
  const decision = guardGridDrafts(["tab", "other", "tab"]);
  assert.deepEqual(state().navigation?.keys, ["tab", "other"]);
  await state().chooseNavigation("discard");
  assert.equal(await decision, true);
  assert.equal(saves, 0);
  assert.equal(isGridDraftDirty(state().drafts.tab), false);
  assert.equal(isGridDraftDirty(state().drafts.other), false);
});

test("保存期间拒绝并发导航、二次提交及草稿修改", async () => {
  let finish!: () => void; let calls = 0;
  dirty(async () => { calls++; await new Promise<void>((resolve) => { finish = resolve; }); });
  const decision = guardGridDraft("tab");
  assert.equal(await guardGridDraft("other"), false);
  const choosing = state().chooseNavigation("save");
  assert.equal(state().drafts.tab.submitting, true);
  assert.equal(await guardGridDraft("other"), false);
  assert.equal(await state().saveDraft("tab"), false);
  state().discardDraft("tab");
  state().updateDraft("tab", () => ({ edits: {} }));
  assert.equal(state().drafts.tab.edits["0:0"], "2");
  assert.equal(calls, 1);
  finish(); await choosing;
  assert.equal(await decision, true);
});
