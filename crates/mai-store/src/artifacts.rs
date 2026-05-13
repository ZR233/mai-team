use crate::*;

impl ConfigStore {
    pub fn save_artifact(&self, info: &ArtifactInfo) -> Result<()> {
        let dir = self.artifact_index_dir();
        std::fs::create_dir_all(dir)?;
        let file = dir.join(format!("{}.json", info.id));
        let data = serde_json::to_string(info)?;
        std::fs::write(file, data)?;
        Ok(())
    }

    pub fn load_artifacts(&self, task_id: &TaskId) -> Result<Vec<ArtifactInfo>> {
        let dir = self.artifact_index_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let data = std::fs::read_to_string(&path)?;
            let info: ArtifactInfo = serde_json::from_str(&data)?;
            if info.task_id == *task_id {
                result.push(info);
            }
        }
        result.sort_by_key(|a| a.created_at);
        Ok(result)
    }

    pub fn load_all_artifacts(&self) -> Result<Vec<ArtifactInfo>> {
        let dir = self.artifact_index_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let data = std::fs::read_to_string(&path)?;
            let info: ArtifactInfo = serde_json::from_str(&data)?;
            result.push(info);
        }
        result.sort_by_key(|a| a.created_at);
        Ok(result)
    }
}
