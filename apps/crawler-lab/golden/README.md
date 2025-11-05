# crawler-lab 行为等价基线（G1 验收参照——DEV_09 §6 条 10）

录制环境：master @ 0962e08（迁移前）· 2026-10-06 · pages=60 depth=2 fanout=3 batch=32

命令：
```bash
./apps/crawler-lab/deploy/run-lab.sh --pages 60 --skip-ts
```

## 关键断言（G1 迁移后必须逐字重现）

- 爬取：`fetched=60（去重后）pushed_total=99`
- ray 索引：`terms=33 postings=1746`
- jvm 倒排：`{"terms":33,"postings":1746,"queries":0}`
- 搜索（top-5 doc/score 对，四组 query 全部一致——见 pre_migration_output.txt）
- 终态：`crawler-lab PASS（四运行时全链集成）`

## 历史输出文件 SHA-256

pre_migration_output.txt 原始文件（包含时间和旧启动器输出）的摘要，不是 cid 轨迹摘要：
1537c00a3d93cb787b36e3c6abc3e00f55916f0197edbd3841fcaf0545dbe67c

判定脚本 run_regression.sh 重建制品后重跑同参数，以结构化字段精确比较固定工作负载，不比较耗时／JSON 对象键序。该兼容入口回归本身不证明 manifest 迁移。`make test-crawler-manifest` 另外执行标准装配、声明连接、三形态业务、TS 查询及回收；两者均不证明历史 cid 轨迹等价，原始 CID 认证仍待实现。
