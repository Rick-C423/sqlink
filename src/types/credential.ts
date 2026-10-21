/** 后端只返回保护状态，不返回密钥、密文或密码。未知状态按不可用展示。 */
export interface CredentialHealth {
  state: string;
  backend: string | null;
  message: string | null;
}
