/** 缓存只接收加载器，避免与服务层或连接 store 形成循环依赖。 */
export interface MetadataCacheIdentity {
  connectionId: string;
  connectionEpoch: number;
  connectionGeneration?: string;
  database: string;
  schema?: string | null;
  table: string;
  category: string;
}

interface CacheValue { connectionGeneration: string }
interface CacheEntry<T> {
  base: string;
  connectionId: string;
  generation: string;
  value: T;
  bytes: number;
  expiresAt: number;
}

export class MetadataCache {
  private entries = new Map<string, CacheEntry<CacheValue>>();
  private pending = new Map<string, { connectionId: string; promise: Promise<CacheValue> }>();
  private bytes = 0;

  constructor(private options = { maxEntries: 128, maxBytes: 8 * 1024 * 1024, ttlMs: 30_000 }) {}

  private base(identity: MetadataCacheIdentity): string {
    return JSON.stringify([identity.connectionId, identity.connectionEpoch, identity.database,
      identity.schema ?? null, identity.table, identity.category]);
  }

  private remove(key: string): void {
    const entry = this.entries.get(key);
    if (entry) this.bytes -= entry.bytes;
    this.entries.delete(key);
  }

  invalidateConnection(connectionId: string): void {
    for (const [key, entry] of this.entries) if (entry.connectionId === connectionId) this.remove(key);
    // 删除在途占位；旧请求完成时必须仍持有相同占位才允许重新入缓存。
    for (const [key, entry] of this.pending) if (entry.connectionId === connectionId) this.pending.delete(key);
  }

  clear(): void {
    this.entries.clear();
    this.pending.clear();
    this.bytes = 0;
  }

  async get<T extends CacheValue>(identity: MetadataCacheIdentity, loader: () => Promise<T>, refresh = false): Promise<T> {
    const base = this.base(identity);
    const requestKey = JSON.stringify([base, identity.connectionGeneration ?? null]);
    const now = Date.now();
    for (const [key, entry] of this.entries) {
      if (entry.expiresAt <= now || (refresh && entry.base === base)) this.remove(key);
    }
    if (refresh) {
      for (const [key] of this.pending) if (JSON.parse(key)[0] === base) this.pending.delete(key);
    } else {
      for (const [key, entry] of this.entries) {
        if (entry.base === base && (!identity.connectionGeneration || entry.generation === identity.connectionGeneration)) {
          this.entries.delete(key);
          this.entries.set(key, entry);
          return entry.value as T;
        }
      }
      const pending = this.pending.get(requestKey);
      if (pending) return pending.promise as Promise<T>;
    }
    const slot = { connectionId: identity.connectionId, promise: Promise.resolve(null as unknown as CacheValue) };
    slot.promise = Promise.resolve().then(loader).then((value) => {
      if (identity.connectionGeneration && value.connectionGeneration !== identity.connectionGeneration) {
        throw new Error("结构元数据的后端连接代次已变化，请刷新后重试。");
      }
      if (this.pending.get(requestKey) !== slot) return value;
      const bytes = new TextEncoder().encode(JSON.stringify(value)).byteLength;
      if (bytes <= this.options.maxBytes && this.options.maxEntries > 0) {
        // 同一来源仅保留当前后端连接代次；实际缓存键包含后端 generation。
        for (const [key, entry] of this.entries) if (entry.base === base) this.remove(key);
        const key = JSON.stringify([base, value.connectionGeneration]);
        this.entries.set(key, { base, connectionId: identity.connectionId, generation: value.connectionGeneration,
          value, bytes, expiresAt: Date.now() + this.options.ttlMs });
        this.bytes += bytes;
        while (this.entries.size > this.options.maxEntries || this.bytes > this.options.maxBytes) {
          this.remove(this.entries.keys().next().value!);
        }
      }
      return value;
    }).finally(() => {
      if (this.pending.get(requestKey) === slot) this.pending.delete(requestKey);
    });
    this.pending.set(requestKey, slot);
    return slot.promise as Promise<T>;
  }
}

export const metadataCache = new MetadataCache();
