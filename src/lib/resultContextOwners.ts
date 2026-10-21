import type { ResultContext } from "@/types/resultContext";

// 页面持有当前结果，草稿持有原结果和恢复预览；这里仅管理寿命，不授予写入权限。
const owners = new Map<string, Set<object>>();
const key = (context: ResultContext) => JSON.stringify([context.resultId, context.revision]);

export function retainResultContextOwner(context: ResultContext | undefined, owner: object): void {
  if (!context) return;
  const reference = key(context);
  const references = owners.get(reference) ?? new Set<object>();
  references.add(owner);
  owners.set(reference, references);
}

export function dropResultContextOwner(context: ResultContext | undefined, owner: object): void {
  if (!context) return;
  const reference = key(context);
  const references = owners.get(reference);
  references?.delete(owner);
  if (!references?.size) owners.delete(reference);
}

export function hasResultContextOwner(context: ResultContext): boolean {
  return !!owners.get(key(context))?.size;
}
