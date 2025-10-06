# crawler-lab 行为等价基线（G1 验收参照——DEV_09 §6 条 10）

录制环境：master @ 0962e08（迁移前）· 2026-10-06 · pages=60 depth=2 fanout=3 batch=32

命令：
```bash
./deploy/crawler-lab/run-lab.sh --pages 60 --skip-ts
```

## 关键断言（G1 迁移后必须逐字重现）

- 爬取：`fetched=60（去重后）pushed_total=99`
- ray 索引：`terms=33 postings=1746`
- jvm 倒排：`{"terms":33,"postings":1746,"queries":0}`
- 搜索（top-5 doc/score 对，四组 query 全部一致——见 pre_migration_output.txt）
- 终态：`crawler-lab PASS（四运行时全链集成）`

## 轨迹 hash

pre_migration_output.txt（含时间戳行——比较时过滤"耗时/progress"行）：
1537c00a3d93cb787b36e3c6abc3e00f55916f0197edbd3841fcaf0545dbe67c

判定脚本：run_regression.sh（G1 交付）将重跑同参数并 diff 关键断言行。
