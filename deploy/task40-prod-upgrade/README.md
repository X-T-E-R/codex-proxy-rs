# Task40：单副本切换与受检恢复

本目录用于正式 `/opt/codex-proxy-rs` 的新 run：支持同 schema52 切换，以及在线准备索引后的 schema52→53 切换。Windows 控制器独立持锁运行，远端进程保存逐动作收据；候选失败时按本 run 的 `baseline` 恢复。以下命令从载体仓库根目录执行，生产写入均须父级签发输入及对应的新 GO

## 签发精确输入

先读取 fresh 基线，不需要 GO，也不创建远端 run：

```powershell
python -B deploy/task40-prod-upgrade/controller.py baseline
```

父级将输出中的 `baseline` 原样写入独立 manifest，绑定现役的 `source`、不可变 `image`、`containerId` 和 `startedAt`，再填写候选精确40位 source、完整 digest URL、`migrationEvidence` 和 `acceptanceEvidence`。精确镜像须为 linux/amd64，RepoDigests 包含该 URL，revision 标签等于 source；本地精确镜像已存在时核身份后使用，仅确认不存在时 pull

- 同 schema52：使用 `accepted.example.json`，迁移字节冻结、插件协议不变，同镜像启动、Admin、恢复及功能验收完成后签发
- schema52→53：使用 `accepted.schema53.example.json`，`source` 与 `schema53Contract.source` 必须一致，合同为 `cpr-successful-trace-index/3`。`schema53Contract.artifacts` 绑定产品 owner 的五份原文字节，`evidence` 指向该合同及同镜像验收。SQL 原文位于 `schema53-contract/`，不改写索引定义或迁移身份；已有 `/2` run 保留其冻结工件

父级批准硬切及同代际在飞请求终结后，才将 `accepted`、`hardCutAuthorized` 设为 `true`。每个新 run 重新取基线及授权，已终结 run 的 manifest、GO 和收据不重放

切换准入还核对 Compose 工作目录及目标服务解析后的镜像、唯一 `.env/CPR_IMAGE` pin、schema、至少2 GiB可用空间、重启策略 `no`、watchdog timer enabled/active、binding、maintenance 和执行锁。保全投影包含账号/Key身份与权限、分组/成员、运行设置、代理、配额、插件身份/策略；凭据原文不输出。请求统计、账本增长和凭据刷新观测等合法运行变化不做逐项相等约束

## schema53：在线准备，再授权短切

schema53 只接受 `admissionMode: "healthy"`。准备阶段可以记录现役的健康状态，真正切换仍须 fresh Docker healthy 且 `/healthz` 为204

1. 父级签发准备 GO，创建新 run 并投递原文工件：

   ```powershell
   python -B deploy/task40-prod-upgrade/controller.py prepare --accepted <父签schema53-manifest> --GO
   ```

   记录返回的 `<新run目录>`。`prepare` 不停止应用、不写 maintenance，也不在慢预建期间占用生命周期锁。产品 SQL 在同一 psql session 中持 SQLx advisory lock，仅对 missing 索引顺序执行 SET120秒 → CIC → SET5秒；前后检查、valid-noop、cleanup、revert 仍为5秒语句，锁等待均为500毫秒。prebuild 的进程传输等待为180秒，容纳120秒CIC及连接/短检查；cleanup、revert 为30秒。传输中断仍记 unknown。准备成功要求精确 `schema52 + valid`，不登记53

2. 观察准备收据及数据库现状：

   ```powershell
   python -B deploy/task40-prod-upgrade/controller.py status --run-dir <新run目录>
   python -B deploy/task40-prod-upgrade/controller.py preparation-status --run-dir <新run目录>
   ```

   `preparation-status` 只读核现役、schema和索引，写独立 `prepare-observed.json`，保留原 `prepare-result.json`。未知结果已完成且事实为 `schema52 + valid` 时可据此继续；`active_operations` 非空时只观察，不重放原动作

   `invalid` 或 `drop_incomplete` 须经父新的清理 GO：

   ```powershell
   python -B deploy/task40-prod-upgrade/controller.py cleanup --run-dir <新run目录> --GO
   ```

   原文 cleanup 仅接受精确52、同 SQLx 锁、空 `active_operations` 和 `missing/invalid/drop_incomplete`，成功为 `schema52 + missing`。有效索引与同名异义对象受到保护。清理后另建新 prepare run，不重复本 run 的已声明预建；`unexpected`、`mismatch` 或观察失败先交人工核实

3. `schema53_prepared` 经 fresh 只读核实后，父级签发新的切换 GO：

   ```powershell
   python -B deploy/task40-prod-upgrade/controller.py activate --run-dir <新run目录> --GO
   ```

   此时才持生命周期锁、写 maintenance、停止精确旧代际并完成其 PG 请求终结/Redis CAS 槽恢复，再 pin 候选并单服务重建。候选自身 Migrator 校验有效索引并登记53；放行要求精确 `schema53 + valid`、fresh 健康、完整保全和 watchdog 绑定重写。原准备、清理收据与 `supervisor-<operation>.json` 分阶段保存

每次 schema53 GO 会先写 `pendingOperation` 并把该动作绑定到 supervisor/worker；前一阶段待启动、执行或发布收据时，下一 GO 会被阻断。supervisor 在同一全局锁内先保存独立阶段收据、再更新 `supervisor.json` 摘要，最后清除 pending。收据的 `localLocks` 记录发布时 worker 已退出、controller 仍持锁；当前锁状态由 fresh verify 核实。pending 留存时先观察收据及原进程，不重复启动

`55000` 为产品前提不符，`55P03` 为忙或锁超时，`57014` 为语句超时；失败后先 inspect 再决定新的授权。进程/传输失败、缺少最终单份JSON均记 unknown，每个动作仅声明并下发一次

准备或清理的前置阻断保存 `preparationStage`、`preparationFailure` 的异常类型/errno及 `zeroSqlConfirmed`，不保存异常正文。生命周期锁忙为 `restart_gate_lifecycle_busy`，部署执行锁忙为 `schema53_deployment_busy`；两者均保留 claim 和独立失败收据

原 run 已结束且同角色终态明确 `schemaWrites=0`、没有 `schemaCommands` 时，父级可签 fresh `/3` manifest 及新的恢复 GO：

```powershell
python -B deploy/task40-prod-upgrade/controller.py recover-preparation --run-dir <原run目录> --accepted <父签新schema53-manifest> --GO
```

该入口仅为原 `prepare` 或 `cleanup` 创建同角色的新 linked run，返回的目录用于后续 status。新 manifest 必须绑定相同候选与精确旧代际；控制器派生 `preparationRecovery`，绑定原 run、角色及终态收据字节身份。原 PID 必须已退出、没有激活声明或临时收据、执行锁可得；执行前重新核原收据、全条祖先链、现役、schema52及空 `active_operations`。历史收据缺少诊断字段时仍按原计数/Commands/claim核实，不推测异常原因

本地和远端的唯一后继声明放在原目录之外，原 run 的 journal、SQL、claim及失败收据全部保留。后继已预留后，原 run 不再 activate、cleanup 或再派第二后继；provision/launch确认丢失只观察该子 run。子 run 若也有明确零SQL终态，可另签 GO 从该子 run 接续；已有 SQL 声明、unknown、缺终态或仍有进程时保持观察，不借祖先 run 重发。新 cleanup 成功后仍须另签新 prepare run，prepare 成功后再签 activate GO

## 同 schema52：一次授权切换

签发 `accepted.example.json` 对应的独立输入后执行：

```powershell
python -B deploy/task40-prod-upgrade/controller.py launch --accepted <父签schema52-manifest> --GO
```

`launch` 新建 `task40-<uuid>`，启动脱离当前会话的 supervisor/worker，再独立启动远端进程。目录创建、pull 或 launch 确认丢失后不重发。每个远端 run 使用独立 claim 与执行锁，同 watchdog 生命周期锁互斥

## 切换失败与接管

恢复始终绑定退出容器的完整 ID 和 StartedAt。未声明 `recoveryHelper` 的原合同仍固定使用 source `476d24c16764a0a535fabd975a3df18d2192df0a`、image `f8c705a4da6687ea39902bb09e020772a4083d2dae12c1d0cd507952bf88944e`，须保留其原 host 与 binding，不能就地替换已完成 run 的工件。

新 host 从候选 `deploy/site-recovery.py` 独立安装为 `.runtime/restart-recovery/site-recovery-confirmed-v1.py`，保留原 `site-recovery.py`。binding 的 `recoveryCommand` 必须使用这个新路径，安装与绑定变更另需父级站点授权。

新批次 host/helper 须在父签 manifest 中显式增加 `recoveryHelper: {"source": "<新helper完整提交>", "image": "<新helper完整digest URL>", "contract": "cpr-recovery-confirmed/1"}`。站点 binding 同时绑定 `helperSource`、`helperImage`、`helperContract`；其 `recoveryCommand` 追加同一精确 `--helper-source`、`--helper-image`，`--redis-helper-command-json` 中也必须是该镜像的 `docker run ... site-recovery-redis`。仅有新 host 或仅有新镜像均不能准入。

切换前检查 binding 与父签输入、镜像 revision/RepoDigest 以及 host `--describe` 能力；停止后 host 再在 PG 写入前以正常 Store 认证核完整能力，拒绝旧 helper，不回落逐条路线。非空恢复 manifest 仅启动两个 helper 容器：认证一次、PG 精确确认后有界释放一次。预算、CAS 和 batch unknown 规则见[单部署 pre-start 恢复](../README.md#单部署-pre-start-恢复)。PG 使用既有认证 psql，Redis-only helper 不运行 Migrator、HTTP 或 Worker；成功重绑后的 watchdog 仍沿已准入的同一入口恢复并 start 原容器

候选失败且命令结果已确认时，先停候选、恢复候选**自身**退出代际，再回退。schema53 run 此时执行产品原文 `revert.sql`：完整核对 ledger/checksum，仅撤53登记，保留 valid 索引及业务数据；确认 `schema52 + valid` 后才启动 `baseline.image`。旧52镜像不能直接在仍登记53的库上启动

`deployed/success=true`、`rolled_back_same52/rollback=true` 仅在健康/保全通过、恢复闸重绑且 maintenance 清除后写入。stop、helper、Compose 或 schema 写动作结果未知时保留维护闸，按本 run 收据只读核实：

```powershell
python -B deploy/task40-prod-upgrade/controller.py status --run-dir <新run目录>
ssh lag python3 /opt/codex-proxy-rs/.runtime/deployments/<新run名>/remote.py receipt --root /opt/codex-proxy-rs/.runtime/deployments/<新run名>
ssh lag python3 /opt/codex-proxy-rs/.runtime/deployments/<新run名>/remote.py verify --root /opt/codex-proxy-rs/.runtime/deployments/<新run名>
```

`verify` 在成功部署/回退后重新核健康、source、pin、目标服务镜像、目标schema、保全、恢复闸及三把锁。`blocked_unchanged` 表示未执行 stop，按 errorCode 核健康与绑定

本地 worker 退出且 `remoteMayHaveStarted=true` 时先观察远端；`schemaMayHaveStarted=true` 则读准备/清理收据。需要封住本地剩余动作时，由父新的授权执行 `recover-local --run-dir <新run目录> --GO`，随后该 run 不再 activate

远端 `recovery_requires_operator`、只剩 progress 或回退失败时，核本 run 的 stop/recovery/compose/schema Commands、精确容器退出代际、pin、同 ID 恢复receipt和 maintenance owner。父级新的恢复 GO 才能启动独立一次接管：

```powershell
python -B deploy/task40-prod-upgrade/controller.py takeover --run-dir <新run目录> --GO
```

接管先等执行锁释放。stop 未知但原代际已退出时不再 stop；helper 未知只读等待原代际 `complete` receipt，最长2100秒；Compose up 未知时先证明替代容器归属，不再次 up。原 worker 已重绑解闸但未写终态时，仅复核并补终态

schema revert unknown 时先核原 SQL session 已退出：已经是精确52可继续启动旧版，仍为53则保留维护闸且不重发。已知 SQLSTATE 失败可在 fresh `schema53 + valid`、无 active_operations 及新接管 GO 下，独立声明一次 `rollback_takeover`；原失败收据保留

首次接管在新动作前仅被只读检查阻断时，`retryableReadOnly=true` 且 `takeoverCommandsIssued=false`；修正偏差后父可签新 GO 再接管一次。首次接管只剩 `takeover_pending` 时，还须证实原 PID 已退出、双锁已释放、claim 与原progress/收据一致且无临时写入。第二次使用 `takeover-retry-*`；已有动作声明或无法证明零动作时保持维护闸，交人工核实

## 精确磁盘事故的旧52准入

`disk_logging_recovery` 仅供 `accepted.example.json` 所列427/35c、55c0及其 StartedAt 的精确旧代际。读取时用 `baseline --mode disk_logging_recovery`，父签 `degradedEvidence` 并接受 `noCurrentComponentSnapshot` 的观测范围后才可 launch

该入口逐次核对本代际恰好10条已知health故障（最后一条为 `2026-10-31T19:34:31.095157Z`）、PG和五worker各自晚于故障的恢复事件、Redis typed只读SCAN及PG只读查询。结构日志要求轮转连续、尾部不超过15秒、解压总量不超过128 MiB；新故障、缺失、不同原因或诊断入队失败均阻断。它只改变该旧代际准入；候选及回退一律要求 fresh Docker healthy、healthz204、保全及重绑

## 隔离验收

部署模拟不连接 lag、Docker 或生产PG：

```powershell
python -B -m unittest discover -s deploy/task40-prod-upgrade -p 'test_*.py' -v
```

同镜像检查使用独立 Compose project、内部网络、随机本机端口与专属数据卷。旧镜像固定90497/a4db，候选source须匹配合同；检查在线准备、真实53启动、固定helper在53下恢复/同容器重启、受检撤53、旧镜像重启和数据保全。prepare/revert各在真实SQL完成后注入回执丢失，要求消费者归为unknown、仅inspect收敛且不重发，不发送provider请求：

```powershell
python -B .github/scripts/task40-schema53-smoke.py --image <候选完整digest-URL> --source <候选40位source> --output .runtime/task40-schema53.json
```

Task40 workflow 的 `workflow_dispatch` 填 `verify_image` 和 `verify_source` 后只运行 `deployment-carrier`，复用该精确runtime已有质量/镜像证据；不填写时沿原质量与镜像lane
