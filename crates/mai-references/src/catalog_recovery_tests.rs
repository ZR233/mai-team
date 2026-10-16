//! 验证登记不会被验证并发额度阻塞，且崩溃后的 Validating 记录能重新进入真实扫描。
use super::*;
use pretty_assertions::assert_eq;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    time::Duration,
};

#[test]
fn catalog_register_returns_durable_validating_and_recovers_after_runtime_drop() {
    // 临时夹具由 tempfile 隔离。此处检验调度与恢复，祖先目录的信任校验由独立
    // 文件系统验收覆盖；工作树祖先可能允许组写入，不能依赖开发机权限布局。
    let test_directory = std::env::current_dir().unwrap().canonicalize().unwrap();
    let workspace = tempfile::tempdir_in(test_directory).unwrap();
    let data_dir = workspace.path().join("data");
    let source_root = workspace.path().join("references").join("linux-v1");
    fs::create_dir_all(&source_root).unwrap();
    let source_file = source_root.join("driver.c");
    let source_content = b"int fixed_reference(void) { return 1; }\n";
    fs::write(&source_file, source_content).unwrap();

    // 测试进程可能由普通用户运行；策略信任目录的真实持有者，而不是假设 root。
    fs::set_permissions(&source_file, fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(&source_root, fs::Permissions::from_mode(0o755)).unwrap();
    let allowed_root = source_root.parent().unwrap().to_path_buf();
    fs::set_permissions(&allowed_root, fs::Permissions::from_mode(0o755)).unwrap();
    let source_metadata = fs::metadata(&source_root).unwrap();
    let policy = ReferenceDeploymentPolicy::new(vec![allowed_root])
        .unwrap()
        .with_trusted_owner_uids(vec![0, source_metadata.uid()])
        .with_source_trust_requirement(SourceTrustRequirement::Unrestricted);
    let request = NewReferenceSourceRequest {
        name: "Linux 固定参考源".to_string(),
        host_path: source_root.to_string_lossy().into_owned(),
        revision_label: "v1".to_string(),
        approved_for_agent_access: true,
    };

    // 第一个运行时占满唯一的验证槽。register 只写入 Validating 并调度 worker，
    // 不能等待槽位；查询接口也必须在这个阶段保持可用。
    let first_runtime = tokio::runtime::Runtime::new().unwrap();
    let (source_id, durable_validating, first_catalog, first_store, held_permit) = first_runtime
        .block_on(async {
            let store = Arc::new(MaiStore::open_in_data_dir(&data_dir).await.unwrap());
            let catalog = ReferenceCatalog::new(Arc::clone(&store), Some(policy.clone()));
            let permit = catalog
                .validation
                .clone()
                .acquire_owned()
                .await
                .expect("占用验证槽");

            let registered = catalog.register(request.clone()).await.unwrap();
            assert_eq!(SourceState::Validating, registered.state);
            assert_eq!(catalog.source(registered.id).await.unwrap(), registered);
            assert_eq!(
                catalog.sources().await.unwrap(),
                Vec::from([registered.clone()])
            );
            assert_eq!(catalog.deployment_policy().unwrap().clone(), policy.clone());

            (registered.id, registered, catalog, store, permit)
        });

    // 先关闭运行时，取消仍在等待验证槽的 worker；随后释放旧目录与数据库句柄，
    // 形成“进程带着 durable Validating 记录崩溃”的边界。
    drop(first_runtime);
    drop(held_permit);
    drop(first_catalog);
    drop(first_store);

    // 第二个运行时重新打开同一数据库。Ready 只能由 resume 出来的真实验证 worker
    // 写入；这里轮询数据库状态，时限只是防挂保护，不把耗时当作成功条件。
    let second_runtime = tokio::runtime::Runtime::new().unwrap();
    second_runtime.block_on(async {
        let store = Arc::new(MaiStore::open_in_data_dir(&data_dir).await.unwrap());
        let catalog = ReferenceCatalog::new(Arc::clone(&store), Some(policy.clone()));
        assert_eq!(catalog.source(source_id).await.unwrap(), durable_validating);
        catalog.resume_validations().await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let recovered = loop {
            let source = catalog.source(source_id).await.unwrap();
            match source.state {
                SourceState::Ready => break source,
                SourceState::Validating => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "恢复的验证 worker 未在时限内完成真实扫描"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                SourceState::Invalid | SourceState::Disabled | SourceState::Missing => {
                    panic!("恢复验证进入失败终态：{:?}", source.state)
                }
            }
        };

        // 用独立真实验证器重新读取同一个目录，得到期望身份与内容摘要；不从测试
        // 手写完成状态或 digest，只把恢复 worker 的持久化结果与真实扫描对比。
        let independently_validated = validate_new_source(&policy, &request, Utc::now()).unwrap();
        assert_eq!(SourceState::Ready, recovered.state);
        assert_eq!(SourceProvenance::directory_snapshot(), recovered.provenance);
        assert_eq!(independently_validated.file_count(), recovered.file_count);
        assert_eq!(independently_validated.entry_count(), recovered.file_count);
        assert_eq!(source_content.len() as u64, recovered.byte_count);
        assert_eq!(
            independently_validated.identity().device(),
            recovered.source_device
        );
        assert_eq!(
            independently_validated.identity().inode(),
            recovered.source_inode
        );
        assert_eq!(
            Some(independently_validated.manifest_digest().to_string()),
            recovered.manifest_digest
        );
        assert_eq!(
            independently_validated.warnings(),
            recovered.warnings.as_slice()
        );
        assert_eq!(catalog.source(source_id).await.unwrap(), recovered);
        assert_eq!(catalog.sources().await.unwrap(), Vec::from([recovered]));
    });

    // TempDir 必须存活到两个运行时都完成，避免源码目录在恢复验证前被清理。
    drop(workspace);
}
