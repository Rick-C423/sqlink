<p align="center">
  <img src="src-tauri/icons/app-icon.png" width="128" alt="SQLink 图标" />
</p>

<h1 align="center">SQLink</h1>

<p align="center">
  跨平台桌面数据库管理工具 · Tauri 2 + React + Rust
</p>

<p align="center">
  <img src="https://img.shields.io/badge/MySQL-supported-4479A1?logo=mysql&logoColor=white" />
  <img src="https://img.shields.io/badge/PostgreSQL-supported-4169E1?logo=postgresql&logoColor=white" />
  <img src="https://img.shields.io/badge/SQLite-supported-003B57?logo=sqlite&logoColor=white" />
  <img src="https://img.shields.io/badge/SQL%20Server-supported-CC2927?logo=microsoftsqlserver&logoColor=white" />
</p>

---

## 简介

SQLink 是一个轻量、快速、跨平台的数据库客户端，基于 **Tauri 2**（Rust 后端 + WebView 前端）构建。相比 Electron 方案，安装包更小、内存占用更低、启动更快；相比重量级 IDE，界面更聚焦日常的数据库浏览、查询和编辑工作。

## 界面预览

<p align="center">
  <img src="docs/screenshots/data-grid.png" alt="数据网格：连接树 / 筛选 / 分页 / 行编辑" />
</p>
<p align="center"><i>历史界面截图；当前无筛选表网格按后端授权开放首批类型化编辑，普通 SQL 与筛选结果保持只读</i></p>

<p align="center">
  <img src="docs/screenshots/connection-dialog.png" alt="新建连接对话框：四种数据库类型" />
</p>
<p align="center"><i>新建连接：MySQL / PostgreSQL / SQLite / SQL Server，密码加密存储</i></p>

<p align="center">
  <img src="docs/screenshots/main-window.png" alt="主界面" />
</p>
<p align="center"><i>主界面：简洁的多标签工作台</i></p>

## 支持的数据库

| 数据库 | 驱动 | 说明 |
| --- | --- | --- |
| MySQL / MariaDB | `mysql_async` | 支持 GBK/GB18030 老库列解码 |
| PostgreSQL | `tokio-postgres` | 跨库惰性连接缓存、查询取消（CancelToken） |
| SQLite | `rusqlite`（bundled） | 本地文件库，支持查询中断（interrupt） |
| SQL Server | `tiberius`（TDS 协议） | rustls 加密、信任自签名证书（暂不支持取消查询） |

## 功能特性

P2 03 已将表/视图的连接、数据库、schema 和名称贯穿连接树、页签、列/索引/外键、结构提交及导出。元数据缓存按来源和连接代次隔离；结构提交前重读并核对代次与修订，显式 SQL、导入、刷新和连接生命周期会使缓存失效。验收范围与限制见 [P2 03 元数据身份验收记录](docs/P2-03元数据身份验收记录.md)。现有网格编辑门禁保持不变。

### 连接管理
- 连接树：扁平化连接列表，点击未连接的配置即自动建连
- 连接配置本地持久化，密码 **AES-256-GCM 加密**落盘，永不下发前端
- 密钥优先保存于系统钥匙串；读取失败、损坏或冲突不会触发换钥。旧 `key.bin` 在原密钥回读、旧密文校验及迁移指针持久化完成后才清理；连接树和编辑框显示恢复提示，元数据仍可读取
- 系统数据库（`information_schema` / `master` / `template0` 等）默认隐藏，可通过眼睛按钮或右键菜单切换显示；系统库禁止新建表、导入、删除等危险操作
- 连接颜色标记：标记色直接渲染在服务器图标上，旁边圆点仅表示连接状态（绿=已连接，灰=未连接）
- 连接编辑、断开、删除

### 服务器与用户管理
- **服务器概览**：点击连接树中的服务器节点，右侧展示该实例的数据库列表与登录用户列表
- **用户管理**：查看各数据库的用户/登录名及其权限（MySQL `SHOW GRANTS` 等）
- **可视化权限编辑器**：勾选式全局 / 库级权限管理（SELECT、INSERT、UPDATE、DELETE、DDL 等），保存时自动生成 GRANT / REVOKE 语句；暂不支持可视化管理的驱动回退为只读权限文本

### 数据浏览与编辑
- 表数据网格：分页浏览、行数统计
- 列宽拖拽调整（会话内有效）；悬停被截断的单元格可查看完整内容并一键复制
- 可视化筛选栏：多条件组合（等于 / 包含 / 大于 / 为空等）
- 无筛选表网格已接通类型化新增、修改、删除及提交回执；只有后端确认凭据健康、对象身份、会话状态和类型范围时才开放编辑，生成列保持只读；普通 SQL 和筛选结果保持只读
- 首批范围：SQLite `main` 安全基础表的 INTEGER 亲和、TEXT、BLOB、REAL；PostgreSQL 17 的 `bigint/varchar/text/bytea`；MySQL 8.4 InnoDB 的有符号 `bigint/varchar/varbinary`。出现未开放类型时整表只读；SQL Server、MariaDB、MySQL 5.7 尚未开放网格写入
- 排序按字段身份去重并补齐完整主键；草稿按页面与结果来源保留，离开前可保存、放弃或取消
- 数据和结构独立刷新

### 结构管理
- 新建数据库对话框（按方言生成：MySQL 字符集 / PG 编码 / SQL Server 排序规则）
- 可视化**新建表设计器**：列、索引、外键、主键、默认值、注释，实时 SQL 预览
- 已有表的结构编辑（ALTER），按数据库方言生成对应语句
- 索引 / 外键 / 列注释查看

### SQL 编辑器
- Monaco 编辑器（VS Code 同款）：SQL 高亮、自动补全
- 执行选中 / 执行全部、查询取消（MySQL KILL QUERY / PG CancelToken / SQLite interrupt）
- 执行耗时、影响行数、截断提示
- 查询历史面板：自动记录执行成功的语句（时间/耗时/行数），点击回填编辑器，支持删单条与清空

### 导入导出
- **导入 SQL 文件**：流式拆分大文件（正确处理字符串、注释里的分号），支持"覆盖已存在的表"策略
- **转储 SQL 文件**：整库或单表导出 DDL + INSERT（分批 100 行/条）
- **导出 CSV**：整表导出，不受查询行数上限限制

### MCP 工具
- 内置只读 MCP Streamable HTTP 服务，复用 SQLink 已保存的四类数据库连接
- 支持连接/数据库/表发现、表结构读取、单值查询、分页读取和限量只读 SQL
- 顶栏「MCP 设置」：启停服务、修改端口（保存即热生效）、按工具启用/禁用、按连接控制访问
- 仅监听本机回环地址，使用随机 Bearer Token 鉴权，并限制 SQL 类型、行数、超时与响应体积
- 连接与工具参数详见 [MCP 工具文档](docs/MCP工具.md)

### 界面体验
- **中英双语界面**：右上角一键切换 中文 / English
- 深色 / 浅色主题一键切换，Windows 系统标题栏随主题联动变色
- 浅色主题以白底浅蓝为主，深色主题为纯灰阶
- 工作台多标签页：标签栏空间不足时先压缩标签，压缩到极限后出现左右移动按钮
- 标签右键菜单：关闭 / 关闭其他 / 关闭全部
- 全局禁用 WebView 默认右键菜单，应用内右键均为功能菜单

## 技术栈

| 层 | 技术 |
| --- | --- |
| 后端 | Rust · Tauri 2 · tokio · async-trait |
| 前端 | React 18 · TypeScript · Vite · Tailwind CSS · Zustand · Monaco Editor |
| 驱动 | mysql_async · tokio-postgres · rusqlite · tiberius |
| 安全 | AES-256-GCM（连接密码加密存储） |

后端采用 trait 驱动的多数据库抽象（`DatabaseDriver`），每种数据库一个独立驱动文件，新增数据库类型只需实现同一套接口。

## 下载安装

不想自己编译？直接去 [Releases](https://github.com/Rick-C423/sqlink/releases) 下载最新版本：

| 平台 | 产物 | 说明 |
| --- | --- | --- |
| Windows | `SQLink_<版本>_x64-setup.exe` / `.msi` | 安装包，安装后出现在开始菜单 |
| Windows | `SQLink-<版本>-portable.zip` | 解压直用便携版，解压后运行 `SQLink.exe` |
| macOS | `aarch64` / `x64` 的 `.dmg` | 分别对应 Apple Silicon 和 Intel；未签名，首次打开请右键 → 打开 |
| Linux | `.deb` / `.AppImage` / `.rpm` | 按需选择发行版对应格式 |

各平台安装包由 GitHub Actions 自动构建（见 `.github/workflows/release.yml`，打 `v*` 标签即触发三平台矩阵构建）。连接配置都存放在用户目录，安装版与便携版互不冲突。

## 快速开始

### 项目文档

| 文档 | 用途 |
| --- | --- |
| [项目交接文档](docs/项目交接文档.md) | 项目现状、架构、风险、负责人和接手清单 |
| [构建与发布操作手册](docs/构建与发布操作手册.md) | 开发、单 EXE 构建、安装包、便携版、校验和发布操作 |
| [对接文档](docs/对接文档.md) | Tauri IPC、数据模型和数据库驱动二次开发 |
| [MCP 工具](docs/MCP工具.md) | MCP 客户端接入、工具参数、安全限制和排障 |
| [P1 实库验收记录](docs/P1实库验收记录.md) | 本轮数据库版本、真实提交测试与剩余验证范围 |
| [P1 桌面验收记录](docs/P1桌面验收记录.md) | 独立配置目录的桌面验收进度与结果 |

### 环境要求

- [Node.js](https://nodejs.org/) 18+
- [Rust](https://rustup.rs/) 1.85+（内置 MCP SDK 使用 Rust 2024 edition）
- Tauri 2 系统依赖（Windows 需 [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/)，一般系统自带）

### 开发模式

```bash
# 安装前端依赖
npm install

# 启动开发模式（Vite 热更新 + Tauri 窗口）
npm run tauri dev
```

### 构建发布

```bash
# 产出安装包（Windows 为 .msi / .exe NSIS 安装器）
npm run tauri build
```

构建产物位于 `src-tauri/target/release/bundle/`。

### 仅编译后端

```bash
cd src-tauri
cargo build          # debug
cargo build --release
cargo test           # 运行单元测试（SQL 导入拆分器等）
```

### 回归测试

```bash
npm test             # 来源隔离、类型化网格、只读边界、凭据、草稿及真实 React 组件交互回归
npm run build        # TypeScript 类型检查与生产构建
cargo test --manifest-path src-tauri/Cargo.toml
```

前端测试使用 Node 内置测试运行器及 React 18 test renderer，无需数据库；组件测试使用真实组件与状态 store，数据库服务仍为 mock，不能替代桌面实库验证。Rust 默认实测 SQLite；网络数据库需要配置 `SQLINK_TEST_MYSQL`、`SQLINK_TEST_POSTGRES`、`SQLINK_TEST_MSSQL`。本地未配置时旧网络集成用例会提前返回，不计作实库验证。

本轮工作区验证状态（2026-10-20）：P2 02 流式原值导出、原子文件替换与有界单租约导入见[验收记录](docs/P2-02流式导出验收记录.md)；P2 01 取数预算部分见[此前记录](docs/P2-01取数预算验收记录.md)。普通 SQL 大整数显示修复此前通过 145 项前端及生产构建，另有 3 项新增 Rust 单元、3 项真实 SQLite 及 5 项前端回归，详见[修复记录](docs/普通查询大整数修复记录.md)。此前 MySQL **8.4.11**、PostgreSQL **17.11** 合计 8 项完整提交/门禁网络测试已真实通过，覆盖 CRUD 精确值、冲突整批回滚、对象重建、持有会话权限变化及约束检查复用；Windows 专用随机钥匙串条目的迁移测试 1 项通过。MySQL 8.4/5.7、PG 17 的取消、隔离和连接复用已实测通过。MySQL 8.4、PG 17、SQLite 的独立配置桌面自测及外部读回已通过，具体范围和保留限制见桌面记录；SQL Server 用户明确无测试实例，未验收并继续只读。这些记录描述本地工作区验证，不代表用户已经验收或版本已经发布。

普通及类型化查询已逐行施加 10,000 行、16 MiB 结果字节与 1 MiB 原始单元格预算。超限拒绝整行，不裁剪值；网络驱动隔离未读会话，截断表页面不发布编辑身份。字节预算计入显示与 typed 原值的持有及编码费用估算，不是进程 RSS 或协议层单行瞬时分配的硬上限。CSV/SQL 文件导出已由 P2 02 改为原值流式写盘；元数据专用路径的完整改造仍列 P2 03。

可信结果使用协议 v1，前端只提交结构化表读取参数，后端固定连接代次、完整来源、列映射及真实键；定位键映射和原行快照保存在后端。句柄按结果与连接隔离，关闭回收，过期或结构变化后拒绝复用。受控读取新增独立 `typedRows` 通道：int64/uint64/decimal 保留十进制文本，binary 使用 Base64，SQL NULL 与无法无损读取的值明确区分。普通 SQL 和筛选仍只读；其标量整数在传输时保留安全范围 Number，超出范围则使用精确十进制字符串，不参与行身份或编辑授权。

P1 02 已实现四驱动原生参数绑定和 `changeset_validate`：变更仅引用原结果、列 ID 和行句柄，预检返回 `executable:false`，不会执行 DML。SQLite 不提供原生精确 decimal；MySQL TIMESTAMP 时区语义、SQL Server 部分时间与编码类型仍保守拒绝。P1 04 的受控提交协调器与凭据保护已接入公开提交路径，首批开放范围由后端逐次核验；底层支持绑定的类型不等于已开放网格写入。详细类型边界见 [对接文档](docs/对接文档.md)。

P1 03 新增任务注册表和独占会话。编辑器单次执行与“执行全部”固定任务 UUID、连接代次和租约；取消请求与成功/失败/已取消/未知终态分开。批次独占物理会话直到提交或回滚，已确认 COMMIT 不会因后续清理中断而改报未提交；Drop/abort/panic 清理失败会隔离连接。SQLite 在专用线程执行，独立中断句柄不等待查询连接锁；MySQL 健康会话重置后复用，PG/SQL Server 当前使用独立短连接。SQL Server 取消明确禁用。单次执行结束会清理未提交事务，不能依赖多次点击之间保留 BEGIN；事务脚本应在同一次“执行全部”中完成。

SQLite 的 `resetPolicy=transactionOnly` 保留既有 TEMP/ATTACH/PRAGMA 逻辑连接状态，其余驱动租约为 full。SQL 导入全文件固定租约，单行/单条限 16 MiB，仍可能部分提交；先预检再执行冻结的计划，支持任务取消及按恢复编号查询持久执行记录。已经发送 SQL 的中断须人工核对，不按语句编号自动重放。导出接入任务阶段、取消和原子文件替换。独占租约基础设施不等于网格写入已获授权。

表浏览首屏完成数据和可信身份核验后即展示，不等待精确 COUNT；总数未知时仍可翻页，统计为按需独立且可取消的任务。分页控制器只缓存当前页和有界续页令牌，保留 offset 跳页；完整主键、非空整数/布尔排序的受控基础表可使用 keyset 连续续页，其余保守回退。各页与统计分别读取，不承诺跨页一致快照。网格编辑继续沿用 P1 已验证的数据库、对象和类型门禁。

P1 04 新增 `changeset_commit/changeset_receipt` 及有界进程内回执。内部协调器在同一写事务中锁住对象、比较后端对象指纹、锁定全部原行并核对完整类型快照，再执行参数化变更。删行、改行、无变化、多行命中、约束错误、提交回执丢失和清理中断分别处理；提交成功或进入提交未知后消费旧结果身份，同 ID 不重放，也不能更换变更 ID 重用未知结果的上下文。前端保存状态保留原请求和回执，已提交但刷新失败只重试读取。公开入口核对凭据健康、后端登记的 `allowedOperations`、列授权及同租约对象证据，不接受请求传入授权开关；旧拼 SQL 网格入口继续拒绝。SQLite 写入显式使用 `INSERT/UPDATE OR ABORT`，并按真实 affinity 检查类型，避免列级冲突策略或隐式类型转换破坏批次原子性与精确值。

对象核验仍有明确能力范围：SQLite 仅 `main` 基础表，要求无触发器且外键/约束相关 PRAGMA 安全，检查写事务、目录版本和同连接 DDL 代次，不承诺外部恶意重置目录版本的检测。PostgreSQL 当前仅 17，分区、继承、临时表、触发器、规则和 RLS 等对象继续拒绝。MySQL 网格当前仅 8.4 InnoDB：核对当前账号对精确 schema 或表的直接 `TRIGGER` 授权，显式 `USE` 后读取触发器目录；仅“可见触发器数为零”或全局授权不能构成证明。禁用 `foreign_key_checks/unique_checks`、非严格 SQL mode、字符集不兼容或存在触发器时拒绝写入。SQL Server 网格仍关闭，不能用 SQLite、MySQL 或 PostgreSQL 的验收替代。

`cargo test --manifest-path src-tauri/Cargo.toml --test result_context` 执行真实 SQLite 上下文集成；三个网络数据库的新用例位于 `result_context_network`，默认显式忽略。配置对应专用测试库后，可选择例如 `cargo test --manifest-path src-tauri/Cargo.toml --test result_context_network postgres_controlled_result_context -- --ignored --exact`；缺配置或连接失败会失败，并只创建、清理本次随机测试表。

`cargo test --manifest-path src-tauri/Cargo.toml --test typed_binding_sqlite --test changeset` 验证真实 SQLite 参数回环及预检零写入。类型绑定网络测试位于 `typed_binding_mysql_network` 和 `typed_binding_network`，默认显式忽略；例如配置专用 PG 测试库后运行 `cargo test --manifest-path src-tauri/Cargo.toml --test typed_binding_network postgres_native_typed_binding_roundtrip -- --ignored --exact`。缺配置会失败，不能将编译通过或忽略计作实库通过。

`cargo test --manifest-path src-tauri/Cargo.toml --test session_sqlite --test query_tasks --test import_session` 覆盖真实 SQLite 事务隔离、排队/执行中取消、旧句柄拒绝、abort 后复用和导入连续性。网络租约测试在 `session_pg_mysql/session_mssql`，默认显式忽略；专用 PG 配置后可运行 `cargo test --manifest-path src-tauri/Cargo.toml --test session_pg_mysql postgres_session_isolation_cancellation -- --ignored --exact`。MySQL/SQL Server 需各自真实环境，不能用 SQLite 或 mock 代替。

`cargo test --manifest-path src-tauri/Cargo.toml --lib commit` 覆盖真实 SQLite 提交及回执故障注入；`--test commit_identity_sqlite` 覆盖同名重建、别名和事务对象保护。三驱动目录身份测试在 `commit_identity_network`，默认 ignored；显式运行缺配置必须失败。

新增 PostgreSQL 取消清理实库回归为显式忽略项，设置 `SQLINK_TEST_POSTGRES` 后执行 `cargo test --manifest-path src-tauri/Cargo.toml postgres_cancel_cleanup -- --ignored`；该命令缺少配置或连接失败会直接失败。PR/main 检查在 `.github/workflows/ci.yml` 中配置了前端、SQLite、PostgreSQL 17 和 MySQL 8.4 测试；SQL Server 必测 CI 服务仍待补齐。本轮工作流仅完成本地静态核对，需推送后在 GitHub 实际运行才能记为 CI 通过。

## 项目结构

```
sqlconnect/
├── src/                        # 前端（React）
│   ├── components/             # 界面组件
│   │   ├── connection/         # 连接树、连接/建库/导入/导出对话框
│   │   ├── editor/             # Monaco SQL 编辑器
│   │   ├── result/             # 结果网格（按后端授权开放类型化编辑）
│   │   ├── ServerOverview.tsx  # 服务器概览（数据库 / 用户）
│   │   ├── UsersView.tsx       # 用户与权限管理
│   │   └── PrivilegeEditor.tsx # 可视化权限编辑器
│   ├── i18n/                   # 中英文字典与语言切换
│   ├── lib/                    # DDL 语句生成（四方言）、工具函数
│   ├── stores/                 # Zustand 状态（连接/编辑器/工作区/主题）
│   └── services/               # Tauri IPC 封装
├── src-tauri/                  # 后端（Rust）
│   └── src/
│       ├── database/           # DatabaseDriver trait + 四种驱动实现
│       │   ├── mysql.rs
│       │   ├── postgres.rs
│       │   ├── sqlite.rs
│       │   ├── mssql.rs
│       │   └── dump.rs         # SQL 流式拆分 / INSERT 批量生成
│       ├── commands/           # Tauri 命令（连接/查询/导入导出/窗口）
│       └── store.rs            # 连接配置加密持久化
└── src-tauri/icons/            # 应用图标（全平台尺寸）
```

## 路线图

- [ ] SQL Server 查询取消
- [x] 查询历史（成功语句按连接记录，面板回填/删除/清空）
- [ ] SQL 收藏
- [ ] 表数据导入（CSV → 表）
- [ ] ER 关系图
- [ ] 更多数据库（Oracle / Redis / ClickHouse…）

## License

MIT
