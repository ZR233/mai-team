use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use mai_protocol::ProjectId;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Default)]
pub(crate) struct ProjectWorkspaceLocks {
    locks: StdMutex<HashMap<ProjectId, Arc<Mutex<()>>>>,
}

impl ProjectWorkspaceLocks {
    pub(crate) async fn lock(&self, project_id: ProjectId) -> ProjectWorkspaceLease {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .expect("project workspace lock registry poisoned");
            Arc::clone(
                locks
                    .entry(project_id)
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        ProjectWorkspaceLease {
            _guard: lock.lock_owned().await,
        }
    }
}

pub(crate) struct ProjectWorkspaceLease {
    _guard: OwnedMutexGuard<()>,
}
