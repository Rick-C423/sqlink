import assert from "node:assert/strict";
import { beforeEach, afterEach, test } from "node:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { TableData } from "../src/components/TableView";
import { useConnectionStore } from "../src/stores/connectionStore";
import type { ControlledTablePage, ControlledTableReadRequest } from "../src/types/resultContext";
import { DataGrid } from "../src/components/result/DataGrid";
import { guardGridDraft, isGridDraftDirty, useGridDraftStore } from "../src/stores/gridDraftStore";
import { tauriService } from "../src/services/tauriService";
import type { QueryResult } from "../src/types/database";
import type { ChangeSetRequest } from "../src/types/typedValue";
import type { CommitReceipt } from "../src/types/commitReceipt";

const id = (n: number) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
const state = () => useGridDraftStore.getState();
function source(): QueryResult {
  const names = ["大整数", "文本", "未验证类型", "生成列"];
  return { columns: names.map((name) => ({ name, dataType: "text", unifiedType: "string", nullable: true })),
    rows: [{ values: [9007199254740992, "旧文本", "只读值", "生成值"] }], executionTimeMs: 1,
    typedRows: [[{ kind: "int64", value: "9007199254740993" }, { kind: "text", value: "旧文本" }, null, { kind: "text", value: "生成值" }]], rowHandles: [id(7)],
    resultContext: { protocolVersion: 1, resultId: id(1), revision: 1, connectionId: "A", connectionGeneration: "g",
      source: { database: "db", table: "原表", kind: "table" }, columns: names.map((name, index) => ({ columnId: id(index + 2), displayName: name,
        databaseType: "text", nullable: true, writable: true, generated: index === 3, origin: { database: "db", table: "原表", column: name } })),
      editPolicy: { allowedOperations: ["insert", "update", "delete"], reasonCodes: [], columnInputKinds: { [id(2)]: "integer", [id(3)]: "text", [id(5)]: "text" } },
      createdAt: 1, expiresAt: 100 } };
}
function receipt(request: ChangeSetRequest): CommitReceipt {
  return { protocolVersion: 1, changeSetId: request.changeSetId, resultId: request.resultId, revision: request.revision,
    connectionId: "A", connectionGeneration: "g", taskId: null, status: "committed", commitState: "committed", rollbackState: "notStarted",
    cleanupState: "succeeded", refreshState: "required", operationResults: request.operations.map((op) => ({ opId: op.opId,
      outcome: "applied", matchedRows: 1, affectedRows: 1, errorCode: null })), errorCode: null, reasonCodes: [], completedAt: 1 };
}
let view: ReactTestRenderer | undefined;
const originalConnectionState = useConnectionStore.getState();
const originals = { read: tauriService.readTablePage, commit: tauriService.commitChangeSet, receipt: tauriService.getChangeSetReceipt, release: tauriService.releaseResultContext };
beforeEach(() => { useGridDraftStore.setState({ drafts: {}, navigation: null }); tauriService.releaseResultContext = async () => {}; });
afterEach(() => { act(() => view?.unmount()); view = undefined;
  useConnectionStore.setState(originalConnectionState); tauriService.readTablePage = originals.read;
  tauriService.commitChangeSet = originals.commit; tauriService.getChangeSetReceipt = originals.receipt; tauriService.releaseResultContext = originals.release; });
const cell = (index: number) => view!.root.findAllByType("td").filter((node) => node.props.onDoubleClick)[index];
async function click(label: string) {
  const button = view!.root.findAllByType("button").find((node) => node.children.some((child) => child === label) || node.props.title === label);
  assert.ok(button, `应存在按钮 ${label}`);
  await act(async () => { await button.props.onClick(); });
}
function edit(index: number, text: string) {
  act(() => cell(index).props.onDoubleClick());
  const input = view!.root.findAllByType("input").find((node) => node.props.autoFocus);
  assert.ok(input, "应定位到网格编辑器，而非分页页码输入框");
  act(() => input.props.onChange({ target: { value: text } }));
}

test("受控DataGrid只开放已发布类型，编辑器用无损整数原值，尚未失焦也走类型化提交", async () => {
  const original = source(); let sent: ChangeSetRequest | undefined; let refreshes = 0;
  tauriService.commitChangeSet = async (request) => { sent = request; return receipt(request); };
  act(() => { view = create(<DataGrid draftKey="typed" result={original} onRefreshCommitted={async (context) => {
    assert.equal(context, original.resultContext); refreshes++;
  }} />); });
  act(() => cell(2).props.onDoubleClick()); assert.equal(view!.root.findAllByType("input").length, 0);
  act(() => cell(3).props.onDoubleClick()); assert.equal(view!.root.findAllByType("input").length, 0);
  act(() => cell(0).props.onDoubleClick()); assert.equal(view!.root.findByType("input").props.value, "9007199254740993");
  act(() => view!.root.findByType("input").props.onChange({ target: { value: "9007199254740995" } }));
  let saved = false; await act(async () => { saved = await state().saveDraft("typed"); });
  assert.equal(saved, true); assert.equal(refreshes, 1);
  const operation = sent!.operations[0]; assert.equal(operation.kind, "update");
  if (operation.kind === "update") {
    assert.equal(operation.rowHandle, id(7));
    assert.deepEqual(operation.changes[id(2)], { mode: "set", value: { kind: "int64", value: "9007199254740995" } });
  }
  assert.equal(isGridDraftDirty(state().drafts.typed), false);
});

test("网格明确NULL按钮与文本空串均可提交，新增省略字段保留DEFAULT语义", async () => {
  const sent: ChangeSetRequest[] = []; tauriService.commitChangeSet = async (request) => { sent.push(request); return receipt(request); };
  const original = source();
  act(() => { view = create(<DataGrid draftKey="typed" result={original} onRefreshCommitted={async () => {}} />); });
  edit(1, "");
  act(() => view!.root.findByType("input").props.onBlur({ target: { value: "" } }));
  assert.equal(state().drafts.typed.edits["0:1"], "");
  await click("新增行");
  act(() => cell(5).props.onDoubleClick());
  act(() => view!.root.findByType("input").props.onChange({ target: { value: "" } }));
  act(() => view!.root.findByType("input").props.onBlur({ target: { value: "" } }));
  let saved = false; await act(async () => { saved = await state().saveDraft("typed"); }); assert.equal(saved, true);
  const [update, insert] = sent[0].operations;
  assert.equal(update.kind, "update"); assert.equal(insert.kind, "insert");
  if (update.kind === "update" && insert.kind === "insert") {
    assert.deepEqual(update.changes[id(3)], { mode: "set", value: { kind: "text", value: "" } });
    assert.deepEqual({ ...insert.changes }, { [id(3)]: { mode: "set", value: { kind: "text", value: "" } } });
  }
  // 用新结果开始下一份草稿，NULL按钮不会把NULL字样当作数据库空值。
  act(() => view!.update(<DataGrid draftKey="typed" result={source()} onRefreshCommitted={async () => {}} />));
  edit(1, "NULL"); await click("设为 SQL NULL");
  await act(async () => { await state().saveDraft("typed"); });
  const last = sent[1].operations[0]; if (last.kind === "update") assert.deepEqual(last.changes[id(3)], { mode: "set", value: { kind: "null" } });
});

test("未知提交固定原请求，导航保存和核查按钮只查回执，提交后刷新失败只重读", async () => {
  let commits = 0; let reads = 0; let lookups = 0; let request!: ChangeSetRequest;
  tauriService.commitChangeSet = async (value) => { commits++; request = value; throw new Error("响应丢失"); };
  tauriService.getChangeSetReceipt = async () => { lookups++; return receipt(request); };
  act(() => { view = create(<DataGrid draftKey="typed" result={source()} onRefreshCommitted={async () => {
    reads++; if (reads === 1) throw new Error("刷新失败");
  }} />); });
  edit(1, "草稿");
  await act(async () => { assert.equal(await state().saveDraft("typed"), false); });
  assert.equal(state().drafts.typed.saveState, "unknown");
  const pending = state().drafts.typed.pendingRequest;
  let decision!: Promise<boolean>; act(() => { decision = guardGridDraft("typed"); });
  await act(async () => { await state().chooseNavigation("save"); }); assert.equal(await decision, false);
  assert.equal(commits, 1); assert.equal(lookups, 1); assert.equal(reads, 1);
  assert.equal(state().drafts.typed.pendingRequest, pending);
  assert.equal(state().drafts.typed.saveState, "committedRefreshFailed");
  assert.equal(isGridDraftDirty(state().drafts.typed), false);
  await click("重试读取"); assert.equal(reads, 2); assert.equal(commits, 1);
});

test("整数校验失败保留原输入，未授权操作与原始SQL结果均不能产生提交", async () => {
  let commits = 0; tauriService.commitChangeSet = async (request) => { commits++; return receipt(request); };
  const original = source(); original.resultContext!.editPolicy.allowedOperations = ["update"];
  act(() => { view = create(<DataGrid draftKey="typed" result={original} onRefreshCommitted={async () => {}} />); });
  assert.equal(view!.root.findAllByType("button").some((node) => node.children.some((child) => child === "新增行")), false);
  edit(0, "1e2"); await act(async () => { assert.equal(await state().saveDraft("typed"), false); });
  assert.equal(commits, 0); assert.equal(state().drafts.typed.editingCell?.value, "1e2"); assert.match(state().drafts.typed.error!, /严格十进制/);
  act(() => state().discardDraft("typed"));
  const raw = { ...source(), resultContext: undefined, rowHandles: undefined, typedRows: undefined };
  act(() => view!.update(<DataGrid draftKey="typed" result={raw} onRefreshCommitted={async () => {}} />));
  act(() => cell(0).props.onDoubleClick()); assert.equal(view!.root.findAllByType("input").length, 0); assert.equal(commits, 0);
});

test("组件卸载重挂后保存和未知回执刷新重绑当前组件，保留原结果身份", async () => {
  const original = source(); let request!: ChangeSetRequest; let oldReads = 0; let newReads = 0;
  tauriService.commitChangeSet = async (value) => { request = value; throw new Error("响应丢失"); };
  tauriService.getChangeSetReceipt = async () => receipt(request);
  act(() => { view = create(<DataGrid draftKey="typed" result={original} onRefreshCommitted={async () => { oldReads++; }} />); });
  edit(1, "重挂草稿");
  act(() => view!.unmount());
  const newer = source(); newer.resultContext = { ...newer.resultContext!, resultId: id(90) };
  act(() => { view = create(<DataGrid draftKey="typed" result={newer} onRefreshCommitted={async (context) => {
    newReads++; assert.equal(context, original.resultContext);
  }} />); });
  await act(async () => { assert.equal(await state().saveDraft("typed"), false); });
  assert.equal(request.resultId, original.resultContext!.resultId);
  act(() => view!.unmount());
  act(() => { view = create(<DataGrid draftKey="typed" result={newer} onRefreshCommitted={async (context) => {
    newReads++; assert.equal(context, original.resultContext);
  }} />); });
  await click("核查回执");
  assert.equal(oldReads, 0); assert.equal(newReads, 1);
});

test("TableData受控读改刷新端到端：切换全局选择仍提交原结果并读取解析后的完整来源", async () => {
  const requests: ControlledTableReadRequest[] = []; let committed: ChangeSetRequest | undefined;
  useConnectionStore.setState({ saved: [{ id: "A", name: "原连接", driverType: "postgres", host: "localhost", port: 5432,
    user: "test", password: "", database: "请求库" }], activeConnection: "A", selectedDatabase: "请求库", liveIds: new Set(["A"]), connectionEpochs: {} });
  tauriService.readTablePage = async (request) => {
    requests.push(request);
    const result = source(); const context = result.resultContext!;
    context.resultId = id(100 + requests.length);
    context.source = { database: "解析库", schema: "真实模式", table: "真实表", kind: "table" };
    context.columns.forEach((column, index) => {
      column.writable = index < 2;
      column.origin = { database: "解析库", schema: "真实模式", table: "真实表", column: column.displayName };
    });
    context.editPolicy.columnInputKinds = { [id(2)]: "integer", [id(3)]: "text" };
    result.typedRows![0][2] = { kind: "text", value: "只读值" };
    if (requests.length > 1) { result.rows[0].values[1] = "新文本"; result.typedRows![0][1] = { kind: "text", value: "新文本" }; }
    return { result, context, rowHandles: result.rowHandles!, typedRows: result.typedRows,
      table: { database: "解析库", schema: "真实模式", table: "真实表", columns: result.columns.map((column, index) => ({
        name: column.name, dataType: column.dataType, nullable: column.nullable, isPrimaryKey: index === 0 })) }, totalRows: 1 } satisfies ControlledTablePage;
  };
  tauriService.commitChangeSet = async (request) => { committed = request; return receipt(request); };
  await act(async () => { view = create(<TableData connectionId="A" database="请求库" table="显示表" />); });
  assert.equal(requests.length, 1); edit(1, "新文本");
  act(() => useConnectionStore.setState({ activeConnection: "B", selectedDatabase: "其他库" }));
  let saved = false;
  await act(async () => { saved = await state().saveDraft(JSON.stringify(["table", "A", "请求库", "显示表"])); });
  assert.equal(saved, true); assert.equal(committed!.resultId, id(101)); assert.equal(requests.length, 2);
  assert.deepEqual(requests[1], { protocolVersion: 1, connectionId: "A", database: "解析库", tableParts: ["真实模式", "真实表"], page: 1, pageSize: 20, sort: null });
  assert.match(JSON.stringify(view!.toJSON()), /新文本/);
});
