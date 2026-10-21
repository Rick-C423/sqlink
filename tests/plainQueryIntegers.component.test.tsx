import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { DataGrid } from "../src/components/result/DataGrid";
import { getGridEditPolicy, QUERY_READ_ONLY_REASON } from "../src/lib/gridEdits";
import { compareGridValues } from "../src/lib/gridSort";
import { useGridDraftStore } from "../src/stores/gridDraftStore";
import { tauriService } from "../src/services/tauriService";
import type { ColumnMetadata, QueryResult } from "../src/types/database";

// Rust 的真实普通查询必须输出同一份夹具；这里再经过 JavaScript 的 JSON.parse。
const wire = () => JSON.parse(readFileSync("tests/fixtures/plain-query-integers.json", "utf8")) as QueryResult;
const column = (dataType: string): ColumnMetadata => ({ name: "value", dataType, unifiedType: "Unknown", nullable: true });
let view: ReactTestRenderer | undefined;
const originalNavigator = Object.getOwnPropertyDescriptor(globalThis, "navigator");
const originalCommit = tauriService.commitChangeSet;
beforeEach(() => { useGridDraftStore.setState({ drafts: {}, navigation: null }); });
afterEach(() => {
  act(() => view?.unmount()); view = undefined;
  tauriService.commitChangeSet = originalCommit;
  if (originalNavigator) Object.defineProperty(globalThis, "navigator", originalNavigator);
  else Reflect.deleteProperty(globalThis, "navigator");
});

test("普通SQL整数夹具经JSON.parse仍逐字准确，安全整数继续为Number，数字文本保持String", () => {
  const result = wire();
  const expected = ["-9223372036854775808", "-9007199254740993", "-9007199254740992", "-9007199254740991",
    "-1", "0", "1", "9007199254740991", "9007199254740992", "9007199254740993", "9223372036854775807"];
  assert.deepEqual(result.rows.map((row) => String(row.values[0])), expected);
  assert.deepEqual(result.rows.map((row) => row.values[1]), expected);
  for (const index of [3, 4, 5, 6, 7]) assert.equal(typeof result.rows[index].values[0], "number");
  for (const index of [0, 1, 2, 8, 9, 10]) assert.equal(typeof result.rows[index].values[0], "string");
});

test("真实DataGrid显示和复制完整int64/uint64，旧编辑上下文不能让普通SQL产生草稿或提交", async () => {
  const result = wire();
  result.rows = result.rows.filter((row) => typeof row.values[0] === "string");
  result.rows.push({ values: ["18446744073709551615", "18446744073709551615"] });
  let copied = ""; let commits = 0;
  Object.defineProperty(globalThis, "navigator", { configurable: true,
    value: { clipboard: { writeText: async (text: string) => { copied = text; } } } });
  tauriService.commitChangeSet = async () => { commits++; throw new Error("普通SQL不应调用提交"); };
  const context = { connectionId: "A", database: "db", table: "samples", driverType: "sqlite", primaryKeys: ["integer_value"] };
  assert.equal(getGridEditPolicy(result, context).editable, false);
  act(() => { view = create(<DataGrid result={result} draftKey="plain-integers" editContext={context}
    readOnlyReason={QUERY_READ_ONLY_REASON} onRefreshCommitted={async () => { throw new Error("不应触发受控刷新"); }} />); });
  const cells = view!.root.findAllByType("td").filter((node) => node.props.onDoubleClick);
  assert.deepEqual(cells.map((node) => node.children.join("")), result.rows.flatMap((row) => row.values));
  for (const cell of cells) act(() => cell.props.onDoubleClick());
  assert.equal(view!.root.findAllByType("input").length, 0);
  assert.equal(view!.root.findAllByType("button").some((node) => node.children.includes("新增行") || node.children.includes("提交")), false);
  assert.equal(useGridDraftStore.getState().drafts["plain-integers"].editingCell, null);
  assert.deepEqual(useGridDraftStore.getState().drafts["plain-integers"].edits, {});
  const copy = view!.root.findAllByType("button").find((node) => node.props.title === "复制全部行（TSV，无表头）");
  assert.ok(copy);
  await act(async () => { await copy.props.onClick(); });
  assert.equal(copied, result.rows.map((row) => row.values.join("\t")).join("\n"));
  assert.equal(commits, 0);
});

test("明确整数列的本地排序精确比较正负大整数与安全Number，升降序NULL均置后", () => {
  const values = ["18446744073709551615", "9223372036854775807", "9007199254740993", "9007199254740992",
    9007199254740991, 10, 2, -2, "-9007199254740992", "-9007199254740993", "-9223372036854775808", null];
  const expected = ["-9223372036854775808", "-9007199254740993", "-9007199254740992", -2, 2, 10, 9007199254740991,
    "9007199254740992", "9007199254740993", "9223372036854775807", "18446744073709551615", null];
  for (const dataType of ["INTEGER", "bigint", "bigint unsigned", "int8", "Int8", "MYSQL_TYPE_LONGLONG"]) {
    assert.deepEqual([...values].sort((a, b) => compareGridValues(column(dataType), a, b, "asc")), expected);
    assert.deepEqual([...values].sort((a, b) => compareGridValues(column(dataType), a, b, "desc")), [...expected.slice(0, -1).reverse(), null]);
  }
});

test("数字TEXT和类型未知表达式保持字符串排序，不凭显示文本推断整数", () => {
  const values = ["2", "10", "9007199254740993", "0002", "-10", "-2"];
  for (const dataType of ["TEXT", "varchar", "MYSQL_TYPE_VAR_STRING", "", "NUMERIC"]) {
    assert.deepEqual([...values].sort((a, b) => compareGridValues(column(dataType), a, b, "asc")), [...values].sort((a, b) => a.localeCompare(b)));
    assert.deepEqual([...values].sort((a, b) => compareGridValues(column(dataType), a, b, "desc")), [...values].sort((a, b) => b.localeCompare(a)));
  }
});

test("DataGrid表头实际触发精确整数本地排序，分页排序仍交由原回调处理", async () => {
  const result: QueryResult = { columns: [column("MYSQL_TYPE_LONGLONG")], rows: ["9007199254740993", 2, "9007199254740992", -1].map((value) => ({ values: [value] })), executionTimeMs: 1 };
  const cells = () => view!.root.findAllByType("td").filter((node) => node.props.onDoubleClick).map((node) => node.children.join(""));
  const header = () => view!.root.findAllByType("th").find((node) => node.props.onClick)!;
  act(() => { view = create(<DataGrid draftKey="sort-integers" result={result} />); });
  await act(async () => { await header().props.onClick(); });
  assert.deepEqual(cells(), ["-1", "2", "9007199254740992", "9007199254740993"]);
  await act(async () => { await header().props.onClick(); });
  assert.deepEqual(cells(), ["9007199254740993", "9007199254740992", "2", "-1"]);
  const calls: unknown[] = [];
  act(() => { view!.update(<DataGrid draftKey="sort-integers" result={result} onSortChange={(sort) => calls.push(sort)} />); });
  await act(async () => { await header().props.onClick(); });
  assert.deepEqual(calls, [{ key: "value", direction: "asc" }]);
  assert.deepEqual(cells(), ["9007199254740993", "2", "9007199254740992", "-1"]);
});
