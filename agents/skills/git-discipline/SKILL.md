---
name: git-discipline
description: evorule 仓 git 提交与推送纪律——commit 身份固定、小步提交、推送前密钥检查、推送后 CI 检绿
---

# evorule 仓 git 纪律

在 evorule 系列仓执行任何 git 提交或推送操作前，先通读本技能并逐条遵守。
本技能是团队工程纪律的权威摘要；若与现场要求冲突，停止操作并如实上报冲突，等待人工裁决。

## 提交纪律

1. **身份固定**：commit 的作者与提交者均为 `Mr.Damu Zheng <evorulelab@gmail.com>`。
   通过逐次提交参数设置（`git -c user.name="Mr.Damu Zheng" -c user.email=evorulelab@gmail.com commit --author="Mr.Damu Zheng <evorulelab@gmail.com>"`）实现，
   **严禁**通过修改 git config 达成。
2. **小步提交**：每完成一个独立小项至少产生一次 commit；不相关改动不得混入同一 commit。
3. **消息规范**：commit 消息说明「为什么」而非罗列文件清单；公开面文件
   （README / CHANGELOG / NOTICE / 文档 / UI 文本 / 公开代码注释）不得出现内部任务编号前缀。

## 推送纪律

4. **推送前密钥检查**：推送任何远端之前，必须对本仓所有 `.env` 文件运行
   `evorule 仓 scripts/check-push-secret-safety.ps1`（逐个 env 文件传参），
   确认 PASS 后方可推送；有命中立即停止并上报。
5. **双推**：先推 Gitee（remote 名 `origin`），再推 GitHub（remote 名 `github`）。
6. **CI 检绿**：推送完成后必须检查远端 CI 是否全绿（可用
   `evorule 仓 scripts/check-ci-green.ps1`，按 HEAD sha 轮询）；
   存在红灯必须修复为绿，任务方算完成；检查超时可改用 GitHub API 直查兜底。
7. **镜像重试**：GitHub 推送偶发失败可隔几分钟重试，一般 2-5 次内成功；仍失败则放置待办。

## 禁止事项

- 禁止 force push 到 `main` / `master`；
- 禁止提交含密钥、凭据、内网台账路径的文件；
- 禁止 `--no-verify` 跳过钩子；
- 新建远端仓库须先经人工审批。

## 违规处置

发现自身即将违反上述任一条时：停止操作，向会话上游如实说明哪一条、为什么无法继续，
等待人工裁决。不得为了「完成任务」静默降级纪律。
