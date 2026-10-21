import assert from "node:assert/strict";
import { test } from "node:test";
import { act, create } from "react-test-renderer";
import { DataGrid } from "../src/components/result/DataGrid";
import { getGridEditPolicy } from "../src/lib/gridEdits";
import { useGridDraftStore } from "../src/stores/gridDraftStore";
import type { QueryResult } from "../src/types/database";

const result = (): QueryResult => ({ columns: [{ name: "payload", dataType: "TEXT", unifiedType: "Unknown", nullable: true }],
  rows: [], truncated: true, executionTimeMs: 1 });

test("零行超大单元格和少行字节截断均显示预算提示，不能误报仅10000行截断", () => {
  for (const rows of [[], [{ values: ["完整内容"] }]]) {
    useGridDraftStore.setState({ drafts: {}, navigation: null });
    const source: QueryResult = { ...result(), rows };
    let view!: ReturnType<typeof create>;
    try {
      act(() => { view = create(<DataGrid draftKey="budget-message" result={source} />); });
      const output = JSON.stringify(view.toJSON());
      assert.match(output, /行数、字节或单元格预算/);
      assert.doesNotMatch(output, /10000 行上限/);
      assert.match(output, /截断结果不可编辑/);
      assert.equal(view.root.findAllByType("input").length, 0);
      assert.equal(view.root.findAllByType("button").some((button) => button.children.includes("新增行") || button.children.includes("提交")), false);
      assert.equal(source.rows.length, rows.length);
    } finally { act(() => view?.unmount()); }
  }
});

test("截断结果即使带有允许操作的上下文也拒绝编辑，完整结果仍按既有能力决定", () => {
  const source = result();
  source.resultContext = { protocolVersion: 1, resultId: "id", revision: 1, connectionId: "A", connectionGeneration: "g",
    source: { database: "db", table: "samples", kind: "table" }, columns: [{ columnId: "column", displayName: "payload",
      databaseType: "text", nullable: true, writable: true, generated: false }],
    editPolicy: { allowedOperations: ["insert", "update", "delete"], reasonCodes: [], columnInputKinds: { column: "text" } }, createdAt: 1, expiresAt: 2 };
  assert.equal(getGridEditPolicy(source).editable, false);
  assert.equal(getGridEditPolicy({ ...source, truncated: false }).editable, true);
  assert.equal(getGridEditPolicy({ ...source, truncated: false, resultContext: undefined }).editable, false);
});
