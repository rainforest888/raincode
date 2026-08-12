//! Greedy similarity clustering for cross-session experience records.
use rc_skill::cosine;

#[derive(Debug, Clone)]
pub struct Cluster {
    pub record_ids: Vec<String>,
    pub session_ids: Vec<String>,
    pub task_signatures: Vec<String>,
    pub centroid: Option<Vec<f32>>,
    pub category_guess: String,
}

/// Connected components over the cosine similarity graph: every record joins
/// the cluster of its nearest above-threshold neighbor. This is the v0.1
/// stand-in for full online clustering.
pub fn greedy_clusters(
    ids: &[String],
    session_ids: &[String],
    task_signatures: &[String],
    category_guesses: &[String],
    embeddings: &[Vec<f32>],
    threshold: f32,
) -> Vec<Cluster> {
    let n = ids.len();
    let mut assigned = vec![false; n];
    let mut clusters = Vec::new();
    for i in 0..n {
        if assigned[i] {
            continue;
        }
        let mut members = vec![i];
        assigned[i] = true;
        let mut changed = true;
        while changed {
            changed = false;
            if let Some(ref centroid) = centroid_of(embeddings, &members) {
                for j in 0..n {
                    if !assigned[j] && cosine(centroid, &embeddings[j]) >= threshold {
                        members.push(j);
                        assigned[j] = true;
                        changed = true;
                    }
                }
            }
        }
        clusters.push(Cluster {
            record_ids: members.iter().map(|&m| ids[m].clone()).collect(),
            session_ids: members.iter().map(|&m| session_ids[m].clone()).collect(),
            task_signatures: members
                .iter()
                .map(|&m| task_signatures[m].clone())
                .collect(),
            centroid: centroid_of(embeddings, &members),
            category_guess: members
                .iter()
                .map(|&m| category_guesses[m].clone()).find(|c| !c.is_empty())
                .unwrap_or_default(),
        });
    }
    clusters
}

fn centroid_of(embeddings: &[Vec<f32>], members: &[usize]) -> Option<Vec<f32>> {
    let mut sum: Vec<f32> = Vec::new();
    for &m in members {
        let e = &embeddings[m];
        if sum.is_empty() {
            sum = e.clone();
        } else {
            for (a, b) in sum.iter_mut().zip(e.iter()) {
                *a += b;
            }
        }
    }
    if sum.is_empty() {
        return None;
    }
    let len = members.len() as f32;
    Some(sum.into_iter().map(|x| x / len).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clusters_similar_records() {
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let sessions = vec!["s1".into(), "s2".into(), "s3".into()];
        let tasks = vec![
            "fix pytest".into(),
            "pytest flaky".into(),
            "docker debug".into(),
        ];
        let cats = vec![
            "testing.pytest".into(),
            "testing.pytest".into(),
            "containers.docker".into(),
        ];
        let embs = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.95, 0.05, 0.0],
            vec![0.0, 1.0, 0.0],
        ];
        let clusters = greedy_clusters(&ids, &sessions, &tasks, &cats, &embs, 0.8);
        assert_eq!(clusters.len(), 2);
        let pytest = clusters.iter().find(|c| c.record_ids.len() == 2).unwrap();
        assert_eq!(pytest.session_ids.len(), 2);
    }
}
