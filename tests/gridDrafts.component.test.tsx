import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { DataGrid } from "../src/components/result/DataGrid";
import { DraftNavigationDialog } from "../src/components/result/DraftNavigationDialog";
import { guardGridDraft, GridDraft, isGridDraftDirty, useGridDraftStore } from "../src/stores/gridDraftStore";
import type { QueryResult } from "../src/types/database";

const source: QueryResult = {
  columns: [{ name: "名称", dataType: "text", unifiedType: "string", nullable: false }],
  rows: [{ values: ["旧数据"] }], executionTimeMs: 1,
};
const state = () => useGridDraftStore.getState();
let view: ReactTestRenderer | undefined;
const originalDocument = Object.getOwnPropertyDescriptor(globalThis, "document");
const originalHTMLElement = Object.getOwnPropertyDescriptor(globalThis, "HTMLElement");

beforeEach(() => {
  useGridDraftStore.setState({ drafts: {}, navigation: null });
  // renderer 无浏览器 DOM，仅补齐对话框焦点恢复所读的两项能力。
  Object.defineProperty(globalThis, "HTMLElement", { configurable: true, value: class {} });
  Object.defineProperty(globalThis, "document", { configurable: true, value: { activeElement: null } });
});
afterEach(() => {
  act(() => view?.unmount()); view = undefined;
  if (originalDocument) Object.defineProperty(globalThis, "document", originalDocument); else Reflect.deleteProperty(globalThis, "document");
  if (originalHTMLElement) Object.defineProperty(globalThis, "HTMLElement", originalHTMLElement); else Reflect.deleteProperty(globalThis, "HTMLElement");
});

function mountDialog(save: NonNullable<GridDraft["save"]> = async () => {}) {
  state().ensureDraft("tab", source, null, save);
  state().updateDraft("tab", () => ({ editingCell: { key: "0:0", value: "输入中" } }));
  act(() => { view = create(<DraftNavigationDialog />); });
}
async function click(label: string) {
  const button = view!.root.findAllByType("button").find((item) => item.children.join("") === label);
  assert.ok(button, `应存在按钮：${label}`);
  let operation: Promise<void> | undefined;
  act(() => { operation = button.props.onClick(); });
  await operation;
  act(() => {});
}

test("真实对话框组件：干净页面不弹窗，取消保留输入且不执行后续操作", async () => {
  let saves = 0; let navigations = 0;
  act(() => { view = create(<DraftNavigationDialog />); });
  assert.equal(await guardGridDraft("tab"), true);
  assert.equal(view!.toJSON(), null);
  act(() => view!.unmount());
  mountDialog(async () => { saves++; });
  let decision!: Promise<boolean>;
  act(() => { decision = guardGridDraft("tab").then((allowed) => { if (allowed) navigations++; return allowed; }); });
  assert.equal(view!.root.findByProps({ role: "dialog" }).props["aria-modal"], "true");
  await click("取消");
  assert.equal(await decision, false);
  assert.equal(saves, 0); assert.equal(navigations, 0);
  assert.equal(state().drafts.tab.editingCell?.value, "输入中");
  assert.equal(view!.toJSON(), null);
});

test("真实对话框组件：保存失败阻止导航，显示持久错误并保留尚未失焦的输入", async () => {
  let saves = 0; let navigations = 0;
  mountDialog(async (draft) => { saves++; assert.equal(draft.edits["0:0"], "输入中"); throw new Error("保存失败示例"); });
  let decision!: Promise<boolean>;
  act(() => { decision = guardGridDraft("tab").then((allowed) => { if (allowed) navigations++; return allowed; }); });
  await click("保存并继续");
  assert.equal(await decision, false);
  assert.equal(saves, 1); assert.equal(navigations, 0);
  assert.equal(isGridDraftDirty(state().drafts.tab), true);
  assert.match(state().drafts.tab.error!, /保存失败示例/);
  assert.equal(state().drafts.tab.edits["0:0"], "输入中");
});

test("真实对话框组件：放弃草稿允许继续且不调用保存", async () => {
  let saves = 0;
  mountDialog(async () => { saves++; });
  let decision!: Promise<boolean>;
  act(() => { decision = guardGridDraft("tab"); });
  await click("放弃改动");
  assert.equal(await decision, true);
  assert.equal(saves, 0);
  assert.equal(isGridDraftDirty(state().drafts.tab), false);
});

test("DataGrid 真实组件：新结果保留原草稿来源，卸载重挂保留草稿并展示只读原因", () => {
  state().ensureDraft("tab", source);
  state().updateDraft("tab", () => ({ edits: { "0:0": "草稿值" }, error: "之前保存失败" }));
  const next = { ...source, rows: [{ values: ["新结果行内容"] }] };
  act(() => { view = create(<DataGrid draftKey="tab" result={next} readOnlyReason="来源尚未验证" />); });
  let rendered = JSON.stringify(view!.toJSON());
  assert.match(rendered, /草稿值/);
  assert.match(rendered, /原始数据/);
  assert.match(rendered, /来源尚未验证/);
  assert.match(rendered, /之前保存失败/);
  assert.doesNotMatch(rendered, /新结果行内容/);
  assert.equal(state().drafts.tab.sourceResult, source);
  act(() => view!.unmount());
  act(() => { view = create(<DataGrid draftKey="tab" result={next} />); });
  assert.equal(state().drafts.tab.edits["0:0"], "草稿值");
  act(() => state().discardDraft("tab"));
  rendered = JSON.stringify(view!.toJSON());
  assert.match(rendered, /新结果行内容/);
  assert.doesNotMatch(rendered, /草稿值/);
});

test("DataGrid 本地排序通过真实对话框守卫，取消保持原排序，放弃后才排序", async () => {
  state().ensureDraft("tab", source);
  state().updateDraft("tab", () => ({ edits: { "0:0": "草稿值" } }));
  act(() => { view = create(<><DataGrid draftKey="tab" result={source} /><DraftNavigationDialog /></>); });
  const header = () => view!.root.findAllByType("th").find((item) => typeof item.props.onClick === "function")!;
  let sorting!: Promise<void>;
  act(() => { sorting = header().props.onClick(); });
  assert.equal(state().navigation?.keys[0], "tab");
  await click("取消"); await sorting;
  assert.doesNotMatch(JSON.stringify(view!.toJSON()), /▲/);
  assert.equal(state().drafts.tab.edits["0:0"], "草稿值");
  act(() => { sorting = header().props.onClick(); });
  await click("放弃改动"); await sorting;
  act(() => {});
  assert.match(JSON.stringify(view!.toJSON()), /▲/);
  assert.equal(isGridDraftDirty(state().drafts.tab), false);
});

test("DataGrid 即使传入编辑上下文和提交回调，也不能绕过真实只读策略", () => {
  let saves = 0;
  act(() => { view = create(<DataGrid draftKey="tab" result={source}
    editContext={{ connectionId: "A", database: "db", table: "items", primaryKeys: ["名称"], driverType: "postgres" }}
    onApplyEdits={async () => { saves++; }} />); });
  const cell = view!.root.findAllByType("td").find((item) => typeof item.props.onDoubleClick === "function")!;
  act(() => cell.props.onDoubleClick());
  assert.equal(view!.root.findAllByType("input").length, 0);
  assert.equal(saves, 0);
  assert.match(JSON.stringify(view!.toJSON()), /只读/);
  assert.match(JSON.stringify(view!.toJSON()), /列来源和行身份/);
});
