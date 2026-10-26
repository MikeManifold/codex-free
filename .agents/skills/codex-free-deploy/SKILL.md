---
name: codex-free-deploy
description: "在 macOS 首次部署或迁移 Codex Free 整套服务，包含安全隧道、持久本机签名、LaunchAgent、可选官方 App Server UI 接入，以及更新回滚和故障自救。用户要求在另一台 Mac 安装本仓库、恢复已有安装或验证部署时使用。"
---

# 部署 Codex Free

把仓库中的可复用实现部署到目标 Mac。每台机器保留自己的路径、登录凭证、钥匙串私钥和系统权限；配置模板不含这些值。

## 1. 先确定目标现状

- 从当前仓库或下载包定位 `scripts/bootstrap-macos.py` 和 `scripts/install.config.example.json`；源码带 `Cargo.toml`，编译包带 `release.json`。如果 skill 被单独复制，先找到配套仓库或包；不要把 skill 目录当根目录。
- 核对 macOS、CPU 架构、Python 3、可用端口、目标工作目录。只有源码构建需要 Rust/Cargo。不要要求目录有 Git 或有效 HEAD，也不要为本地操作自动建立 worktree/checkpoint。
- 查看 `~/.codex-free/install.local.json`、运行配置、已有 LaunchAgent、安装二进制和公开签名元数据。只显示必要摘要，不能输出完整配置、环境或凭证。已有服务走第 5 步，不执行首次安装。
- 如果需要 UI，定位目标机实际安装的官方 Codex CLI、官方 `node_repl` 和受信任 CUA 后端。CLI/App 版本和工具会变化，读取该机器现有配置与工具清单，不复制另一台机器的固定工具列表。Chrome 接入也要核对目标机实际可用后端。

## 2. 准备本机配置与身份

1. 下载包先核对来源、外部 ZIP 的 `.sha256` 文件，再核对 `release.json` 的架构、提交与包内 `codex-free` 的 SHA-256，选择它作为源程序，不必安装 Rust。源码模式则执行 `cargo build --release --locked`，选择 `target/release/codex-free`。首次允许下载锁定依赖；依赖齐全后更新入口可离线构建。只在目标机缺少依赖时安装，不替用户升级其他软件。
2. 已有可用隧道/配置就复用。没有时使用选定源程序执行 `quickstart`，按向导创建目标机的安全隧道，运行密钥只在本机安全输入。选择正确工作目录；多个项目选访问根目录。最后选择**保存但不启动**，交给 LaunchAgent 启动。不要重新申请已有上游 OAuth。
3. 默认安装设置可直接用。需要自定义时，将 `scripts/install.config.example.json` 复制到 `~/.codex-free/install.local.json` 后修改，权限设为 600。路径和 serviceLabel 使用目标机值；signingIdentifier 一经部署保持固定，其他 App 使用不同标识。运行配置也留在 `~/.codex-free/`。
4. 按 [本机签名](references/macos-signing.md) 复用或建立完整签名身份，确认私钥 ACL 只允许 `/usr/bin/codesign`。记录所选证书的公开 SHA-1 指纹，私钥始终留在钥匙串。

## 3. 首次安装

在仓库根目录运行，替换参数中的工作目录和公开指纹：

```sh
python3 scripts/bootstrap-macos.py \
  --source /absolute/path/to/selected/codex-free \
  --work-dir /absolute/path/to/workspace \
  --certificate-sha1 PUBLIC_CERTIFICATE_SHA1
```

此时仅预检。需要官方 App Server 时再加 `--codex-cli /absolute/path/to/codex`，入口会创建本机 `bridge.config.local.json`，不会启动旧的 Node 桥接进程。已有 adapter 配置保持不变；检查它是否指向目标机的 CLI。

预检通过且部署已获用户授权后，以相同参数增加 `--apply`。入口先在副本上签名、严格验证，再安装文件和 LaunchAgent；健康检查失败会停止自己的服务并回滚。它拒绝覆盖已有安装，不创建、导出或轮换私钥，也不改变命令执行策略。一次失败先查具体原因，不能循环签名或认证。

官方 UI 后端、浏览器及独立 MCP 属于可选接入：参照 [App Server 配置](../../../integrations/codex-app-server-bridge/README.md) 和现有通用模板逐项接入。凭证缺失只说明哪一项未配置，不复制原机器账户、券商权限或全部上游，也不凭猜测重新授权。

## 4. 验收并收尾

- 检查新服务 PID、`http://127.0.0.1:PORT/health` 的 `status=ok`、隧道就绪和 ChatGPT 连接。健康不等于所有上游已连通，分别报告。
- 用 `codesign --verify --all-architectures --strict` 和 `codesign -d -r-` 检查安装文件：固定 identifier，DR 同时绑定该标识与所选证书，不能只有 cdhash。核对公开元数据匹配证书，回到钥匙串确认私钥 ACL。签名成功不能代替 ACL 验收。
- 通过实际 Free 连接在自有临时目录验证选目录、读取、写入、mkdir、命令和读回；包含普通目录或无提交仓库。删除本次测试文件，不影响邻居项目。
- 若启用 UI，通过 Free 的 `codex_cua_state` / `codex_cua_docs` 读取实际能力，再做可逆的点击、键入、截图及图片返回验证。确认表单转交真人，通过 `codex_confirmation_respond` 继续原操作，不能重发写入。结束后调用 `codex_ui_session_end`，核对该调用者的采集状态释放；另验空闲 5 分钟自动回收。不要终止其他聊天或桌面 App 的共享助手。
- Rust 文件、命令、`local_shell/exec` 救援入口必须在 App Server 不可用时仍可用；子进程退出后按需重建，不重放失败操作。保留签名更新、备份回滚和 LaunchAgent KeepAlive。
- 新机器的首次原生权限可能仍需用户在系统窗口确认。按实际责任链区分数据访问、自动化、辅助功能和屏幕录制；不重置 TCC、不批量申请无关权限、不重签官方 App。

报告实际完成项、未配置项和测试边界。模拟测试、签名副本测试与真实部署升级分别描述；没有目标机现场结果就不能写成部署成功。个人验收记录仅留本机忽略目录，不提交 Git。

## 5. 已有安装的更新与恢复

先核对本机 install 配置与签名 pin，不生成新身份：

```sh
python3 scripts/install-stable-signed.py --source /absolute/path/to/new/codex-free --check
python3 scripts/install-stable-signed.py --source /absolute/path/to/new/codex-free
```

选定新下载包或完成源码构建后，标准更新会备份、签名，验证新文件满足旧 DR，再原子替换并检查新 PID/健康；失败恢复原文件和服务。源码目录也可用 `./scripts/install-local-signed.sh` 构建并更新。下载包的临时签名不能代替目标 Mac 的持久身份；新 Mac 不继承其他机器的 TCC 权限。`--migrate-from-adhoc` 只用于首次对**当前已安装程序的完全相同副本**迁移签名，不夹带功能升级。`install-fallback-ready.py --apply-unrestricted` 另含执行策略变更，不能用于默认部署。

故障先读本机日志和实际注册状态。只重启本安装拥有的组件，给 OAuth 凭证落盘留出正常退出时间。保留备份直到健康和实际工具链验收完成，不能通过强杀、反复登录、全局放宽权限掩盖问题。
