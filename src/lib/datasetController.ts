import type { QueryResult } from "@/types/database";
import type { ResultContext } from "@/types/resultContext";

export interface DatasetTicket { source: string; sequence: number; taskId: string; page: number; cursor?: string }
export interface DatasetCountTicket { source: string; sequence: number; taskId: string; resultId?: string; revision?: number; connectionGeneration?: string }

/** 每个页面仅持有当前结果；历史只留有界游标，不把浏览过的行累积在内存中。 */
export class DatasetController {
  private source = "";
  private sequence = 0;
  private countSequence = 0;
  private active?: DatasetTicket;
  private counting?: DatasetCountTicket;
  private cached?: QueryResult;
  private cursors = new Map<number, string>();
  private page = 0;
  private disposed = false;
  retainedBytes = 0;

  constructor(private callbacks: {
    cancelPage: (taskId: string) => Promise<void>;
    cancelCount: (taskId: string) => Promise<void>;
    retire: (result: QueryResult) => void;
  }, private limits = { pageBytes: 64 * 1024 * 1024, cursorEntries: 128 }) {}

  private cancel(kind: "page" | "count"): void {
    const taskId = kind === "page" ? this.active?.taskId : this.counting?.taskId;
    if (!taskId) return;
    void (kind === "page" ? this.callbacks.cancelPage(taskId) : this.callbacks.cancelCount(taskId)).catch(() => undefined);
  }

  reset(source: string): void {
    this.cancel("page"); this.cancel("count");
    this.sequence++; this.countSequence++;
    this.active = undefined; this.counting = undefined;
    if (this.cached) this.callbacks.retire(this.cached);
    this.cached = undefined; this.retainedBytes = 0; this.cursors.clear(); this.page = 0;
    this.source = source;
  }

  beginPage(source: string, page: number, force = false): DatasetTicket {
    if (this.disposed) throw new Error("页面控制器已关闭。");
    if (source !== this.source || force) this.reset(source);
    this.cancel("page"); this.cancel("count");
    this.countSequence++; this.counting = undefined;
    const cursor = page === this.page + 1 ? this.cursors.get(page) : undefined;
    const ticket = { source, page, cursor, sequence: ++this.sequence, taskId: crypto.randomUUID() };
    this.active = ticket;
    return ticket;
  }

  isCurrent(ticket: DatasetTicket): boolean {
    return !this.disposed && ticket.source === this.source && ticket.sequence === this.sequence;
  }

  accept(ticket: DatasetTicket, result: QueryResult, nextCursor?: string | null): boolean {
    if (!this.isCurrent(ticket)) { this.callbacks.retire(result); return false; }
    const bytes = new TextEncoder().encode(JSON.stringify(result)).byteLength;
    if (bytes > this.limits.pageBytes) {
      this.callbacks.retire(result);
      throw new Error("DATASET_PAGE_BUDGET_EXCEEDED: 当前页超过前端缓存预算，请减少每页行数。");
    }
    if (this.cached && this.cached !== result) this.callbacks.retire(this.cached);
    this.cached = result; this.retainedBytes = bytes; this.page = ticket.page;
    // 只保存服务端给出的不透明令牌，翻回旧页/任意跳页沿用 offset。
    if (nextCursor) this.cursors.set(ticket.page + 1, nextCursor);
    else this.cursors.delete(ticket.page + 1);
    while (this.cursors.size > this.limits.cursorEntries) this.cursors.delete(this.cursors.keys().next().value!);
    return true;
  }

  finishPage(ticket: DatasetTicket): void {
    if (this.isCurrent(ticket)) this.active = undefined;
  }

  ownsResult(result?: QueryResult): boolean {
    return !this.disposed && result !== undefined && this.cached === result;
  }

  beginCount(context?: ResultContext): DatasetCountTicket {
    this.cancel("count");
    const ticket = { source: this.source, sequence: ++this.countSequence, taskId: crypto.randomUUID(),
      resultId: context?.resultId, revision: context?.revision, connectionGeneration: context?.connectionGeneration };
    this.counting = ticket;
    return ticket;
  }

  isCountCurrent(ticket: DatasetCountTicket): boolean {
    return !this.disposed && ticket.source === this.source && ticket.sequence === this.countSequence;
  }

  finishCount(ticket: DatasetCountTicket): void {
    if (this.isCountCurrent(ticket)) this.counting = undefined;
  }

  async requestCancel(kind: "page" | "count"): Promise<void> {
    const taskId = kind === "page" ? this.active?.taskId : this.counting?.taskId;
    if (taskId) await (kind === "page" ? this.callbacks.cancelPage(taskId) : this.callbacks.cancelCount(taskId));
  }

  dispose(): void { this.reset(""); this.disposed = true; }
  /** React StrictMode 会复用 memo 值执行 setup→cleanup→setup，旧请求仍由序号拒绝。 */
  activate(): void { this.disposed = false; }
}
