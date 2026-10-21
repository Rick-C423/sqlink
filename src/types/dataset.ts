import type { ControlledTablePage, ControlledTableReadRequest } from "./resultContext";

export interface DatasetReadOptions { taskId: string; cursor?: string }
export interface DatasetPage extends ControlledTablePage {
  hasMore: boolean;
  nextCursor: string | null;
  paginationMode: "offset" | "keyset";
  observedOrder: { column: string; direction: "asc" | "desc" }[];
  approxBytes: number;
  truncatedReason: string | null;
  paginationFallbackReason?: string | null;
}
export interface DatasetPageRequest { request: ControlledTableReadRequest; taskId: string; cursor?: string }
export interface TableCountRequest { protocolVersion: 1; resultId: string; revision: number; taskId: string }
export interface TableCountResult {
  resultId: string; revision: number; connectionId: string; connectionGeneration: string; totalRows: string;
}
