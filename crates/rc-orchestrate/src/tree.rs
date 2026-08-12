//! Task tree: 根 = orchestrator,子 = executor,孙 = 次级 orchestrator 的子。
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct TaskNode {
    pub id: String,
    pub parent: Option<String>, // None = 根(orchestrator)
    pub description: String,    // 完整任务描述,如 "前端代码开发+代码审计"
    pub model: Option<String>,  // 池选的模型
    pub skill: Option<String>,  // 需要的顶层设计/执行 skill
    pub status: TaskStatus,
    pub summary: Option<String>, // 完成摘要(紧凑)
    pub depth: u8,
}

impl TaskNode {
    pub fn new(id: impl Into<String>, parent: Option<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            parent,
            description: description.into(),
            model: None,
            skill: None,
            status: TaskStatus::Pending,
            summary: None,
            depth: 1,
        }
    }
}

/// 任务树:根 = orchestrator,子 = executor,孙 = 次级 orchestrator 的子。
#[derive(Debug, Clone, Default)]
pub struct TaskTree {
    pub nodes: BTreeMap<String, TaskNode>,
}

impl TaskTree {
    pub fn root(&self) -> Option<&TaskNode> {
        self.nodes.values().find(|n| n.parent.is_none())
    }

    pub fn children_of(&self, id: &str) -> Vec<&TaskNode> {
        self.nodes
            .values()
            .filter(|n| n.parent.as_deref() == Some(id))
            .collect()
    }

    pub fn add(&mut self, node: TaskNode) {
        self.nodes.insert(node.id.clone(), node);
    }

    pub fn update_status(&mut self, id: &str, status: TaskStatus) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.status = status;
        }
    }

    pub fn set_summary(&mut self, id: &str, summary: String) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.summary = Some(summary);
        }
    }

    /// 深度:根=1,直接子=2,孙=3。返回 None 若 >3(超层)。
    pub fn depth_of(&self, id: &str) -> Option<u8> {
        let mut n = self.nodes.get(id)?;
        let mut d = 1;
        while let Some(p) = &n.parent {
            d += 1;
            if d > 3 {
                return None;
            }
            n = self.nodes.get(p)?;
        }
        Some(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_root_and_children_and_depth() {
        let mut tree = TaskTree::default();
        tree.add(TaskNode::new("root", None, "build app"));
        tree.add(TaskNode::new("a1", Some("root".into()), "write backend"));
        tree.add(TaskNode::new("a1-1", Some("a1".into()), "write api"));
        assert_eq!(tree.root().unwrap().id, "root");
        assert_eq!(tree.children_of("root").len(), 1);
        assert_eq!(tree.children_of("a1")[0].id, "a1-1");
        assert_eq!(tree.depth_of("root"), Some(1));
        assert_eq!(tree.depth_of("a1"), Some(2));
        assert_eq!(tree.depth_of("a1-1"), Some(3));
    }

    #[test]
    fn tree_depth_exceeds_three_returns_none() {
        let mut tree = TaskTree::default();
        tree.add(TaskNode::new("root", None, "r"));
        tree.add(TaskNode::new("a", Some("root".into()), "a"));
        tree.add(TaskNode::new("a1", Some("a".into()), "a1"));
        tree.add(TaskNode::new("a1x", Some("a1".into()), "a1x")); // 第4层
        assert_eq!(tree.depth_of("a1x"), None);
    }

    #[test]
    fn tree_status_and_summary() {
        let mut tree = TaskTree::default();
        tree.add(TaskNode::new("a1", Some("root".into()), "write api"));
        tree.update_status("a1", TaskStatus::Done);
        tree.set_summary("a1", "wrote api, 2 tests pass".into());
        assert_eq!(tree.nodes["a1"].status, TaskStatus::Done);
        assert_eq!(tree.nodes["a1"].summary.as_deref(), Some("wrote api, 2 tests pass"));
    }
}
