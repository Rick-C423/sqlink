import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { ImportSqlDialog } from "../src/components/connection/ImportSqlDialog";
import { tauriService } from "../src/services/tauriService";
import { useI18nStore } from "../src/i18n";
import type { ImportPlan, ImportPlanRequest, ImportReport } from "../src/types/importJob";
import type { TaskSnapshot } from "../src/types/task";

const originals = { previewImport: tauriService.previewImport, executeImport: tauriService.executeImport, getImportReceipt: tauriService.getImportReceipt, cancelImport: tauriService.cancelImport, getTaskStatus: tauriService.getTaskStatus };
const originalWindow = Object.getOwnPropertyDescriptor(globalThis,"window");
const originalLocale = useI18nStore.getState().locale;
let view: ReactTestRenderer | undefined;
let pick: () => Promise<unknown>;
let requests: ImportPlanRequest[];
let executions: { planId: string; taskId: string }[];
let cancelled: string[][];
let imported: number;
function deferred<T>() { let resolve!: (value:T)=>void; const promise = new Promise<T>(yes=>{resolve=yes;}); return {promise,resolve}; }
function snapshot(id:string):TaskSnapshot {return {protocolVersion:1,taskId:id,queryId:null,connectionId:"A",connectionGeneration:"g",operation:"import",phase:"running",sequence:1,cancelRequested:false,capabilities:{cancellation:true,cancellationReason:null,exclusiveSession:true,resetPolicy:"transactionOnly"},sessionId:"s",startedAt:1,completedAt:null,commitState:"unknown",rollbackState:"notStarted",cleanupState:"notStarted",errorCode:null};}
function plan(request:ImportPlanRequest):ImportPlan {return {planId:"frozen-plan",connectionId:request.connectionId,database:request.database??null,path:request.path,onConflict:request.onConflict,connectionGeneration:"g",fileBytes:80,statementCount:2,warnings:["未验证服务器语法或权限"],expiresAt:Date.now()+30000,task:{...snapshot(request.taskId),phase:"succeeded"}};}
function report(id:string):ImportReport{return {runId:"run-1",planId:"frozen-plan",task:{...snapshot(id),phase:"succeeded"},summary:{statements:2,executed:2,skipped:0,cleanupNotice:"执行成功不等于已提交"},failedStatement:null,sentStatements:2,commitState:"unknown",cleanupState:"succeeded",errorCode:null,message:null,durable:false,recovery:"manualVerification",recovered:false};}
function text(node?:ReactTestInstance):string {if(!node)return JSON.stringify(view!.toJSON());return node.children.map(child=>typeof child==="string"?child:text(child)).join("");}
function button(label:string) {const result=view!.root.findAllByType("button").find(node=>text(node)===label);assert.ok(result,`缺少按钮 ${label}`);return result;}
async function click(label:string){await act(async()=>{await button(label).props.onClick();});}
function dialog(connectionId="A",open=true){return <ImportSqlDialog open={open} connectionId={connectionId} database="main" onClose={()=>{}} onImported={()=>{imported++;}}/>;}
async function mount(){await act(async()=>{view=create(dialog());});await click("选择…");}
beforeEach(()=>{
  requests=[];executions=[];cancelled=[];imported=0;
  pick=async()=>"C:/imports/frozen.sql";
  useI18nStore.setState({locale:"zh-CN"});
  Object.defineProperty(globalThis,"window",{configurable:true,value:{__TAURI_INTERNALS__:{invoke:async(command:string)=>{assert.equal(command,"plugin:dialog|open");return pick();}}}});
  tauriService.previewImport=async request=>{requests.push(request);return plan(request);};
  tauriService.executeImport=async request=>{executions.push(request);return report(request.taskId);};
  tauriService.cancelImport=async(connection,id)=>{cancelled.push([connection,id]);};
  tauriService.getTaskStatus=async id=>snapshot(id);
  tauriService.getImportReceipt=async()=>report("recovered");
});
afterEach(()=>{act(()=>view?.unmount());view=undefined;Object.assign(tauriService,originals);useI18nStore.setState({locale:originalLocale});if(originalWindow)Object.defineProperty(globalThis,"window",originalWindow);else Reflect.deleteProperty(globalThis,"window");});

test("先预检展示冻结计划，开始导入只传 planId 和新 taskId",async()=>{
  await mount();await click("预检文件");
  assert.equal(requests.length,1);assert.equal(executions.length,0);
  assert.match(text(),/未验证服务器语法或权限/);
  await click("开始导入");
  assert.equal(executions.length,1);
  assert.deepEqual(Object.keys(executions[0]).sort(),["planId","taskId"]);
  assert.notEqual(executions[0].taskId,requests[0].taskId);
  assert.equal(imported,1);assert.match(text(),/不能|禁止/);assert.match(text(),/内存记录/);
});

test("来源切换隔离迟到预检和迟到执行结果",async()=>{
  await mount();const pending=deferred<ImportPlan>();
  tauriService.previewImport=request=>{requests.push(request);return pending.promise;};
  let waiting!:Promise<void>;
  await act(async()=>{waiting=button("预检文件").props.onClick();});
  await act(async()=>{view!.update(dialog("B"));});
  await act(async()=>{pending.resolve(plan(requests[0]));await waiting;});
  assert.doesNotMatch(text(),/预检完成/);assert.equal(imported,0);
  await click("选择…");tauriService.previewImport=async request=>plan(request);await click("预检文件");
  const executing=deferred<ImportReport>();tauriService.executeImport=async request=>{executions.push(request);return executing.promise;};
  await act(async()=>{waiting=button("开始导入").props.onClick();});
  await act(async()=>{view!.update(dialog("C"));});
  await act(async()=>{executing.resolve(report(executions[0].taskId));await waiting;});
  assert.doesNotMatch(text(),/执行记录：run-1/);assert.equal(imported,0);
});

test("文件选择器在关闭再打开后迟到不会替换新来源",async()=>{
  await act(async()=>{view=create(dialog());});
  const pending=deferred<unknown>();pick=()=>pending.promise;
  let waiting!:Promise<void>;await act(async()=>{waiting=button("选择…").props.onClick();});
  await act(async()=>{view!.update(dialog("A",false));});await act(async()=>{view!.update(dialog());});
  await act(async()=>{pending.resolve("C:/imports/stale.sql");await waiting;});
  assert.doesNotMatch(text(),/stale.sql/);assert.equal(button("预检文件").props.disabled,true);
});

test("取消请求保持运行中，直到最终回执确认边界",async()=>{
  await mount();await click("预检文件");const pending=deferred<ImportReport>();
  tauriService.executeImport=async request=>{executions.push(request);return pending.promise;};
  let waiting!:Promise<void>;await act(async()=>{waiting=button("开始导入").props.onClick();});
  assert.match(text(),/恢复编号（可复制/);assert.ok(text().includes(executions[0].taskId));
  await click("请求取消");assert.deepEqual(cancelled,[["A",executions[0].taskId]]);
  assert.match(text(),/已请求取消，等待/);assert.equal(button("开始导入").props.disabled,true);
  await act(async()=>{const value=report(executions[0].taskId);value.task.phase="cancelled";value.errorCode="IMPORT_CANCELLED";value.message="已停止后续语句";pending.resolve(value);await waiting;});
  assert.match(text(),/已停止后续语句/);assert.match(text(),/cancelled/);
});

test("更改策略使旧计划失效，必须重新预检",async()=>{
  await mount();await click("预检文件");
  const radio=view!.root.findAllByType("input").find(node=>node.props.type==="radio"&&!node.props.checked)!;
  await act(async()=>{radio.props.onChange();});
  assert.equal(button("预检文件").props.disabled,false);assert.doesNotMatch(text(),/预检完成/);assert.equal(executions.length,0);
});

test("恢复记录仅展示人工核对，新预检隔离迟到查询",async()=>{
  await mount();
  const pending=deferred<ImportReport>();tauriService.getImportReceipt=()=>pending.promise;
  const input=view!.root.findAllByType("input").find(node=>node.props["aria-label"]==="执行记录编号")!;
  await act(async()=>{input.props.onChange({target:{value:"record-id"}});});
  let waiting!:Promise<void>;await act(async()=>{waiting=button("查询执行记录").props.onClick();});
  await click("预检文件");
  await act(async()=>{const value=report("old");value.recovered=true;value.task.phase="unknown";pending.resolve(value);await waiting;});
  assert.match(text(),/预检完成/);assert.doesNotMatch(text(),/执行记录：run-1/);assert.equal(executions.length,0);
});

test("取消失败不显示已取消，未知总字节不显示百分比",async()=>{
  await mount();await click("预检文件");const pending=deferred<ImportReport>();
  tauriService.executeImport=async request=>{executions.push(request);return pending.promise;};
  tauriService.cancelImport=async()=>{throw new Error("取消未确认");};
  tauriService.getTaskStatus=async id=>({...snapshot(id),progress:{stage:"executing",processedBytes:4,totalBytes:null,processedStatements:1}});
  let waiting!:Promise<void>;await act(async()=>{waiting=button("开始导入").props.onClick();});
  await click("请求取消");assert.match(text(),/取消未确认/);assert.doesNotMatch(text(),/已请求取消，等待/);assert.doesNotMatch(text(),/%/);
  await act(async()=>{pending.resolve(report(executions[0].taskId));await waiting;});
});
