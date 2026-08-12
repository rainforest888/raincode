//! TodoList:每个 agent 自持的待办清单(spec §1.3b/§2.1b)。
//! 干活前先拆出待办逐步完成,状态可见;TUI 底部显示 claude-code 风格概览。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct TodoItem {
    /// 稳定 id(默认 = text;OrchestratorDispatch 用子任务 id,便于按结果回标状态)。
    pub id: String,
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Debug, Default, Clone)]
pub struct TodoList {
    pub items: Vec<TodoItem>,
}

impl TodoList {
    pub fn add(&mut self, text: &str) {
        self.items.push(TodoItem {
            id: text.into(),
            text: text.into(),
            status: TodoStatus::Pending,
        });
    }

    /// 带稳定 id 添加(OrchestratorDispatch 的子任务用,结果可按 id 回标)。
    pub fn add_with_id(&mut self, id: &str, text: &str) {
        self.items.push(TodoItem {
            id: id.into(),
            text: text.into(),
            status: TodoStatus::Pending,
        });
    }

    pub fn set(&mut self, text: &str, status: TodoStatus) {
        if let Some(i) = self
            .items
            .iter_mut()
            .find(|i| i.text == text || i.id == text)
        {
            i.status = status;
        }
    }

    /// 按稳定 id 回标状态(OrchestratorResult 用子任务 id 标记完成/失败)。
    pub fn set_by_id(&mut self, id: &str, status: TodoStatus) {
        if let Some(i) = self.items.iter_mut().find(|i| i.id == id) {
            i.status = status;
        }
    }

    pub fn count(&self, status: TodoStatus) -> usize {
        self.items.iter().filter(|i| i.status == status).count()
    }

    /// 统计行,如 "15 tasks (8 done, 4 in progress, 3 open)"。
    pub fn stats_line(&self) -> String {
        let total = self.items.len();
        let done = self.count(TodoStatus::Done);
        let prog = self.count(TodoStatus::InProgress);
        let open = self.count(TodoStatus::Pending);
        format!("{total} tasks ({done} done, {prog} in progress, {open} open)")
    }

    /// 最近进行中 + 未开始项(最多 n 条)。
    pub fn recent(&self, n: usize) -> Vec<&TodoItem> {
        self.items
            .iter()
            .filter(|i| matches!(i.status, TodoStatus::InProgress | TodoStatus::Pending))
            .take(n)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_stats_and_recent() {
        let mut t = TodoList::default();
        for s in ["全面功能验证", "补全 skill 网络", "主界面子代理看板"] {
            t.add(s);
        }
        t.set("全面功能验证", TodoStatus::Done);
        t.set("补全 skill 网络", TodoStatus::InProgress);
        assert_eq!(t.stats_line(), "3 tasks (1 done, 1 in progress, 1 open)");
        let recent = t.recent(5);
        assert_eq!(recent.len(), 2);
        assert!(recent[0].text.contains("补全 skill 网络"));
    }

    #[test]
    fn todo_persists_across_agent_steps() {
        let mut t = TodoList::default();
        t.add("写后端");
        t.add("写前端");
        t.set("写后端", TodoStatus::Done);
        t.set("写前端", TodoStatus::InProgress);
        assert_eq!(t.count(TodoStatus::Done), 1);
        assert_eq!(t.count(TodoStatus::InProgress), 1);
        assert_eq!(t.count(TodoStatus::Pending), 0);
    }

    #[test]
    fn todo_recent_respects_limit() {
        let mut t = TodoList::default();
        for i in 0..7 {
            t.add(&format!("task-{i}"));
        }
        assert_eq!(t.recent(5).len(), 5);
        assert_eq!(t.recent(100).len(), 7);
    }
}
