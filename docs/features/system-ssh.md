# 系统 SSH 配置复用

该功能自 v0.5.0 起提供，v0.8.1 补充了隔离 Science 的 SSH 前置校验桥接。它让隔离 Science 在用户明确授权后，按系统 OpenSSH 语义复用真实 `~/.ssh/config`；它不是 SSH server、端口转发 UI 或公网暴露功能。

## 默认与 opt-in

`reuse_system_ssh` 默认关闭。关闭时，CSSwitch 不把真实系统 SSH 配置注入隔离 Science。

启用时，CSSwitch 会递归枚举真实 `~/.ssh/config` 及其 `Include` 中的具体 Host alias，并在隔离 HOME 的 `.ssh/config` 创建一个 `0600` 普通入口文件。该文件只投影一行具体 alias 和一条指向真实 config 的绝对 `Include`，不复制 `HostName`、用户、端口或连接选项。alias 投影让 Science 的“Add SSH host”选择器可发现系统 host；实际连接仍由下述 wrapper 固定使用真实配置。

## 可发现 alias 与 Compute host

“Add SSH host”中可发现的 alias 和 Compute 列表中已注册的 host 是两个独立状态：

```text
真实 SSH config → CSSwitch alias discovery → Add SSH host picker
                                          ↓ 用户选择
                              Science 自己注册 Compute host
```

CSSwitch 不会把所有 alias 写入隔离 Science `config.toml` 的 `ssh_hosts`，也不会在正常启动时创建 `csswitch-ssh-bridge.v1.json`。新增、更新和删除 Compute host 完全由 Science UI 管理；从 Compute 中 remove 后，CSSwitch 重启不会把它重新注册。CSSwitch 也不会读取或修改 Science 账号数据库来清理历史 provider。

从旧版本升级时，如果存在可证明由 CSSwitch 创建且与当前 `ssh_hosts` 一致的旧 sidecar，启动事务会恢复其中记录的 `original_ssh_hosts`，再删除 sidecar。没有 sidecar 时不会打开或改写 `config.toml`；sidecar、权限、路径或内容不一致时保持原样并 fail closed。

启用后，CSSwitch 在隔离环境 PATH 前放置一个窄 wrapper，最终执行：

```text
/usr/bin/ssh -F <real-home>/.ssh/config <原始参数...>
```

参数仍由调用方交给系统 `ssh`；wrapper 只固定配置文件入口，不实现 SSH 协议，也不读取或显示私钥内容。

## 授权的真实含义

这是一项行为授权，不只是“读一个 config 文件”。系统 OpenSSH 会按原生规则处理：

- `Include`
- `IdentityFile`
- `IdentityAgent`
- `ProxyCommand`
- `Match exec`

这些规则可能进一步访问其他文件、ssh-agent 或本机命令。用户启用前应理解现有 SSH 配置的信任边界。

## 不会做的事

- 不复制或 symlink 整个 `.ssh`，也不复制真实 config 内容；
- 隔离 config 不是指向真实文件的 symlink，避免 Science 写穿真实配置；
- 不把 private key、config 内容或 ssh-agent 数据传到 CSSwitch UI；
- 不自动注册、选择或删除 Science Compute host；
- 不启动 `sshd`，不开启 macOS Remote Login；
- 不修改防火墙或建立 `0.0.0.0` listener；
- 不把 SSH 访问与 CSSwitch inference Gateway 混成同一服务；
- 不保证某个 host、key、agent、ProxyCommand 或网络一定可用。

## 失败边界

默认关闭时，SSH 不是普通 Science 启动的前置条件。用户启用该设置时，CSSwitch 先验证真实 `~/.ssh/config`；SSH 授权状态变化会先停止仍使用旧授权的隔离 Science，再保存新设置。关闭授权会撤销 CSSwitch 管理的隔离 config；若该位置是外来文件、symlink 或特殊文件，CSSwitch 会拒绝覆盖或删除并据实报错。

启用后的每次启动都会再次枚举当前 alias，并校验 managed v2 stub 与 packaged wrapper。alias、config、stub 或 wrapper 缺失/变化/不安全时，旧运行态不能复用，启动会受控重建或 fail closed，不能以 warning 略过。只有 Science 已成功启动后的某次 `/usr/bin/ssh` 命令失败，才只影响该命令。

错误报告不得打印私钥路径、config 内容、ssh-agent 数据或其他敏感信息，也不得为了诊断读取真实 private key。

## 验证层

1. 配置默认关闭；
2. opt-in 保存时缺失 config 会被拒绝；
3. 启用后启动时 wrapper 内容、权限与 config 再次通过 fail-closed 校验；
4. 正常启动不改 `config.toml`、不创建 registration sidecar；
5. fresh Compute 列表为空，但“Add SSH host”可发现 fixture alias；
6. Science UI add 后只注册所选项，remove 后重启不恢复；
7. 隔离 Science PATH 选择 wrapper；
8. wrapper 将参数转给 `/usr/bin/ssh -F`；
9. 没有 `.ssh` 复制、账号数据库读取、`sshd`、防火墙或公网 listener；
10. 特定真实 server 连通性只在单独授权后验证。

其中源码、stub、wrapper 与 legacy 清理合同可由本地 fixture 和系统 OpenSSH 自动验证；第 5～6 项必须在单独授权后使用当前安装版本、临时 outer HOME、临时 data-dir、fixture alias 和动态端口做隔离 UI 验收，不能由 `/usr/bin/ssh -G` 的结果代替。

源码 / 合同测试不能替代真实 Science UI 或真实 server 连通性验收；单个 fixture 或 server 结果也不能泛化为所有用户配置可用。
