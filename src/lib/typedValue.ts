import type { CellInput, TypedValue } from "@/types/typedValue";

export const MAX_TYPED_CELL_BYTES = 1024 * 1024;
const encoder = new TextEncoder();
const base64Alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

function hasKeys<K extends string>(value: unknown, keys: K[]): value is Record<K, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value) &&
    Object.keys(value).length === keys.length && keys.every((key) => Object.prototype.hasOwnProperty.call(value, key));
}

function validDate(value: string): boolean {
  if (!/^[0-9]{4}-[0-9]{2}-[0-9]{2}$/.test(value)) return false;
  const year = Number(value.slice(0, 4));
  const month = Number(value.slice(5, 7));
  const day = Number(value.slice(8, 10));
  if (year === 0 || month < 1 || month > 12 || day < 1) return false;
  const leap = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
  return day <= [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month - 1];
}

function validTime(value: string, precision: number): boolean {
  const match = /^([0-9]{2}):([0-9]{2}):([0-9]{2})(?:\.([0-9]{1,9}))?$/.exec(value);
  return Number.isInteger(precision) && precision >= 0 && precision <= 9 && match !== null &&
    Number(match[1]) < 24 && Number(match[2]) < 60 && Number(match[3]) < 60 &&
    (match[4]?.length ?? 0) === precision;
}

function validBinary(value: string): boolean {
  if (value.length > Math.ceil(MAX_TYPED_CELL_BYTES / 3) * 4 ||
    value.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(value)) return false;
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0;
  if (value.length === 0) return true;
  if (value.length - padding < 2) return false;
  if (value.length / 4 * 3 - padding > MAX_TYPED_CELL_BYTES) return false;
  // 补位比特必须为零，防止多个 Base64 字符串表示同一字节串。
  return padding === 0 || (base64Alphabet.indexOf(value[value.length - padding - 1]) & (padding === 2 ? 15 : 3)) === 0;
}

/** 只校验显式类型，绝不把空串、NULL 字样或零转换成其他类型。 */
export function isTypedValue(value: unknown): value is TypedValue {
  if (!value || typeof value !== "object" || !("kind" in value)) return false;
  if (value.kind === "null") return hasKeys(value, ["kind"]);
  if (!hasKeys(value, ["kind", "value"])) return false;
  const payload = value.value;
  switch (value.kind) {
    case "text": return typeof payload === "string" && encoder.encode(payload).byteLength <= MAX_TYPED_CELL_BYTES;
    case "boolean": return typeof payload === "boolean";
    case "int64":
      return typeof payload === "string" && /^-?(?:0|[1-9][0-9]*)$/.test(payload) && payload !== "-0" &&
        payload.length <= 20 && BigInt(payload) >= -9223372036854775808n && BigInt(payload) <= 9223372036854775807n;
    case "uint64":
      return typeof payload === "string" && /^(?:0|[1-9][0-9]*)$/.test(payload) && payload.length <= 20 &&
        BigInt(payload) <= 18446744073709551615n;
    case "decimal":
      return typeof payload === "string" && payload.length <= 1024 && /^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?$/.test(payload) &&
        !/^-0(?:\.0+)?$/.test(payload);
    case "float": return typeof payload === "number" && Number.isFinite(payload);
    case "binary": return typeof payload === "string" && validBinary(payload);
    case "date": return typeof payload === "string" && validDate(payload);
    case "time":
      return hasKeys(payload, ["value", "precision"]) && typeof payload.value === "string" &&
        typeof payload.precision === "number" && validTime(payload.value, payload.precision);
    case "timestamp": {
      if (!hasKeys(payload, ["value", "precision", "timezone"]) || typeof payload.value !== "string" ||
        typeof payload.precision !== "number" || !validDate(payload.value.slice(0, 10)) || payload.value[10] !== "T") return false;
      const rest = payload.value.slice(11);
      if (payload.timezone === "none") return validTime(rest, payload.precision);
      if (payload.timezone !== "offset") return false;
      const match = /^(.*)(Z|[+-]([0-9]{2}):([0-9]{2}))$/.exec(rest);
      return match !== null && validTime(match[1], payload.precision) &&
        (match[2] === "Z" || (Number(match[3]) < 24 && Number(match[4]) < 60));
    }
    default: return false;
  }
}

export function isCellInput(value: unknown): value is CellInput {
  if (!value || typeof value !== "object" || !("mode" in value)) return false;
  return value.mode === "default" ? hasKeys(value, ["mode"]) :
    value.mode === "set" && hasKeys(value, ["mode", "value"]) && isTypedValue(value.value);
}

/** 传输前复制已经校验的值，避免调用方随后修改草稿而改变请求。 */
export function cloneCellInput(input: CellInput): CellInput {
  if (!isCellInput(input)) throw new Error("单元格输入必须是有效的显式类型化值或 DEFAULT 意图。");
  if (input.mode === "default") return { mode: "default" };
  const value = input.value;
  if (value.kind === "time" || value.kind === "timestamp") return { mode: "set", value: { ...value, value: { ...value.value } } as TypedValue };
  return { mode: "set", value: { ...value } };
}
