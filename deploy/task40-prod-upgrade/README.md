# 正式 lag CPR：新 run 的同 schema52 切换与恢复

本目录用于正式 `/opt/codex-proxy-rs` 单副本的新 run 切换与同 schema52 恢复。已知生产基线 source `a6461769337f83a2a7e41f69be232897c91b9041`、镜像 `ghcr.io/x-t-e-r/codex-proxy-rs@sha256:96c2584487318f721d3ca2ba6b67299c9a17747071578cd3d768b8d1e1a9b5b4`；签发与切换前重新核验。候选 source/image 尚待父级签发

## 为新 run 签发候选

父级在产品 owner 交付后，将 `accepted.example.json` 复制为专属于这次 run 的 manifest，并填写精确的40位产品 source SHA、完整 `ghcr.io/x-t-e-r/codex-proxy-rs@sha256:` digest，以及可追溯的 `migrationEvidence` 与 `acceptanceEvidence`。候选必须确认为 schema52、迁移字节冻结、插件协议不变、相同运行镜像测试与隔离 Admin/恢复验证完成。父级明确批准硬切及同代际在飞请求终结后，将 `accepted`、`hardCutAuthorized` 设为 `true`；不能使用 Task38 的 accepted 文件、旧 GO 或历史收据

父级签发前核对正式容器的 ID、source、实际 image ID、healthy/healthz204、Compose 工作目录和解析后的 `services.codex-proxy-rs.image`、唯一 `.env/CPR_IMAGE` pin、schema52及账号/Key/分组/设置、请求活跃数、磁盘可用空间、重启策略 `no`、watchdog timer/binding/maintenance/锁。源代码与镜像仍须独立匹配：远端精确 digest pull 后，`remote.py` 核对 RepoDigests 包含该 digest、镜像 linux/amd64 和 revision 标签等于 manifest source。切换过程不会写账号配置或关闭业务渠道，也不发模型请求；切换前后数据库为只读查询。保全收据含 PG 内聚合的账号、Key 值摘要与权限、分组/成员关系、代理、配额和插件身份/策略投影；凭据原文不输出，运行中合法变化的请求数、账本增长及凭据刷新观测不做逐项相等约束

控制器进入恢复闸 `lifecycle.lock`、写入该新 run 的 `maintenance`，与 watchdog 互斥；停止旧容器只使用 fresh 核对的具名 ID。已退出容器用固定恢复 helper 的 `--old-container` 与精确退出代际 receipt 终结 PostgreSQL 在飞记录并 CAS 释放相应 Redis 请求槽，最长可等待1800秒（通常约5分钟）。完成后才更改唯一 `.env/CPR_IMAGE` 并经 Compose 单服务重建。失败且命令结果已确认时，停止候选、按候选**自身**退出代际恢复、再 pin 回 96c；候选健康/保全、回退健康/保全及恢复闸实际容器绑定均有收据。终态 `deployed/success=true` 与 `rolled_back_same52/rollback=true` 仅在恢复闸成功重绑、maintenance 清除后写入；绑定失败保留闸并记 `recovery_requires_operator`。stop、恢复 helper 或 Compose up 的命令结果未知时不重发，保留维护闸，待操作员按本 run 收据核实

## 启动一次（仅父级签发与新 GO 后）

先运行不需要 GO 的只读基线入口，并将 JSON 与父级签发证据一起保存。它不会创建远端 run：

```powershell
python -B C:/Programs/AI/MLAIGateWay/outputs/cpr-prod-upstream-20261027/deploy/task40-prod-upgrade/controller.py baseline
```

在 Windows 控制机运行，`--accepted` 指向父本次签发的**独立文件**；`launch` 新建 `task40-<uuid>`，启动脱离当前模型会话的 supervisor 和 worker。以下示例不可直接执行：`<父签manifest>` 是占位符

```powershell
python -B C:/Programs/AI/MLAIGateWay/outputs/cpr-prod-upstream-20261027/deploy/task40-prod-upgrade/controller.py launch --accepted <父签manifest> --GO
```

运行会依序创建远端新目录、pull 精确 digest，并独立启动远端进程；若目录创建、pull 或 launch 确认丢失，不重复下发动作。每个远端 run 使用独立 `attempt-claimed.json` 与 `launch.guard`，进程断开后不依赖本会话。远端锁和 watchdog 锁防止并发生产切换

## 只读观察与恢复决策

记录 `launch` 输出的 `<新run目录>`；本地状态保留签发输入、远端结果与 postcheck。远端两条查询只读取收据及 fresh 校验，不触发切换

```powershell
python -B C:/Programs/AI/MLAIGateWay/outputs/cpr-prod-upstream-20261027/deploy/task40-prod-upgrade/controller.py status --run-dir <新run目录>
ssh lag python3 /opt/codex-proxy-rs/.runtime/deployments/<新run名>/remote.py receipt --root /opt/codex-proxy-rs/.runtime/deployments/<新run名>
ssh lag python3 /opt/codex-proxy-rs/.runtime/deployments/<新run名>/remote.py verify --root /opt/codex-proxy-rs/.runtime/deployments/<新run名>
```

`verify` 仅在 `deployed` 或 `rolled_back_same52` 可用，重新确认健康、source、pin、生效镜像 URL、schema/保全、恢复闸和三把锁不被持有；本地 worker 成功终态会保存 `fresh`。`blocked_unchanged` 表示没有执行 stop，查看 `errorCode` 和当前健康/binding；该状态不承诺 fresh 验证通过

若本地 `worker_exit` 且 `remoteMayHaveStarted=true`，先只读观察远端，不重发 launch。若确认本地 worker 已结束并需要阻止剩余本地动作，在父本次明确恢复授权后运行 `recover-local --run-dir <新run目录> --GO`；该命令只封本地 worker 并读远端收据，不触发 stop、回退或渠道修改

若远端 `recovery_requires_operator`、worker 已退出但只剩 `progress.json`，或回退失败，先只读核本 run 的 `stopCommands`、`recoveryCommands`、`composeCommands`，精确容器 ID/StartedAt、`.env` pin、同 ID 的 `stopped-recovery-<id前12位>.json`、DB running 和 `maintenance` owner。经父级**新的恢复 GO** 后，用同一个 `<新run目录>` 启动一次 detached 接管：

```powershell
python -B C:/Programs/AI/MLAIGateWay/outputs/cpr-prod-upstream-20261027/deploy/task40-prod-upgrade/controller.py takeover --run-dir <新run目录> --GO
```

该入口先等原本地 worker 与远端执行锁结束，以 `takeover-launch-claimed.json` 禁止重复接管。若维护闸仍由本 run 持有，接管按精确代际走回退 96c 路径：stop 返回未知但原 ID 已停止时不再 stop；恢复 helper 返回未知时最长只读等待2100秒，只消费**原退出代际**已 `complete` 的 receipt；Compose up 未知时须先确认新容器的镜像/source、ID/StartedAt 归属，不再次 up。若 worker 在重绑并清除维护闸后、写终态前退出，接管只读复核绑定、健康、pin、目标服务镜像、保全及原恢复 receipt，写出本 run 的终态收据，不再次切换。

若首次接管在发出新动作前被镜像、配置或恢复闸只读检查阻断，`takeover-result.json` 同时记录 `retryableReadOnly=true`、`takeoverCommandsIssued=false`；修正偏差（例如恢复 `.env` 原字节）后，父级可签**新的恢复 GO** 对同一 `<新run目录>` 再执行上述 `takeover` 一次。若首次进程在写出收据前退出，`receipt` 显示 `takeover_pending`；第二次接管仅在远端确认原 PID 已退出、执行与生命周期锁已释放、初次 claim 与原 `progress.json`/源收据字节一致且无残余临时写入时准入。两条路径都重新取得双锁，核旧容器 ID/StartedAt、维护闸 owner、pin、配置和原命令/退出代际 receipt。第二次使用独立 `takeover-retry-*.json` 收据；首次或第二次若已有动作声明、结果未知且无法证明零动作、或容器身份不明，不再重复接管。helper receipt 仍是 `prepared`、stop 未证实退出或恢复闸归属不符时保持当前状态等待人工核实。读取 `receipt` 的 `takeover_pending` 或最终 `takeover-result.json`／`takeover-retry-result.json` 后，再用 `verify` 复核；新的恢复授权不得复用 Task38 receipt、旧实例 ID 或旧 GO

测试（不会连接 lag、Docker 或生产 PG）：

```powershell
python -B -m unittest discover -s deploy/task40-prod-upgrade -p 'test_*.py' -v
```
