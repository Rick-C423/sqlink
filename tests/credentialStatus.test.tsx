import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { CredentialStatus } from "../src/components/connection/CredentialStatus";
import { ConnectionDialog } from "../src/components/connection/ConnectionDialog";
import { credentialNotice } from "../src/lib/credentialStatus";
import { useConnectionStore } from "../src/stores/connectionStore";
import { tauriService } from "../src/services/tauriService";
import { useI18nStore } from "../src/i18n";
import type { ConnectionConfig } from "../src/types/database";
import type { CredentialHealth } from "../src/types/credential";

const originalState = useConnectionStore.getState();
const originalHealth = tauriService.getCredentialHealth;
const originalUpdate = tauriService.updateConnection;
const originalSave = tauriService.saveConnection;
const originalLocale = useI18nStore.getState().locale;
const config: ConnectionConfig = {
  id: "saved-connection", name: "原连接", driverType: "mysql", host: "localhost", port: 3306,
  user: "test", password: "", savePassword: true,
};
const ready: CredentialHealth = { state: "ready", backend: "keychain", message: null };
let view: ReactTestRenderer | undefined;

beforeEach(() => {
  useConnectionStore.setState({ ...originalState, saved: [], treeData: [], credentialHealth: null });
  useConnectionStore.getState().setSaved([{ ...config }]);
  useI18nStore.setState({ locale: "zh-CN" });
  tauriService.getCredentialHealth = async () => ready;
});
afterEach(() => {
  act(() => view?.unmount()); view = undefined;
  tauriService.getCredentialHealth = originalHealth;
  tauriService.updateConnection = originalUpdate;
  tauriService.saveConnection = originalSave;
  useConnectionStore.setState(originalState);
  useI18nStore.setState({ locale: originalLocale });
});

function text() { return JSON.stringify(view!.toJSON()); }
async function mount(element: React.ReactElement) {
  await act(async () => { view = create(element); });
}
async function saveDialog() {
  const save = view!.root.findAllByType("button").find((button) => button.children.some((child) => child === "保存修改"));
  assert.ok(save, "应显示已有连接的保存按钮");
  assert.equal(save.props.disabled, false, "凭据故障不阻止元数据编辑");
  await act(async () => { await save.props.onClick(); });
}

test("凭据IPC读取失败保留连接列表、树节点与密码占位", async () => {
  const before = useConnectionStore.getState();
  tauriService.getCredentialHealth = async () => { throw new Error("模拟IPC故障"); };
  await before.refreshCredentialHealth();
  const after = useConnectionStore.getState();
  assert.equal(after.saved, before.saved);
  assert.equal(after.treeData, before.treeData);
  assert.equal(after.saved[0].password, "");
  assert.equal(after.credentialHealth?.state, "unavailable");
  assert.match(after.credentialHealth!.message!, /已保留当前连接列表/);
});

test("并发状态检查拒绝迟到失败覆盖较新的健康结果", async () => {
  let rejectOld!: (reason: Error) => void;
  const old = new Promise<CredentialHealth>((_, reject) => { rejectOld = reject; });
  let calls = 0;
  tauriService.getCredentialHealth = () => ++calls === 1 ? old : Promise.resolve(ready);
  const first = useConnectionStore.getState().refreshCredentialHealth();
  await useConnectionStore.getState().refreshCredentialHealth();
  rejectOld(new Error("模拟迟到的IPC失败"));
  await first;
  assert.equal(useConnectionStore.getState().credentialHealth, ready);
});

test("所有故障状态给出恢复提示，正常钥匙串无警报，本地密钥说明备份要求", () => {
  assert.equal(credentialNotice(null), null);
  assert.equal(credentialNotice(ready), null);
  for (const state of ["locked", "permission", "unavailable", "corrupt", "missing", "conflict", "unsupported", "io", "migration_pending", "future_state"]) {
    const notice = credentialNotice({ state, backend: null, message: null });
    assert.ok(notice?.failed, state);
    assert.ok(notice.recovery.length > 10, state);
  }
  const local = credentialNotice({ state: "ready", backend: "file", message: null });
  assert.equal(local?.failed, false);
  assert.match(local!.recovery, /备份.*原密钥/);
});

test("真实凭据提示组件展示损坏与恢复说明，不产生凭据写入", async () => {
  let writes = 0;
  tauriService.getCredentialHealth = async () => ({ state: "corrupt", backend: null, message: "旧密钥校验失败，材料已保留" });
  tauriService.saveConnection = async () => { writes++; return "unexpected"; };
  tauriService.updateConnection = async () => { writes++; };
  await mount(<CredentialStatus />);
  assert.ok(view!.root.findByProps({ role: "alert" }));
  assert.match(text(), /凭据或配置校验失败/);
  assert.match(text(), /配套备份恢复原密钥/);
  assert.match(text(), /旧密钥校验失败/);
  assert.equal(writes, 0);
  assert.equal(useConnectionStore.getState().saved[0].name, "原连接");
});

test("编辑已有连接时凭据故障仍能保存元数据，空密码原样交给后端保留密文", async () => {
  let request: ConnectionConfig | undefined;
  let closed = 0;
  tauriService.getCredentialHealth = async () => ({ state: "permission", backend: null, message: "钥匙串锁定" });
  tauriService.updateConnection = async (value) => { request = value; };
  tauriService.saveConnection = async () => assert.fail("编辑不能进入新建凭据路径");
  await mount(<ConnectionDialog open editing={config} onClose={() => { closed++; }} />);
  const name = view!.root.findAllByType("input").find((input) => input.props.value === "原连接");
  assert.ok(name);
  act(() => name.props.onChange({ target: { value: "新名称" } }));
  await saveDialog();
  assert.equal(request?.name, "新名称");
  assert.equal(request?.password, "");
  assert.equal(request?.savePassword, true);
  assert.equal(useConnectionStore.getState().saved[0].name, "新名称");
  assert.equal(closed, 1);
});

test("密码保存失败保留编辑输入与原连接元数据，不伪造成功或关闭编辑框", async () => {
  let closed = 0;
  tauriService.getCredentialHealth = async () => ({ state: "missing", backend: null, message: "原密钥缺失" });
  tauriService.updateConnection = async () => { throw new Error("模拟凭据保护拒绝保存"); };
  await mount(<ConnectionDialog open editing={config} onClose={() => { closed++; }} />);
  const password = view!.root.findAllByType("input").find((input) => input.props.type === "password");
  assert.ok(password);
  act(() => password.props.onChange({ target: { value: "仅测试的合成密码" } }));
  await saveDialog();
  assert.equal(closed, 0);
  assert.equal(view!.root.findAllByType("input").find((input) => input.props.type === "password")?.props.value, "仅测试的合成密码");
  assert.equal(useConnectionStore.getState().saved[0].name, "原连接");
  assert.equal(useConnectionStore.getState().saved[0].password, "");
  assert.match(text(), /模拟凭据保护拒绝保存/);
});
