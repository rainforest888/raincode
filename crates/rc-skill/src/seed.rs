//! First-run installation of the bundled seed skill corpus.
use crate::model::Skill;
use std::path::Path;

/// Copy every seed skill directory into `dest/.seed`, force `scope=system`
/// and `origin=seed`, then write them back as validated SKILL.md files.
/// Returns the installed skill names.
pub fn install_seed(seed_root: &Path, dest: &Path) -> Result<Vec<String>, String> {
    let seed_dest = dest.join(".seed");
    std::fs::create_dir_all(&seed_dest).map_err(|e| e.to_string())?;
    let mut installed = Vec::new();
    for entry in std::fs::read_dir(seed_root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let md = entry.path().join("SKILL.md");
        if !md.exists() {
            continue;
        }
        let mut skill = Skill::from_path(&md)
            .map_err(|e| format!("seed {}: {e}", entry.file_name().to_string_lossy()))?;
        skill.scope = "system".into();
        skill.origin = "seed".into();
        skill.auto = false;
        let target = seed_dest.join(&skill.name);
        std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
        std::fs::write(
            target.join("SKILL.md"),
            skill.render().map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        installed.push(skill.name);
    }
    Ok(installed)
}

/// Returns true if seed skills are already present.
pub fn seed_installed(dest: &Path) -> bool {
    let seed_dest = dest.join(".seed");
    if !seed_dest.exists() {
        return false;
    }
    std::fs::read_dir(&seed_dest)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_install_marks_system_scope() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        std::fs::create_dir_all(seed.join("read-then-edit")).unwrap();
        std::fs::write(
            seed.join("read-then-edit").join("SKILL.md"),
            "---\nname: read-then-edit\ndescription: read before editing\ncategory: workflow\n---\nRead first.",
        )
        .unwrap();
        let dest = dir.path().join("dest");
        let installed = install_seed(&seed, &dest).unwrap();
        assert_eq!(installed, vec!["read-then-edit"]);
        let loaded =
            Skill::from_path(dest.join(".seed").join("read-then-edit").join("SKILL.md")).unwrap();
        assert_eq!(loaded.scope, "system");
        assert_eq!(loaded.origin, "seed");
        assert!(seed_installed(&dest));
    }
}
