//! 持久阶段的只读观察点；运行时默认不安装观察者，隔离验收可在精确边界结束进程。

use mai_store::PersistedReferenceOperation;

/// 只观察已经提交的收据，不可修改配置、释放准入或取代资源所有者。
/// 回调必须短暂且同步；跨进程崩溃验收可在该边界终止自己的测试进程。
pub trait ReferenceApplicationObserver: Send + Sync {
    fn recorded(&self, receipt: &PersistedReferenceOperation);
}
