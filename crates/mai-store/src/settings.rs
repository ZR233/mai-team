use crate::records::*;
use crate::*;

impl ConfigStore {
    pub async fn list_mcp_servers(&self) -> Result<BTreeMap<String, McpServerConfig>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<McpServerRecord>>::all().exec(&mut db).await?;
        rows.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.name.cmp(&right.name))
        });

        let mut servers = BTreeMap::new();
        for row in rows {
            let mut config = serde_json::from_str::<McpServerConfig>(&row.config_json)?;
            config.enabled = row.enabled;
            servers.insert(row.name.clone(), config);
        }
        Ok(servers)
    }

    pub async fn save_mcp_servers(
        &self,
        servers: &BTreeMap<String, McpServerConfig>,
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<McpServerRecord>>::all()
            .delete()
            .exec(&mut tx)
            .await?;

        for (index, (name, config)) in servers.iter().enumerate() {
            toasty::create!(McpServerRecord {
                name: name.clone(),
                config_json: serde_json::to_string(config)?,
                enabled: config.enabled,
                sort_order: index as i64,
            })
            .exec(&mut tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        get_setting_on(&self.db, key).await
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let mut db = self.db.clone();
        set_setting_on(&mut db, key, value).await
    }

    pub async fn load_agent_config(&self) -> Result<AgentConfigRequest> {
        let Some(value) = self.get_setting(SETTING_AGENT_CONFIG).await? else {
            return Ok(AgentConfigRequest::default());
        };
        match serde_json::from_str(&value) {
            Ok(config) => Ok(config),
            Err(_) => {
                let mut db = self.db.clone();
                let mut tx = db.transaction().await?;
                delete_setting_in_tx(&mut tx, SETTING_AGENT_CONFIG).await?;
                tx.commit().await?;
                Ok(AgentConfigRequest::default())
            }
        }
    }

    pub async fn save_agent_config(&self, config: &AgentConfigRequest) -> Result<()> {
        self.set_setting(SETTING_AGENT_CONFIG, &serde_json::to_string(config)?)
            .await
    }

    pub async fn load_skills_config(&self) -> Result<SkillsConfigRequest> {
        let Some(value) = self.get_setting(SETTING_SKILLS_CONFIG).await? else {
            return Ok(SkillsConfigRequest::default());
        };
        match serde_json::from_str(&value) {
            Ok(config) => Ok(config),
            Err(_) => {
                let mut db = self.db.clone();
                let mut tx = db.transaction().await?;
                delete_setting_in_tx(&mut tx, SETTING_SKILLS_CONFIG).await?;
                tx.commit().await?;
                Ok(SkillsConfigRequest::default())
            }
        }
    }

    pub async fn save_skills_config(&self, config: &SkillsConfigRequest) -> Result<()> {
        self.set_setting(SETTING_SKILLS_CONFIG, &serde_json::to_string(config)?)
            .await
    }

    pub async fn get_github_settings(&self) -> Result<GithubSettingsResponse> {
        let has_token = self.get_setting(SETTING_GITHUB_TOKEN).await?.is_some();
        Ok(GithubSettingsResponse { has_token })
    }

    pub async fn save_github_token(&self, token: &str) -> Result<GithubSettingsResponse> {
        self.set_setting(SETTING_GITHUB_TOKEN, token).await?;
        Ok(GithubSettingsResponse { has_token: true })
    }

    pub async fn clear_github_token(&self) -> Result<GithubSettingsResponse> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_setting_in_tx(&mut tx, SETTING_GITHUB_TOKEN).await?;
        tx.commit().await?;
        Ok(GithubSettingsResponse { has_token: false })
    }
}

pub(crate) async fn get_setting_on(db: &Db, key: &str) -> Result<Option<String>> {
    let mut db = db.clone();
    let row =
        Query::<List<SettingRecord>>::filter(SettingRecord::fields().key().eq(key.to_string()))
            .first()
            .exec(&mut db)
            .await?;
    Ok(row.map(|row| row.value))
}

pub(crate) async fn set_setting_on(db: &mut Db, key: &str, value: &str) -> Result<()> {
    let mut tx = db.transaction().await?;
    set_setting_in_tx(&mut tx, key, value).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn set_setting_in_tx(
    tx: &mut toasty::Transaction<'_>,
    key: &str,
    value: &str,
) -> Result<()> {
    delete_setting_in_tx(tx, key).await?;
    toasty::create!(SettingRecord {
        key: key.to_string(),
        value: value.to_string(),
    })
    .exec(tx)
    .await?;
    Ok(())
}

pub(crate) async fn delete_setting_in_tx(
    tx: &mut toasty::Transaction<'_>,
    key: &str,
) -> Result<()> {
    Query::<List<SettingRecord>>::filter(SettingRecord::fields().key().eq(key.to_string()))
        .delete()
        .exec(tx)
        .await?;
    Ok(())
}
