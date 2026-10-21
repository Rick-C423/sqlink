import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { tauriService } from "../src/services/tauriService";
import { releaseGridDraft, useGridDraftStore } from "../src/stores/gridDraftStore";
import type { QueryResult } from "../src/types/database";
import type { ResultContext } from "../src/types/resultContext";

const originalRelease = tauriService.releaseResultContext;
let released: [string, number][];
beforeEach(() => {
  released=[];
  useGridDraftStore.setState({drafts:{},navigation:null});
  tauriService.releaseResultContext=async (resultId,revision)=>{released.push([resultId,revision]);};
});
afterEach(()=>{tauriService.releaseResultContext=originalRelease;});

function result(id:string):QueryResult {
  const resultContext: ResultContext = {protocolVersion:1,resultId:id,revision:1,connectionId:"A",connectionGeneration:"generation",source:{database:"db",schema:"main",table:"t",kind:"table"},columns:[],editPolicy:{allowedOperations:[],reasonCodes:[]},createdAt:0,expiresAt:100};
  return {columns:[],rows:[],executionTimeMs:1,resultContext};
}

test("关闭干净草稿回收对应后端结果；脏草稿不会被释放",()=>{
  const store=useGridDraftStore.getState();
  store.ensureDraft("a",result("old"));
  store.updateDraft("a",()=>({edits:{"0:0":"输入"}}));
  releaseGridDraft("a"); assert.deepEqual(released,[]);
  store.discardDraft("a"); releaseGridDraft("a");
  assert.deepEqual(released,[["old",1]]);
});

test("草稿钉住旧上下文，放弃并接受新结果后才回收旧身份",()=>{
  const store=useGridDraftStore.getState();
  const old=result("old"); const next=result("next");
  store.ensureDraft("a",old); store.updateDraft("a",()=>({newRows:[{}]}));
  store.ensureDraft("a",next); assert.deepEqual(released,[]);
  assert.equal(useGridDraftStore.getState().drafts.a.sourceResult,old);
  store.discardDraft("a"); store.ensureDraft("a",next);
  assert.deepEqual(released,[["old",1]]);
});

test("其他页面仍引用同一结果时保留，最后一个页面关闭才释放",()=>{
  const store=useGridDraftStore.getState(); const shared=result("shared");
  store.ensureDraft("a",shared); store.ensureDraft("b",shared);
  releaseGridDraft("a"); assert.deepEqual(released,[]);
  releaseGridDraft("b"); assert.deepEqual(released,[["shared",1]]);
});
