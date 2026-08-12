//! SkillNavigator:命中索引 → 菜单(方向+子列表) → 模型选分支 → 下钻/回溯/预算。
use crate::network::SkillNetwork;
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct NavFrame {
    pub skill: String,
    pub menu: String,
    pub siblings: Vec<String>,
    pub visited: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct NavigatorLimits {
    pub descend_budget: usize,
    pub backtrack_budget: usize,
    pub max_depth: usize,
}

impl Default for NavigatorLimits {
    fn default() -> Self {
        Self { descend_budget: 3, backtrack_budget: 2, max_depth: 6 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum NavAction {
    AtLeaf { body: String },
    Menu { name: String },
    BudgetExhausted,
}

pub struct SkillNavigator<'a> {
    pub network: &'a SkillNetwork,
    pub limits: NavigatorLimits,
}

impl<'a> SkillNavigator<'a> {
    /// 生成索引菜单:方向描述(description)+ 子 skill 列表(名 + 一句描述)。
    pub fn menu(&self, name: &str) -> String {
        let node = self.network.nodes.iter().find(|n| n.skill.name == name)
            .expect("skill exists in network");
        let mut out = format!("## {name}\n{}\n\n可选方向:\n", node.skill.description);
        for child in self.network.children_of(name) {
            out.push_str(&format!("- [{}] {} — {}\n", child.skill.name, child.skill.name, child.skill.description));
        }
        out
    }

    /// 下钻到 choice 分支。
    pub fn descend(&self, stack: &mut Vec<NavFrame>, choice: &str) -> Result<NavAction, String> {
        // visited 防绕圈:同一分支本任务已访问 → 报错让调用方回溯。
        let current_skill = {
            let current = stack.last().ok_or("empty nav stack")?;
            if current.visited.contains(choice) {
                return Err(format!("branch {choice} already visited in this task"));
            }
            current.skill.clone()
        };
        // 预算检查。
        if stack.len() >= self.limits.max_depth {
            return Ok(NavAction::BudgetExhausted);
        }
        let descend_used = stack.len(); // 根=0 次,每下钻一次 +1
        if descend_used >= self.limits.descend_budget {
            return Ok(NavAction::BudgetExhausted);
        }
        let child = self.network.nodes.iter().find(|n| n.skill.name == choice)
            .ok_or_else(|| format!("skill {choice} not found"))?;
        // 叶子 → 返回正文。
        if child.is_leaf {
            return Ok(NavAction::AtLeaf { body: child.skill.body.clone() });
        }
        // 索引 → 记录 visited 并 push 新 frame。
        let siblings: Vec<String> = self.network.children_of(&current_skill)
            .iter().map(|n| n.skill.name.clone()).collect();
        let mut visited = HashSet::new();
        visited.insert(choice.to_string());
        // 先把 choice 记进当前 frame 的 visited(跨回溯存活),再 push 新 frame:
        // 否则回溯回本层后重下钻同一分支会绕过 visited 检查(当前 frame 的 visited 为空)。
        if let Some(current) = stack.last_mut() {
            current.visited.insert(choice.to_string());
        }
        stack.push(NavFrame {
            skill: child.skill.name.clone(),
            menu: self.menu(&child.skill.name),
            siblings,
            visited,
        });
        Ok(NavAction::Menu { name: child.skill.name.clone() })
    }

    /// 回溯:pop 当前 frame,返回上层 frame 的菜单(展示兄弟分支)。
    pub fn backtrack(&self, stack: &mut Vec<NavFrame>) -> Result<NavAction, String> {
        if stack.len() <= 1 {
            return Err("already at root, cannot backtrack".to_string());
        }
        stack.pop();
        let parent = stack.last().ok_or("empty nav stack")?;
        Ok(NavAction::Menu { name: parent.skill.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::{NavAction, NavFrame, NavigatorLimits, SkillNavigator};
    use crate::model::Skill;
    use crate::network::SkillNetwork;
    use crate::store::SkillStore;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn skill_with(name: &str, desc: &str, cat: &str, body: &str) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            short_description: None,
            category: cat.into(),
            path: PathBuf::new(),
            body: body.into(),
            relations: vec![],
            triggers: vec![],
            tags: vec![],
            version: 1,
            confidence: 0.8,
            usage_count: 0,
            success_rate: 0.0,
            last_used: None,
            auto: false,
            origin: "manual".into(),
            origin_url: None,
            scope: "user".into(),
            allow_implicit: true,
            embedding: None,
        }
    }

    /// 构造 index→leaf 两级目录(react 索引,react.performance 叶子),返回 network 和导航器。
    fn index_leaf_network() -> (SkillNetwork, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        store.save(&skill_with("react", "react framework", "frontend", "INDEX_BODY")).unwrap();
        store.save(&skill_with("react.performance", "react performance", "frontend.react", "full body")).unwrap();
        (SkillNetwork::from_store(&store), dir)
    }

    fn frame(skill: &str, menu: String) -> NavFrame {
        NavFrame {
            skill: skill.into(),
            menu,
            siblings: vec![],
            visited: HashSet::new(),
        }
    }

    #[test]
    fn navigator_menu_lists_children_with_descriptions() {
        let (net, _dir) = index_leaf_network();
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };
        let m = nav.menu("react");
        // 包含子名 + 子 description,按 "[child] name — desc" 格式。
        assert!(m.contains("react.performance"), "menu must list child name");
        assert!(m.contains("react performance"), "menu must include child one-line description");
        assert!(m.contains("- [react.performance] react.performance — react performance"));
        // 不含子 body。
        assert!(!m.contains("full body"), "menu must not include child bodies");
        // 含索引自身的方向提示(description)。
        assert!(m.contains("react framework"), "menu must include the index description");
    }

    #[test]
    fn navigator_descend_leaf_returns_body() {
        let (net, _dir) = index_leaf_network();
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };
        let mut stack = vec![frame("react", nav.menu("react"))];
        let action = nav.descend(&mut stack, "react.performance").unwrap();
        assert_eq!(action, NavAction::AtLeaf { body: "full body".into() });
        assert_eq!(stack.len(), 1, "leaf descent must not push a frame");
    }

    #[test]
    fn navigator_descend_index_returns_menu_and_pushes_frame() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        // 根 a → 索引 b(含子叶子 d)+ 兄弟叶子 c。
        store.save(&skill_with("a", "root idx", "root", "A")).unwrap();
        store.save(&skill_with("b", "b idx", "root.a", "B")).unwrap();
        store.save(&skill_with("d", "d leaf", "root.a.b", "D BODY")).unwrap();
        store.save(&skill_with("c", "c leaf", "root.a", "C")).unwrap();
        let net = SkillNetwork::from_store(&store);
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };
        let mut stack = vec![frame("a", nav.menu("a"))];

        let action = nav.descend(&mut stack, "b").unwrap();
        assert_eq!(action, NavAction::Menu { name: "b".into() });
        assert_eq!(stack.len(), 2, "index descent must push a frame");
        let top = stack.last().unwrap();
        assert_eq!(top.skill, "b");
        // siblings = 父 a 的分支列表(含本次下钻的 b)——展示该层所有可选方向。
        let mut sib = top.siblings.clone();
        sib.sort();
        assert_eq!(sib, vec!["b".to_string(), "c".to_string()]);
        assert!(top.visited.contains("b"), "new frame records the branch taken");
        assert!(top.menu.contains("可选方向:"), "pushed frame carries its own menu");

        // 继续下钻到叶子。
        let action = nav.descend(&mut stack, "d").unwrap();
        assert_eq!(action, NavAction::AtLeaf { body: "D BODY".into() });
    }

    #[test]
    fn navigator_descend_unknown_skill_errors() {
        let (net, _dir) = index_leaf_network();
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };
        let mut stack = vec![frame("react", nav.menu("react"))];
        let err = nav.descend(&mut stack, "no.such.skill").unwrap_err();
        assert!(err.contains("no.such.skill"), "unexpected err: {err}");
    }

    #[test]
    fn navigator_descend_empty_stack_errors() {
        let (net, _dir) = index_leaf_network();
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };
        let mut stack: Vec<NavFrame> = Vec::new();
        assert!(nav.descend(&mut stack, "react.performance").is_err());
    }

    #[test]
    fn navigator_descend_budget_exhausts_after_3() {
        // 4 层索引链 a→b→c→d;descend_budget 3 → 第 3 次下钻 BudgetExhausted。
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        store.save(&skill_with("a", "a", "root", "A")).unwrap();
        store.save(&skill_with("b", "b", "root.a", "B")).unwrap();
        store.save(&skill_with("c", "c", "root.a.b", "C")).unwrap();
        store.save(&skill_with("d", "d", "root.a.b.c", "D")).unwrap();
        let net = SkillNetwork::from_store(&store);
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };

        let mut stack = vec![frame("a", nav.menu("a"))];
        assert!(matches!(nav.descend(&mut stack, "b").unwrap(), NavAction::Menu { .. }));
        assert!(matches!(nav.descend(&mut stack, "c").unwrap(), NavAction::Menu { .. }));
        // 第 3 次下钻超出 descend_budget(3)→ BudgetExhausted;max_depth 6 不触发。
        assert_eq!(nav.descend(&mut stack, "d").unwrap(), NavAction::BudgetExhausted);
        assert_eq!(stack.len(), 3, "budget-exhausted descent must not push a frame");
    }

    #[test]
    fn navigator_max_depth_limits_descent() {
        // 预算拉高,max_depth=2 → 到第 2 层后 BudgetExhausted。
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        store.save(&skill_with("a", "a", "root", "A")).unwrap();
        store.save(&skill_with("b", "b", "root.a", "B")).unwrap();
        store.save(&skill_with("c", "c", "root.a.b", "C")).unwrap();
        let net = SkillNetwork::from_store(&store);
        let nav = SkillNavigator {
            network: &net,
            limits: NavigatorLimits { descend_budget: 10, backtrack_budget: 2, max_depth: 2 },
        };
        let mut stack = vec![frame("a", nav.menu("a"))];
        assert!(matches!(nav.descend(&mut stack, "b").unwrap(), NavAction::Menu { .. }));
        assert_eq!(nav.descend(&mut stack, "c").unwrap(), NavAction::BudgetExhausted);
    }

    #[test]
    fn navigator_backtrack_pops_to_parent_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        // 根 a → b,c 两分支;b 是索引(有子 d)。
        store.save(&skill_with("a", "root", "root", "A")).unwrap();
        store.save(&skill_with("b", "b idx", "root.a", "B")).unwrap();
        store.save(&skill_with("d", "d leaf", "root.a.b", "D")).unwrap();
        store.save(&skill_with("c", "c leaf", "root.a", "C")).unwrap();
        let net = SkillNetwork::from_store(&store);
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };
        let mut stack = vec![frame("a", nav.menu("a"))];
        nav.descend(&mut stack, "b").unwrap();
        assert_eq!(stack.len(), 2);

        // 回溯 → 回到 a 的菜单,展示兄弟分支 b/c。
        let action = nav.backtrack(&mut stack).unwrap();
        assert_eq!(action, NavAction::Menu { name: "a".into() });
        assert_eq!(stack.len(), 1);
        let top = stack.last().unwrap();
        assert_eq!(top.skill, "a");
        assert!(top.menu.contains("b"), "parent menu shows sibling b");
        assert!(top.menu.contains("c"), "parent menu shows sibling c");
    }

    #[test]
    fn navigator_backtrack_at_root_errors() {
        let (net, _dir) = index_leaf_network();
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };
        let mut stack = vec![frame("react", nav.menu("react"))];
        assert!(nav.backtrack(&mut stack).is_err(), "cannot backtrack above root");
    }

    #[test]
    fn navigator_visited_prevents_redescend() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        // b 是索引(有子叶子 c):只有下钻索引才 push frame 并记录 visited。
        store.save(&skill_with("a", "a", "root", "A")).unwrap();
        store.save(&skill_with("b", "b idx", "root.a", "B")).unwrap();
        store.save(&skill_with("c", "c leaf", "root.a.b", "C")).unwrap();
        let net = SkillNetwork::from_store(&store);
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };
        let mut stack = vec![frame("a", nav.menu("a"))];
        let action = nav.descend(&mut stack, "b").unwrap();
        assert!(matches!(action, NavAction::Menu { .. }));
        assert_eq!(stack.len(), 2);

        // 当前 frame 的 visited 记录已下钻的 b → 再下钻 b 报错,让调用方回溯。
        let err = nav.descend(&mut stack, "b").unwrap_err();
        assert!(err.contains("already visited"), "unexpected err: {err}");
    }

    #[test]
    fn navigator_visited_survives_backtrack() {
        // 下钻 b(索引)→ 回溯回 a → 再下钻 b:visited 必须随当前 frame 存活,
        // 否则重下钻同一分支被放行(防绕圈失效)。
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path());
        store.save(&skill_with("a", "root", "root", "A")).unwrap();
        store.save(&skill_with("b", "b idx", "root.a", "B")).unwrap();
        store.save(&skill_with("d", "d leaf", "root.a.b", "D")).unwrap();
        store.save(&skill_with("c", "c leaf", "root.a", "C")).unwrap();
        let net = SkillNetwork::from_store(&store);
        let nav = SkillNavigator { network: &net, limits: NavigatorLimits::default() };
        let mut stack = vec![frame("a", nav.menu("a"))];

        nav.descend(&mut stack, "b").unwrap();
        assert_eq!(stack.len(), 2);
        // 回溯回 a(当前 frame 的 visited 应保留 b)。
        nav.backtrack(&mut stack).unwrap();
        assert_eq!(stack.len(), 1);
        assert!(
            stack.last().unwrap().visited.contains("b"),
            "visited must survive backtrack"
        );
        // 重下钻同一分支 b → 被 visited 拦截。
        let err = nav.descend(&mut stack, "b").unwrap_err();
        assert!(err.contains("already visited"), "unexpected err: {err}");
    }
}
