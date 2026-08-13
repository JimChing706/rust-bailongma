# M4 Conversations 覆盖率 · 半程报告

- 日期：2026-08-13
- 数据源：真实 jarvis.db（38MB，29 张表，E:\BailongmaData\data\jarvis.db）
- 回访工具：scripts/revisit_m4_metrics.py（commit 6e52d44）

## 结论

分母侧完全健康、持续活跃；分子侧为零。**覆盖率当前 0%**，等新版（M1 观测层已挂载）重启建表后自动开始积累。

## 分母：conversations（1300 行）

| 指标 | 数值 |
|---|---|
| user 轮 | 620 |
| jarvis 轮 | 680 |
| 应答率 | 109.7%（每条用户消息都有回复，无遗漏） |
| 活跃度 | 近 14 天每天都有对话 |
| 高峰 | 8/10-8/12 进入高峰（153 → 102 → 98） |
| 今日 | 43（截至报告时刻） |
| 渠道分布 | TUI 448 / API 406 / WECHAT 371 / REMINDER 75 |

## 唤醒成本（半程信号）

- REMINDER/唤醒类 75 轮，占 5.8%
- 唤醒没有空转，都落在真实对话里

## 分子：观测层 5 表全缺失

- llm_calls / llm_turns / llm_tool_calls / llm_context_sections / llm_metrics_daily 均未建表
- 覆盖率 = llm_turns ÷ user 轮 = 0 ÷ 620 = 0%
- 根因：当前运行实例仍是旧版本，M1 观测层（metrics::init 启动建表）未在真实环境挂载生效

## 下一步

1. 用最新构建（含 M1 装配）重启应用 → 自动建 5 张观测表
2. 跑几轮真实 LLM 调用积累数据
3. 重跑 `python scripts/revisit_m4_metrics.py` 出完整验收报告

## 复现

```powershell
$env:PYTHONIOENCODING="utf-8"
python scripts/_conv_coverage.py   # 本报告的 conversations 侧覆盖统计
python scripts/revisit_m4_metrics.py  # 完整 M4 六项验收 SQL
```
