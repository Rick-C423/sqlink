import assert from "node:assert/strict";
import { test } from "node:test";
import { cloneCellInput, isCellInput, isTypedValue, MAX_TYPED_CELL_BYTES } from "../src/lib/typedValue";
import type { CellInput, TypedValue } from "../src/types/typedValue";

test("显式空串、NULL 文本、零、布尔及 SQL NULL 保持各自类型", () => {
  for (const value of [
    { kind: "null" }, { kind: "text", value: "" }, { kind: "text", value: "NULL" },
    { kind: "int64", value: "0" }, { kind: "uint64", value: "0" }, { kind: "float", value: 0 },
    { kind: "boolean", value: false }, { kind: "decimal", value: "0.000" }, { kind: "binary", value: "" },
  ]) assert.equal(isTypedValue(value), true, JSON.stringify(value));
  for (const value of [null, "NULL", "", 0, false, { kind: "default" }, { kind: "null", value: null },
    { kind: "text", value: 1 }, { kind: "boolean", value: "false" }, { kind: "float", value: "1" },
    { kind: "float", value: NaN }, { kind: "float", value: Infinity }, { kind: "float", value: -Infinity },
    { kind: "text", value: "合法", extra: true }]) assert.equal(isTypedValue(value), false);
});

test("int64 与 uint64 只接受范围内的规范十进制字符串且不会转为 JS number", () => {
  const valid = [
    { kind: "int64", value: "-9223372036854775808" }, { kind: "int64", value: "9223372036854775807" },
    { kind: "int64", value: "9007199254740993" }, { kind: "uint64", value: "18446744073709551615" },
  ];
  for (const value of valid) assert.equal(isTypedValue(value), true);
  for (const [kind, value] of [
    ["int64", "-9223372036854775809"], ["int64", "9223372036854775808"], ["uint64", "18446744073709551616"],
    ["uint64", "-1"], ["int64", "-0"], ["int64", "+1"], ["int64", "01"], ["uint64", "00"],
    ["int64", "1e3"], ["int64", " 1"], ["int64", "1.0"], ["uint64", "１"],
  ]) assert.equal(isTypedValue({ kind, value }), false, `${kind}:${value}`);
  assert.equal(isTypedValue({ kind: "int64", value: 9007199254740992 }), false);
});

test("decimal 保留 scale，拒绝指数、前导零、负零及超长载荷", () => {
  for (const value of ["0", "0.000", "123.4500", "-0.0001", "-123.4500", "9".repeat(1024)]) {
    const input: CellInput = { mode: "set", value: { kind: "decimal", value } };
    assert.equal(isCellInput(input), true);
    assert.deepEqual(cloneCellInput(input), input);
  }
  for (const value of ["1e-9", "+1", "01", "00.1", "-0", "-0.000", ".1", "1.", " 1", "9".repeat(1025)]) {
    assert.equal(isTypedValue({ kind: "decimal", value }), false, value.slice(0, 30));
  }
});

test("文本按 UTF-8 字节数限制；二进制按解码后大小和 Base64 规范补位限制", () => {
  assert.equal(isTypedValue({ kind: "text", value: "a".repeat(MAX_TYPED_CELL_BYTES) }), true);
  assert.equal(isTypedValue({ kind: "text", value: "a".repeat(MAX_TYPED_CELL_BYTES + 1) }), false);
  assert.equal(isTypedValue({ kind: "text", value: "中".repeat(Math.floor(MAX_TYPED_CELL_BYTES / 3)) }), true);
  assert.equal(isTypedValue({ kind: "text", value: "中".repeat(Math.floor(MAX_TYPED_CELL_BYTES / 3) + 1) }), false);
  for (const value of ["Zg==", "Zm8=", "Zm9v", "/w==", Buffer.alloc(MAX_TYPED_CELL_BYTES, 255).toString("base64")]) {
    assert.equal(isTypedValue({ kind: "binary", value }), true);
  }
  for (const value of ["Zg", "Zg=", "Zh==", "Zm9=", "_w==", "Z g==", "====", "=AAA", "A===",
    Buffer.alloc(MAX_TYPED_CELL_BYTES + 1).toString("base64")]) assert.equal(isTypedValue({ kind: "binary", value }), false);
});

test("日期与时间严格检查闰年、秒数和 precision，不接受自动日期归一化", () => {
  for (const value of ["2024-02-29", "2000-02-29", "0001-01-01", "9999-12-31"]) {
    assert.equal(isTypedValue({ kind: "date", value }), true);
  }
  for (const value of ["0000-01-01", "1900-02-29", "2023-02-29", "2024-04-31", "2024-00-01", "2024-13-01", "2024-01-00", "2024-1-01"]) {
    assert.equal(isTypedValue({ kind: "date", value }), false, value);
  }
  for (const value of [{ value: "00:00:00", precision: 0 }, { value: "23:59:59.123456789", precision: 9 },
    { value: "12:34:56.000", precision: 3 }]) assert.equal(isTypedValue({ kind: "time", value }), true);
  for (const value of [{ value: "24:00:00", precision: 0 }, { value: "23:59:60", precision: 0 },
    { value: "00:60:00", precision: 0 }, { value: "01:02:03.1", precision: 0 }, { value: "01:02:03", precision: 1 },
    { value: "01:02:03.1", precision: 1.5 }, { value: "01:02:03", precision: -1 },
    { value: "01:02:03.1234567890", precision: 10 }, { value: "01:02:03Z", precision: 0 }]) {
    assert.equal(isTypedValue({ kind: "time", value }), false, JSON.stringify(value));
  }
});

test("timestamp 明确区分无时区与 offset，精度和时区不能互相猜测", () => {
  for (const [value, timezone, precision] of [
    ["2024-02-29T01:02:03", "none", 0], ["2024-02-29T01:02:03.123456789Z", "offset", 9],
    ["2024-02-29T01:02:03+08:00", "offset", 0], ["2024-02-29T01:02:03-23:59", "offset", 0],
  ]) assert.equal(isTypedValue({ kind: "timestamp", value: { value, timezone, precision } }), true);
  for (const [value, timezone, precision] of [
    ["2024-02-29T01:02:03Z", "none", 0], ["2024-02-29T01:02:03", "offset", 0],
    ["2024-02-29T01:02:03+24:00", "offset", 0], ["2024-02-29T01:02:03+08:60", "offset", 0],
    ["2024-02-29T01:02:03z", "offset", 0], ["2024-02-29 01:02:03Z", "offset", 0],
    ["2024-02-29T01:02:60Z", "offset", 0], ["2024-02-29T01:02:03.12Z", "offset", 1],
    ["2024-02-29T01:02:03Z", "unknown", 0],
  ]) assert.equal(isTypedValue({ kind: "timestamp", value: { value, timezone, precision } }), false);
});

test("DEFAULT 是独立输入意图；复制请求隔离嵌套时间值且拒绝非显式输入", () => {
  assert.equal(isCellInput({ mode: "default" }), true);
  assert.equal(isCellInput({ mode: "set", value: { kind: "null" } }), true);
  for (const value of [{ mode: "default", value: null }, { mode: "set", value: null }, { kind: "null" }, { mode: "set", value: "NULL" }]) {
    assert.equal(isCellInput(value), false);
    assert.throws(() => cloneCellInput(value as CellInput), /显式类型化值/);
  }
  const time: TypedValue = { kind: "time", value: { value: "12:34:56.123", precision: 3 } };
  const copied = cloneCellInput({ mode: "set", value: time });
  time.value.value = "00:00:00.000";
  assert.deepEqual(copied, { mode: "set", value: { kind: "time", value: { value: "12:34:56.123", precision: 3 } } });
});
