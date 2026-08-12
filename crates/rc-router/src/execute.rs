//! Sub-task execution for routed multi-agent runs.
//!
//! `execute_subtasks` fans out each routed sub-task to a fresh `rc_core::Agent`
//! session (bounded concurrency), collects a [`SubtaskResult`] per sub-task and
//! persists every outcome into `swarm_runs` via `Store::save_swarm_run`.

use rc_proto::AgentEvent;
use rc_state::Store;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub struct SubtaskResult {
    pub subtask_id: String,
    pub model: String,
    pub summary: String,
    pub usage: Option<Value>,
    pub ok: bool,
}

/// Run sub-tasks concurrently (bounded by `concurrency`), each as a fresh
/// `rc_core::Agent::run` session, and persist every outcome to `swarm_runs`.
///
/// `emit` is an optional supervision-event callback: `AgentSpawned` before each
/// spawn, `AgentToolCall`/`AgentStatus` while streaming, `AgentResult` on
/// completion. It is `Arc<dyn Fn + Send + Sync>` (not a plain `&dyn Fn`)
/// because each spawned task needs its own clone that lives across
/// `tokio::spawn`'s `'static` boundary.
///
/// `Store` holds a rusqlite `Connection` which is `Send` but *not* `Sync`, so
/// it is never shared into the spawned tasks: each task only runs the agent and
/// returns its result, and `Store::save_swarm_run` is applied afterwards on the
/// caller's thread (which also keeps ordering deterministic).
pub async fn execute_subtasks(
    jobs: Vec<(String, String, rc_core::AgentConfig)>, // (subtask_id, prompt, config)
    store: &Store,
    concurrency: usize,
    subtask_timeout: std::time::Duration,
    emit: Option<std::sync::Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    steer_hub: Option<std::sync::Arc<rc_core::SteerHub>>,
    cancel: Option<std::sync::Arc<AtomicBool>>,
) -> Vec<SubtaskResult> {
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut handles = Vec::with_capacity(jobs.len());
    for (subtask_id, prompt, mut config) in jobs {
        // 取消:不再启动新子任务(已在跑的让其收尾,结果照常收集)。
        if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        // 每个子任务注册 steering receiver;用户对某 agent 发命令/喂引导 →
        // hub.send(id, text) → 该 agent 的 steer_rx 拾取(下一轮最高优先级注入)。
        if let Some(hub) = &steer_hub {
            config.steer_rx = Some(hub.register(&subtask_id));
        }
        // Each sub-task gets its own session inside its own config store so
        // rc-core can persist messages / summary for that run independently.
        let session_id = config
            .store
            .create_session(&config.cwd.to_string_lossy())
            .map(|s| s.id)
            .unwrap_or_else(|_| subtask_id.clone());
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .expect("router semaphore closed");
        // 监督事件:子任务 spawn 前上报 AgentSpawned,桌面监督板据此为该子任务建行。
        if let Some(emit) = &emit {
            emit(AgentEvent::AgentSpawned {
                id: subtask_id.clone(),
                model: config.provider.id().to_string(),
                role: "subtask".into(),
                task: prompt.clone(),
            });
        }
        // 每个 spawn 持有一份自己的 Arc 克隆(`&dyn Fn` 无法跨 tokio::spawn 的
        // 'static 边界,Arc 克隆可随 async move 一起进入任务)。
        let emit_for_task = emit.clone();
        let hub_for_task = steer_hub.clone();
        let timeout_for_task = subtask_timeout;
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let model = config.provider.id().to_string();
            let agent = rc_core::Agent::new(config);
            // 每个子任务独立超时:防止挂死的 provider 无限占用信号量许可、拖垮整个 route。
            // 时长来自配置(默认 600s),跑 cargo test 的编译型子任务需要更宽容限。
            let timed = tokio::time::timeout(
                timeout_for_task,
                async {
                    let start = std::time::Instant::now();
                    let mut last_beat = start;
                    let mut stream = agent.run(session_id, prompt);
                    let mut summary = String::new();
                    let mut usage = None;
                    let mut ok = false;
                    let mut tokens: u64 = 0;
                    while let Some(ev) = stream.next().await {
                        // 心跳:消费事件后至少间隔 500ms 才再发 AgentStatus,让监督板在
                        // 工具调用间隙、长时间无输出时仍能看到 agent 在运行。
                        let now = std::time::Instant::now();
                        if now.duration_since(last_beat).as_millis() >= 500 {
                            if let Some(emit) = &emit_for_task {
                                emit(AgentEvent::AgentStatus {
                                    id: subtask_id.clone(),
                                    phase: "running".into(),
                                    tokens,
                                    elapsed_ms: now.duration_since(start).as_millis() as u64,
                                });
                            }
                            last_beat = now;
                        }
                        match ev {
                            AgentEvent::Token { delta } => {
                                summary.push_str(&delta);
                                // NOTE: `tokens` here is NOT provider token usage — it is
                                // the streamed output character count (chars), a cheap
                                // progress heuristic for the supervision UI. Real usage
                                // arrives later in AgentEvent::Done { usage }. Frontends
                                // display this field as "chars" accordingly.
                                tokens = tokens.saturating_add(delta.chars().count() as u64);
                            }
                            AgentEvent::ToolCall { name, args, .. } => {
                                // 监督事件:工具调用时上报 AgentToolCall(监督板展示调用明细)。
                                if let Some(emit) = &emit_for_task {
                                    emit(AgentEvent::AgentToolCall {
                                        id: subtask_id.clone(),
                                        tool: name,
                                        args_preview: serde_json::to_string(&args)
                                            .unwrap_or_default(),
                                    });
                                }
                                // 工具调用本身即一次活跃信号,重置心跳计时。
                                last_beat = now;
                            }
                            AgentEvent::Done { summary: s, usage: u, .. } => {
                                summary = s;
                                usage = u;
                                ok = true;
                            }
                            AgentEvent::Error { message } => {
                                summary = message;
                                ok = false;
                            }
                            // 监督事件:上下文累计推进时上报 ContextUpdate(桌面监督板顶栏进度条)。
                            // 携带子任务 id,前端据此把 per-agent used 聚合成会话级窗口。
                            AgentEvent::ContextUpdate { used, limit, pct, .. } => {
                                if let Some(emit) = &emit_for_task {
                                    emit(AgentEvent::ContextUpdate {
                                        used,
                                        limit,
                                        pct,
                                        agent_id: Some(subtask_id.clone()),
                                    });
                                }
                            }
                            AgentEvent::SessionStarted { session_id } => {
                                if let Some(emit) = &emit_for_task {
                                    emit(AgentEvent::SessionStarted { session_id });
                                }
                            }
                            _ => {}
                        }
                    }
                    (summary, usage, ok)
                },
            )
            .await;
            let (summary, usage, ok) = match timed {
                Ok(inner) => inner,
                Err(_) => {
                    // 超时:取消底层 agent 任务(否则它继续跑工具/改文件,任务泄漏)。
                    agent.cancel();
                    ("subtask timed out".to_string(), None, false)
                }
            };
            // 监督事件:子任务结束上报 AgentResult(ok/failed + 成本),监督板据此收行。
            if let Some(emit) = &emit_for_task {
                let cost = usage
                    .as_ref()
                    .and_then(|u| u.get("total_cost"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                emit(AgentEvent::AgentResult {
                    id: subtask_id.clone(),
                    verdict: if ok { "ok".into() } else { "failed".into() },
                    tests: String::new(),
                    cost,
                });
            }
            // 子任务结束,注销 steering 注册(后续 send 返回 false)。
            if let Some(hub) = &hub_for_task {
                hub.unregister(&subtask_id);
            }
            (subtask_id, model, summary, usage, ok)
        }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok((subtask_id, model, summary, usage, ok)) => {
                let result_json =
                    serde_json::json!({ "summary": summary, "ok": ok }).to_string();
                if let Err(e) = store.save_swarm_run(&subtask_id, "", &result_json) {
                    // Surface persistence failures instead of silently dropping them
                    // (Task 5 review finding): a lost swarm_runs row means the run is
                    // unrecoverable from the data layer. tracing::warn! alone is dropped
                    // under the default `raincode=info` filter (rc_router::execute is a
                    // different target), so also emit an unconditional eprintln! to make
                    // the failure visible in default config.
                    tracing::warn!("save_swarm_run failed for {subtask_id}: {e}");
                    eprintln!("route: failed to persist swarm result for {subtask_id}: {e}");
                }
                out.push(SubtaskResult {
                    subtask_id,
                    model,
                    summary,
                    usage,
                    ok,
                });
            }
            Err(error) => out.push(SubtaskResult {
                subtask_id: String::new(),
                model: String::new(),
                summary: format!("subtask panicked: {error}"),
                usage: None,
                ok: false,
            }),
        }
    }
    out
}

/// 按依赖分批执行:每批运行所有未满足依赖(全部依赖已完成)的 job,完成后解锁
/// 依赖它的 job。结果经 emit 回灌 `OrchestratorResult`,供主控决定下一批。
///
/// `jobs`: `(id, prompt, depends_on, config)`。环形依赖防护:无就绪 job 时停止。
pub async fn execute_subtasks_batched(
    jobs: Vec<(String, String, Vec<String>, rc_core::AgentConfig)>,
    store: &Store,
    concurrency: usize,
    subtask_timeout: std::time::Duration,
    emit: Option<std::sync::Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    steer_hub: Option<std::sync::Arc<rc_core::SteerHub>>,
    cancel: Option<std::sync::Arc<AtomicBool>>,
) -> Vec<SubtaskResult> {
    let mut remaining = jobs;
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut all_results = Vec::new();
    while !remaining.is_empty() {
        // 取消:提前结束(不启动下一批)。
        if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        // 收集本轮就绪的下标(deps 全部 done),再 drain 取出。
        let ready_idx: Vec<usize> = remaining
            .iter()
            .enumerate()
            .filter(|(_, (_, _, deps, _))| deps.iter().all(|d| done.contains(d)))
            .map(|(i, _)| i)
            .collect();
        if ready_idx.is_empty() {
            break; // 环形依赖,防死循环
        }
        // 逆序取,避免下标偏移。
        let mut ready: Vec<(String, String, rc_core::AgentConfig)> = Vec::new();
        for i in ready_idx.iter().rev() {
            let (id, p, _, c) = remaining.remove(*i);
            ready.push((id, p, c));
        }
        ready.reverse();
        let results =
            execute_subtasks(
                ready,
                store,
                concurrency,
                subtask_timeout,
                emit.clone(),
                steer_hub.clone(),
                cancel.clone(),
            )
            .await;
        for r in &results {
            done.insert(r.subtask_id.clone());
            if let Some(emit) = &emit {
                emit(AgentEvent::OrchestratorResult {
                    node_id: r.subtask_id.clone(),
                    status: if r.ok { "ok".into() } else { "failed".into() },
                    summary: r.summary.clone(),
                });
            }
        }
        all_results.extend(results);
    }
    all_results
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_pro::mock::MockProvider;
    use rc_pro::ProviderConfig;
    use rc_sandbox::{AutoApproveHook, AutoUserHook, CommandPolicy, NetworkPolicy};
    use rc_skill::SkillStore;
    use std::sync::Arc;

    fn mock_config(tmp: &tempfile::TempDir, model: &str) -> rc_core::AgentConfig {
        let skill_store = SkillStore::new(tmp.path().join("skills"));
        let cfg = ProviderConfig {
            kind: "mock".into(),
            base_url: String::new(),
            model: model.into(),
            api_key: None,
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: serde_json::json!({
                "script": [
                    {"type": "text", "text": format!("out-{model}")},
                    {"type": "done", "stop_reason": "end_turn"}
                ],
                "auto_advance": true
            }),
        };
        rc_core::AgentConfig {
            provider: Arc::new(MockProvider::new(cfg, model.into())),
            plan_provider: None,
            review_provider: None,
            store: Store::open_in_memory().unwrap(),
            skill_store,
            tools: vec![],
            approval: Arc::new(AutoApproveHook),
            command_policy: CommandPolicy::default(),
            network_policy: NetworkPolicy::default(),
            cwd: tmp.path().to_path_buf(),
            state_path: tmp.path().join("state.db"),
            max_turns: 4,
            max_steps: 0,
            evolve_on_finish: false,
            plan_mode: false,
            hooks: rc_core::HooksConfig::default(),
            agent: Some("coding".into()),
            max_history_bytes: Some(64 * 1024),
            mcp_servers: vec![],
            entropy_mode: false,
            plan_max_rounds: 6,
            plan_max_questions: 5,
            review_max_rounds: 3,
            max_cycles: 1,
            user_input: Arc::new(AutoUserHook::default()),
        steer_rx: None,
        context_window: 0,
        subagent: None,
        guard_cfg: None,
        guard_hook: None,
        guard_memo: None,
        guard_home: None,
}
    }

    #[tokio::test]
    async fn execute_subtasks_runs_each_job_and_persists_swarm_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("router.db")).unwrap();
        let jobs = vec![
            ("s1".to_string(), "do task one".to_string(), mock_config(&tmp, "mock-1")),
            ("s2".to_string(), "do task two".to_string(), mock_config(&tmp, "mock-2")),
        ];
        let results =
            execute_subtasks(jobs, &store, 2, std::time::Duration::from_secs(60), None, None, None)
                .await;

        assert_eq!(results.len(), 2, "each sub-task must produce a SubtaskResult");
        assert!(results.iter().all(|r| r.ok));
        assert_eq!(results[0].subtask_id, "s1");
        assert_eq!(results[0].model, "mock-1");
        assert!(results[0].summary.contains("out-mock-1"));
        assert_eq!(results[1].subtask_id, "s2");
        assert_eq!(results[1].model, "mock-2");

        // Every outcome must be persisted into swarm_runs.
        let runs = store.list_swarm_runs(10).unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[tokio::test]
    async fn execute_subtasks_emits_supervision_events() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("router.db")).unwrap();
        let jobs = vec![
            ("s1".to_string(), "do task one".to_string(), mock_config(&tmp, "mock-1")),
        ];
        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> = Arc::default();
        let sink = {
            let events = events.clone();
            move |ev: AgentEvent| events.lock().unwrap().push(ev)
        };
        let results =
            execute_subtasks(
                jobs,
                &store,
                2,
                std::time::Duration::from_secs(60),
                Some(Arc::new(sink)),
                None,
                None,
            )
            .await;

        assert_eq!(results.len(), 1);
        assert!(results[0].ok);

        let evs = events.lock().unwrap().clone();
        // AgentSpawned 先于任何结果,携带子任务 id 与角色。
        let spawned = evs.iter().find_map(|ev| match ev {
            AgentEvent::AgentSpawned { id, role, .. } => Some((id.clone(), role.clone())),
            _ => None,
        });
        let (id, role) = spawned.expect("AgentSpawned must be emitted");
        assert_eq!(id, "s1");
        assert_eq!(role, "subtask");
        // AgentResult 携带最终判定(ok)。
        let result_ok = evs.iter().any(|ev| {
            matches!(
                ev,
                AgentEvent::AgentResult { id, verdict, .. }
                    if id.as_str() == "s1" && verdict.as_str() == "ok"
            )
        });
        assert!(result_ok, "AgentResult(ok) must be emitted for s1");
    }

    #[tokio::test]
    async fn execute_subtasks_skips_when_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("router.db")).unwrap();
        let mk_jobs = || {
            vec![
                ("s1".to_string(), "do task one".to_string(), mock_config(&tmp, "mock-1")),
                ("s2".to_string(), "do task two".to_string(), mock_config(&tmp, "mock-2")),
            ]
        };
        // 取消置位 → 不启动任何子任务。
        let cancel = std::sync::Arc::new(AtomicBool::new(true));
        let results = execute_subtasks(
            mk_jobs(),
            &store,
            2,
            std::time::Duration::from_secs(60),
            None,
            None,
            Some(cancel),
        )
        .await;
        assert!(results.is_empty(), "cancelled run must not spawn sub-tasks");
        // 未取消 → 全部执行。
        let cancel2 = std::sync::Arc::new(AtomicBool::new(false));
        let results2 = execute_subtasks(
            mk_jobs(),
            &store,
            2,
            std::time::Duration::from_secs(60),
            None,
            None,
            Some(cancel2),
        )
        .await;
        assert_eq!(results2.len(), 2);
    }

    #[tokio::test]
    async fn batched_executes_in_dependency_order() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("router.db")).unwrap();
        // a:无依赖;b,c:依赖 a。
        let jobs = vec![
            ("a".to_string(), "do a".to_string(), vec![], mock_config(&tmp, "mock-a")),
            ("b".to_string(), "do b".to_string(), vec!["a".to_string()], mock_config(&tmp, "mock-b")),
            ("c".to_string(), "do c".to_string(), vec!["a".to_string()], mock_config(&tmp, "mock-c")),
        ];
        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> = Arc::default();
        let sink = {
            let events = events.clone();
            move |ev: AgentEvent| events.lock().unwrap().push(ev)
        };
        let results = execute_subtasks_batched(
            jobs,
            &store,
            2,
            std::time::Duration::from_secs(60),
            Some(Arc::new(sink)),
            None,
            None,
        )
        .await;
        assert_eq!(results.len(), 3);
        // 回灌的 OrchestratorResult 应覆盖全部三个。
        let evs = events.lock().unwrap().clone();
        let result_nodes: Vec<String> = evs
            .iter()
            .filter_map(|ev| match ev {
                AgentEvent::OrchestratorResult { node_id, .. } => Some(node_id.clone()),
                _ => None,
            })
            .collect();
        assert!(result_nodes.contains(&"a".to_string()));
        assert!(result_nodes.contains(&"b".to_string()));
        assert!(result_nodes.contains(&"c".to_string()));
    }
}
