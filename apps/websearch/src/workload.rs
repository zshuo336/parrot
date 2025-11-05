//! Shared real crawl/recovery/HTTP workflow, independent of deployment ownership.
use super::*;

pub(crate) struct Refs {
    pub frontier: Option<BoxedActorRef>,
    pub tokenizer: Option<BoxedActorRef>,
    pub search: BoxedActorRef,
}

pub(crate) struct Workload {
    pub options: cli::Options,
    pub identity: component_identity::Identity,
    web_listener: tokio::net::TcpListener,
    journal: Option<crawl_journal::Journal>,
    _serve_lease: Option<std::fs::File>,
    dedupe_path: PathBuf,
    dedupe: HashSet<String>,
    legacy_urls: Vec<String>,
}

/// 内置多样化种子（站点目标制 ≥100 站——队列耗尽/无种子时自动回填）。
const BUILTIN_SEEDS: &[&str] = &[
    // 中文技术/科普/社区
    "https://www.runoob.com",
    "https://developer.mozilla.org/zh-CN/",
    "https://www.zhihu.com",
    "https://www.cnblogs.com",
    "https://juejin.cn",
    "https://segmentfault.com",
    "https://www.oschina.net",
    "https://www.infoq.cn",
    "https://www.imooc.com",
    "https://www.liaoxuefeng.com",
    // 门户/百科（出链丰富——快速扩散 host 多样性）
    "https://www.wikipedia.org",
    "https://zh.wikipedia.org",
    "https://baike.baidu.com",
    "https://www.hao123.com",
    "https://www.qq.com",
    "https://www.sina.com.cn",
    "https://www.sohu.com",
    "https://www.163.com",
    "https://www.ifeng.com",
    "https://www.people.com.cn",
    // 开发者/文档站
    "https://github.com",
    "https://stackoverflow.com",
    "https://docs.python.org",
    "https://www.rust-lang.org",
    "https://go.dev",
    "https://nodejs.org",
    "https://www.erlang.org",
    "https://akka.io",
    "https://ray.io",
    "https://redis.io",
    "https://www.postgresql.org",
    "https://nginx.org",
    "https://httpd.apache.org",
    "https://maven.apache.org",
    "https://gradle.org",
    "https://www.docker.com",
    "https://kubernetes.io",
    // 高校/机构（外链丰富）
    "https://www.tsinghua.edu.cn",
    "https://www.pku.edu.cn",
    "https://www.ustc.edu.cn",
    "https://www.fudan.edu.cn",
    "https://www.sjtu.edu.cn",
    "https://www.nju.edu.cn",
    "https://www.zju.edu.cn",
    "https://www.cas.cn",
    "https://www.cctv.com",
    "https://www.gov.cn",
];

impl Workload {
    /// The driver keeps this guard until component and transport teardown finish.
    pub fn retain_lease(&self) -> std::io::Result<std::fs::File> {
        match (&self.journal, &self._serve_lease) {
            (Some(journal), _) => journal.retain_lease(),
            (_, Some(owner)) => owner.try_clone(),
            _ => Err(std::io::Error::other("workload has no storage owner")),
        }
    }

    /// Acquire local storage ownership and the actual listener before deployment.
    pub async fn prepare(
        mut options: cli::Options,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let cli::Options {
            web_port,
            data_dir,
            index_data,
            serve_only,
            keep_data,
            ..
        } = options.clone();
        // Reserve the actual socket before modifying data or deploying components.
        // Retain it through startup so another process cannot take the checked port.
        let (web_listener, actual_port) = match bind_web_listener(web_port).await {
            Ok(bound) => bound,
            Err(error) => {
                return Err(shutdown::StartupError(format!(
                    "cannot bind Web port {web_port}: {error}"
                ))
                .into());
            }
        };
        if web_port != 0 && actual_port != web_port {
            println!("[ws] Web 服务改用端口 {actual_port}（{web_port} 被占）");
        }
        let web_port = actual_port;
        options.web_port = web_port;
        // Binding is not readiness; launchers still probe the running HTTP service.
        println!("PARROT_WEBSEARCH_PORT={web_port}");

        // 数据目录策略：--keep 续爬（回放旧段+去重表）；默认全新爬取清空旧数据
        // （否则历次残留索引混入 /docs 页面统计——用户实测 docs=175 即此因）
        std::fs::create_dir_all(&data_dir)?;
        // Hold the local directory lease before any remote Stop/Deploy/Clear.
        let serve_lease = if serve_only {
            Some(crawl_journal::Journal::lease(&data_dir)?)
        } else {
            None
        };
        let journal = if serve_only {
            None
        } else {
            Some(crawl_journal::Journal::open(&data_dir)?)
        };
        let mut identity = component_identity::Identity::open(&data_dir)?;
        options.index_data = identity.select_index(index_data, keep_data)?;
        let dedupe_path = data_dir.join("dedupe.tsv");
        let mut legacy_urls = Vec::new();
        let mut dedupe: HashSet<String> = HashSet::new();
        if keep_data && !serve_only && dedupe_path.exists() {
            let file = std::fs::File::open(&dedupe_path)?;
            if file.metadata()?.len() > 256 * 1024 * 1024 {
                return Err("legacy dedupe exceeds byte budget".into());
            }
            let mut txt = String::new();
            std::io::Read::read_to_string(
                &mut std::io::Read::take(file, 256 * 1024 * 1024 + 1),
                &mut txt,
            )?;
            if txt.len() > 256 * 1024 * 1024 {
                return Err("legacy dedupe exceeds byte budget".into());
            }
            for l in txt.lines() {
                if !l.trim().is_empty() {
                    dedupe.insert(l.trim().to_string());
                    legacy_urls.push(l.trim().to_string());
                }
            }
            println!("[ws] 保留数据模式：恢复去重表 {} URLs", dedupe.len());
        }
        // The JVM owns index storage. Cold startup resets it with acknowledged Clear
        // after deployment. Keep the last committed index and journal until then.

        Ok(Self {
            options,
            identity,
            web_listener,
            journal,
            _serve_lease: serve_lease,
            dedupe_path,
            dedupe,
            legacy_urls,
        })
    }

    pub async fn run(
        self,
        refs: Refs,
        client: Arc<RemoteActorSystem>,
        stop: shutdown::Stop,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Self {
            options,
            mut journal,
            _serve_lease,
            identity: _identity,
            web_listener,
            dedupe_path,
            mut dedupe,
            mut legacy_urls,
        } = self;
        let cli::Options {
            seeds,
            pages,
            sites_target,
            max_depth,
            web_port,
            keep_data,
            serve_only,
            crawl_origins,
            ..
        } = options;
        let searcher = refs.search;
        let mut web = None;
        let outcome: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
            if stop.requested() { return Ok(()); }
    let scope: Vec<String> = crawl_origins.iter().map(|url| url.origin().ascii_serialization()).collect();
    if let Some(journal) = journal.as_mut() {
        if !keep_data {
            let initial: Vec<String> = if seeds.is_empty() { BUILTIN_SEEDS.iter().map(|s| s.to_string()).collect() } else { seeds.clone() };
            let mut seen = HashSet::new();
            let tasks = initial.iter().filter_map(|url| normalize_url(url,url)).filter(|url|seen.insert(url.clone())).map(|url|(url,0)).collect();
            // Record reset intent before Clear. A restarted --keep finishes the
            // same intentional reset, instead of joining old tasks to an empty index.
            journal.reset_intent(tasks, scope.clone())?;
        }
        if let Some((tasks, old_scope)) = journal.interrupted_reset() {
            crawl_journal::Journal::check_reset_budget(&tasks, &old_scope)?;
            if old_scope.iter().collect::<HashSet<_>>() != scope.iter().collect::<HashSet<_>>() {
                return Err("interrupted crawl reset requires the original --crawl-origin scope".into());
            }
            let WsClearAck(n) = checked_reply(searcher.as_ref(),"clear index",Box::new(WsClear(vec![]))).await?;
            match std::fs::remove_file(&dedupe_path) {
                Ok(()) => {}
                Err(error) if error.kind()==std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            dedupe.clear(); legacy_urls.clear();
            println!("[ws] 已提交空索引快照（取代 {n} 个历史快照）");
        }
    }
    if serve_only {
        // 只起检索：回放段文件后直接开 Web 服务（搜集/索引/检索各自独立——用户裁定 3）
        let WsHealthzR(health) = checked_reply(
            searcher.as_ref(),
            "replay health",
            Box::new(WsHealthz(vec![])),
        )
        .await?;
        println!(
            "[ws] serve-only 回放索引：{}",
            serde_json::to_string(&health)?
        );
        println!("[ws] 浏览器打开 http://localhost:{web_port} 开始搜索（serve-only）");
        web = Some(tokio::spawn(serve_web(web_listener, client.clone(), searcher, stop.clone())));
        stop.wait().await;
        return Ok(());
    }
    let frontier = refs.frontier.ok_or("missing frontier wire")?;
    let tokenizer = refs.tokenizer.ok_or("missing tokenizer wire")?;

    let journal = journal.as_mut().expect("crawl journal initialized outside serve-only");
    if journal.interrupted_reset().is_none() && !index_preparation::prepare(searcher.as_ref(), &stop).await? {
        return Ok(());
    }
    let history = if journal.interrupted_reset().is_none() {
        journal.checkpoint().filter(|state|state.digest.is_some()).map(|state|state.encode()).unwrap_or_default()
    } else { Vec::new() };
    let WsCrawlStateR(remote_state) = checked_reply(searcher.as_ref(),"read durable crawl state",Box::new(WsCrawlState(history))).await?;
    if let Some((tasks, reset_scope)) = journal.interrupted_reset() {
        journal.begin(remote_state,tasks,reset_scope)?;
    } else if let Some(local_state) = journal.checkpoint() {
        if !local_state.accepts(remote_state) {
            return Err("crawl journal/index generation mismatch; restore the matching index or start an explicit fresh crawl".into());
        }
        if local_state.digest.is_none() {
            eprintln!("[ws] legacy crawl journal migration: replaying all tasks without a snapshot history proof");
            journal.adopt_history(remote_state)?;
        }
    } else {
        // Legacy dedupe has no completion/depth information. Conservative
        // migration replays every old URL as a root; never infer completion.
        for url in &legacy_urls { robots::crawl_url(url).map_err(|e|format!("invalid legacy crawl journal URL: {e}"))?; }
        let mut seen = HashSet::new();
        let legacy: Vec<_> = legacy_urls.iter().filter_map(|url|normalize_url(url,url)).filter(|url|seen.insert(url.clone())).map(|url|(url,0)).collect();
        if !legacy.is_empty() { eprintln!("[ws] legacy dedupe migration: replaying {} URLs as depth-0 roots",legacy.len()); }
        journal.begin(remote_state,legacy,scope)?;
    }
    let recovered = journal.pending();
    let allowed: Vec<_> = crawl_origins.iter().map(url::Url::origin).collect();
    for (url, depth) in &recovered {
        if *depth > max_depth || !robots::in_scope(&robots::crawl_url(url)?, &allowed) {
            return Err("pending crawl task is outside the requested origin/depth scope; restore its original scope or start an explicit fresh crawl".into());
        }
    }
    // The legacy text file is only a compatibility projection; recovery uses
    // the checksum journal, including intents whose Push reply was lost.
    for chunk in recovered.chunks(16) {
        push_pending(frontier.as_ref(),&mut dedupe,&dedupe_path,chunk.to_vec()).await?;
    }
    println!("[ws] 持久待爬任务恢复：{} URLs",recovered.len());
    let builtin: Vec<String> = if !crawl_origins.is_empty() { Vec::new() } else { BUILTIN_SEEDS.iter().map(|s|s.to_string()).collect() };
    let n_user = inject_durable(&seeds,journal,&mut dedupe,&dedupe_path,frontier.as_ref()).await?;
    let n_builtin = if seeds.is_empty() { inject_durable(&builtin,journal,&mut dedupe,&dedupe_path,frontier.as_ref()).await? } else {0};
    // 内置种子游标：队列耗尽且目标未达时回填下一条（单种子死路自愈）
    let mut builtin_cursor = 0usize;
    println!("[ws] 种子注入：用户 {n_user} 条 + 内置 {n_builtin} 条");

    // ── 漫爬主循环（并发抓取 + 双路索引 + 出链回注）──────────────────
    // Web 服务先行启动（站点目标制耗时较长——边爬边查）
    web = Some(tokio::spawn(serve_web(web_listener, client.clone(), searcher.clone_boxed(), stop.clone())));
    println!("[ws] 浏览器打开 http://localhost:{web_port} 开始搜索（爬取后台持续——站点/分词表页实时可见）");
    let fetcher = Arc::new(Fetcher::for_origins(crawl_origins));
    let fetched = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let mut pending: Vec<(String, u16)> = Vec::new();
    let mut in_flight: usize = 0;
    let mut frontier_drained_just_now = false; // 最近一次取批为空（队列耗尽信号）
    let concurrency = 8usize;
    let mut host_last: HashMap<String, Instant> = HashMap::new();
    let mut pages_buf: Vec<indexing::Document> = Vec::new();
    let mut buffered_tasks: Vec<String> = Vec::new();
    let mut submitted_tasks: Vec<String> = Vec::new();
    // 站点目标制（用户裁定 3）：已成功抓取的 host 集 + 每 host 已达最大深度
    let mut hosts_done: HashSet<String> = HashSet::new();
    let mut host_max_depth: HashMap<String, u16> = HashMap::new();
    let t0 = Instant::now();
    let mut last_log = Instant::now();
    let mut last_flush = Instant::now();

    println!(
        "[ws] 爬取目标：≥{sites_target} 个站点 · 每站深度 ≥{max_depth}（安全页数上限 {pages}）"
    );

    loop {
        // Stop only between complete waves: all fetched documents still reach
        // metadata, tokenization and the acknowledged final snapshot below.
        if stop.requested() { break; }
        // 进度打点
        if last_log.elapsed() >= Duration::from_secs(3) {
            let deep_enough = host_max_depth.values().filter(|d| **d >= max_depth).count();
            println!(
                "[ws {:>4}s] fetched={} fail={} sites={}/{} 深度达标={} pending={} inflight={}",
                t0.elapsed().as_secs(),
                fetched.load(Ordering::Relaxed),
                failed.load(Ordering::Relaxed),
                hosts_done.len(),
                sites_target,
                deep_enough,
                pending.len(),
                in_flight
            );
            last_log = Instant::now();
        }
        // 周期段落盘（60s 一次）：崩溃/中断后 serve-only 可回放已爬部分——
        // JVM 侧 flushSegs 全量快照式写段（非增量），周期做不丢数据只多段文件
        if last_flush.elapsed() >= Duration::from_secs(60) {
            indexing::submit(tokenizer.as_ref(),searcher.as_ref(),&pages_buf).await?;
            pages_buf.clear();submitted_tasks.append(&mut buffered_tasks);
            let checkpoint = commit_tasks(searcher.as_ref(),journal,&mut submitted_tasks).await?;
            println!("[ws] 周期段落盘：generation={}",checkpoint.generation);
            last_flush = Instant::now();
        }
        // 站点目标制终局（用户裁定 3）：
        //   达标 = ≥sites_target 站 且 其中 ≥sites_target 站深度 ≥max_depth
        //   ——用户要求“至少 100 站 × 每站至少 10 深”，按已爬站全达标实现；
        //   队列耗尽且未达标 → 自动回填内置种子续爬；回填源也尽 → 收尾
        let done = fetched.load(Ordering::Relaxed) + failed.load(Ordering::Relaxed);
        let deep_enough = host_max_depth.values().filter(|d| **d >= max_depth).count();
        let goal_hit = hosts_done.len() >= sites_target && deep_enough >= sites_target;
        if in_flight == 0 && (done >= pages || (pending.is_empty() && goal_hit)) {
            if goal_hit {
                println!(
                    "[ws] 站点目标达成：{} 站 × 深度≥{} —— 收尾",
                    hosts_done.len(),
                    max_depth
                );
            }
            break;
        }
        // 队列耗尽但目标未达 → 回填内置种子（单种子死路/JS 渲染页自愈）
        if in_flight == 0 && pending.is_empty() && frontier_drained_just_now {
            if builtin_cursor < builtin.len() {
                let batch: Vec<String> =
                    builtin[builtin_cursor..(builtin_cursor + 8).min(builtin.len())].to_vec();
                builtin_cursor += batch.len();
                let n = inject_durable(&batch, journal, &mut dedupe, &dedupe_path, frontier.as_ref()).await?;
                println!(
                    "[ws] 队列耗尽（sites={}/{}）——回填内置种子 +{n}（游标 {}/{}）",
                    hosts_done.len(),
                    sites_target,
                    builtin_cursor,
                    builtin.len()
                );
                frontier_drained_just_now = false; // 新种子已入队——重新等取批
                continue;
            }
            println!(
                "[ws] 内置种子全部耗尽仍 sites={}/{} —— 自然收尾",
                hosts_done.len(),
                sites_target
            );
            break;
        }
        // 补批（frontier 取批 + pending 汇流）——目标达成前持续为 frontier 泵入新 URL
        if pending.len() < concurrency * 2
            && done
                .saturating_add(in_flight as u64)
                .saturating_add(pending.len() as u64)
                < pages
        {
            match frontier
                .send(Box::new(WsNext({
                    let mut b = vec![];
                    put_u32(
                        &mut b,
                        pages
                            .saturating_sub(done)
                            .saturating_sub(in_flight as u64)
                            .saturating_sub(pending.len() as u64)
                            .min(16) as u32,
                    );
                    b
                })))
                .await
                .map_err(|e| e.to_string())
                .and_then(expect_reply::<WsBatch>)
            {
                Ok(WsBatch(batch)) => {
                    frontier_drained_just_now = batch.is_empty();
                    pending.extend(batch);
                }
                Err(e) => {
                    eprintln!("[ws] frontier next 失败：{e:?}");
                    return Err(e.into());
                }
            }
        }
        // 派发并发抓取（每域 ≥200ms 间隔 + robots）
        let mut launched = Vec::new();
        // Complete the shallowest pending layer before launching deeper work.
        // Frontier prefetch may have moved a deeper batch here while shallow
        // URLs are still waiting for their per-host politeness window.
        let wave_depth = pending.iter().map(|(_, depth)| *depth).min();
        let wave_len = pending
            .iter()
            .filter(|(_, depth)| Some(*depth) == wave_depth)
            .count();
        let mut inspected = 0usize;
        while !stop.requested()
            && in_flight < concurrency
            && done.saturating_add(in_flight as u64) < pages
            && !pending.is_empty()
            && inspected < wave_len
        {
            let Some(index) = pending
                .iter()
                .position(|(_, depth)| Some(*depth) == wave_depth)
            else {
                break;
            };
            let (url, depth) = pending.remove(index);
            inspected += 1;
            if depth > max_depth {
                continue;
            }
            let host = host_of(&url);
            let now = Instant::now();
            let ok_to_go = !matches!(host_last.get(&host),
                Some(t) if now.duration_since(*t) < Duration::from_millis(200));
            if !ok_to_go {
                pending.push((url, depth)); // 队尾重试
                continue;
            }
            host_last.insert(host, now);
            let f = fetcher.clone();
            let url_c = url.clone();
            let fetched_c = fetched.clone();
            let failed_c = failed.clone();
            in_flight += 1;
            launched.push(tokio::spawn(async move {
                match f.fetch(&url_c).await {
                    Ok((body, _ct, final_url)) => {
                        fetched_c.fetch_add(1, Ordering::Relaxed);
                        Some((url_c, final_url, depth, body))
                    }
                    Err(e) => {
                        failed_c.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[ws] fetch fail {url_c}: {e}");
                        None
                    }
                }
            }));
        }
        if launched.is_empty() && in_flight == 0 {
            if pending.is_empty() {
                // 队列耗尽——不在此 break：留给上方回填/收尾分支处理
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            // 全队同域节流中——等窗口
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        // join 全部本轮任务（并发已在 spawn 时形成——顺序 join 只等结果）
        if !launched.is_empty() {
            in_flight -= launched.len();
            let mut results = Vec::with_capacity(launched.len());
            for t in launched {
                match t.await {
                    Ok(value) => results.push(value),
                    Err(error) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[ws] fetch task failed: {error}");
                    }
                }
            }
            for res in results.into_iter().flatten() {
                let (task_url, url, depth, body) = res;
                let ex = extract_html(&body);
                let doc_id = doc_id_of(&url);
                // 站点目标制记账（用户裁定 3）：host 集合 + 每 host 已探最深深度
                {
                    let h = host_of(&url);
                    hosts_done.insert(h.clone());
                    host_max_depth
                        .entry(h)
                        .and_modify(|d| *d = (*d).max(depth))
                        .or_insert(depth);
                }
                // Only mark eligible links seen after the frontier acknowledges them.
                let new_links: Vec<_> = if depth < max_depth {
                    ex.links
                        .iter()
                        .take(80)
                        .filter_map(|link| normalize_url(&url, link))
                        .filter(|url| robots::crawl_url(url).is_ok_and(|url|robots::in_scope(&url,&fetcher.crawl_origins)))
                        .map(|url| (url, depth + 1))
                        .collect()
                } else {
                    Vec::new()
                };
                let new_links = journal.discover(new_links)?;
                if enqueue_urls(frontier.as_ref(), &mut dedupe, &dedupe_path, new_links).await? > 0
                {
                    frontier_drained_just_now = false;
                }
                // Buffer complete documents, then replace metadata and the full
                // term set together. Empty token sets also replace old postings.
                buffered_tasks.push(task_url);
                pages_buf.push(indexing::Document {
                    id: doc_id, url, title: ex.title, text: ex.text,
                });
                if pages_buf.len() >= 8 {
                    indexing::submit(tokenizer.as_ref(), searcher.as_ref(), &pages_buf).await?;
                    pages_buf.clear();
                    submitted_tasks.append(&mut buffered_tasks);
                }
            }
        }
    }
    // Accept the tail batch, including documents with no searchable terms.
    indexing::submit(tokenizer.as_ref(), searcher.as_ref(), &pages_buf).await?;
    // Confirm persistence and reconciliation before reporting completion.
    submitted_tasks.append(&mut buffered_tasks);
    let checkpoint = commit_tasks(searcher.as_ref(),journal,&mut submitted_tasks).await?;
    println!("[ws] 索引段落盘：generation={}",checkpoint.generation);
    let WsIndexStatsR((terms, postings)) = checked_reply(
        tokenizer.as_ref(),
        "tokenizer statistics",
        Box::new(WsIndexStats(vec![])),
    )
    .await?;
    println!("[ws] ray 分词统计：terms={terms} postings={postings}");
    let WsHealthzR(health) = checked_reply(
        searcher.as_ref(),
        "index health",
        Box::new(WsHealthz(vec![])),
    )
    .await?;
    println!("[ws] akka 索引：{}", serde_json::to_string(&health)?);
    println!(
        "[ws] 爬取完成：fetched={} fail={} 站点={}（最深达 {} 层）耗时 {:?}",
        fetched.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
        hosts_done.len(),
        host_max_depth.values().copied().max().unwrap_or(0),
        t0.elapsed()
    );

    // ── Web 查询服务（百度式——浏览器 http://localhost:port）─────────
    // 站点目标制爬取耗时较长——Web 服务已随爬取启动（spawn），此处常驻不退出
    println!(
        "[ws] 爬取收尾完成——Web 服务常驻：浏览器打开 http://localhost:{web_port}（Ctrl-C 退出）"
    );
    stop.wait().await;
    Ok(())

        }.await;
        stop.request();
        let web_result = match web {
            Some(task) => task.await.map_err(|error| error.to_string()),
            None => Ok(()),
        };
        if let Err(error) = &web_result {
            eprintln!("[ws] cleanup: {error}");
        }
        outcome?;
        web_result?;
        Ok(())
    }
}
