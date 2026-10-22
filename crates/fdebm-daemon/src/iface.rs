//! D-Bus 契约层(org.fdebm):GUI 瘦客户端与守护进程的唯一接口。
//!
//! 方法:
//! - AnalyzeLocalFile(s path, as apt_opts) → (b installable, s failure,
//!   as install, as remove, as unauthenticated, s installed_version,
//!   a(ss) conffile_conflicts)
//! - InstallLocalFile(s path, as apt_opts, s conf_policy) → u id
//! - RemovePackage(s name) → u id
//! - Cancel(u id) → b accepted
//! - GetTransactionStatus(u id) → (s state, s error)
//!
//! 信号见 txn.rs 头注释。分析为只读操作,免 Polkit 授权。

use std::sync::Arc;

use zbus::message::Header;

use crate::txn::{self, Manager};

pub struct FdebmIface {
    manager: Arc<Manager>,
}

impl FdebmIface {
    pub fn new(manager: Arc<Manager>) -> Self {
        Self { manager }
    }

    fn caller(header: &Header<'_>) -> Option<String> {
        header
            .sender()
            .map(|s| s.as_str().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[zbus::interface(name = "org.fdebm")]
impl FdebmIface {
    async fn analyze_local_file(
        &self,
        path: String,
        apt_opts: Vec<String>,
    ) -> zbus::fdo::Result<fdebm_core::AnalyzeResult> {
        // 求解是重活(索引解析),放独立线程避免阻塞总线 executor
        let manager = self.manager.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            // send 是 async 方法,同步线程内必须用 send_blocking:
            // let _ = tx.send(..) 只创建 future 即丢弃,消息永远不会入队
            let _ = tx.send_blocking(txn::analyze(&manager, &path, &apt_opts));
        });
        rx.recv()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    async fn install_local_file(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        path: String,
        apt_opts: Vec<String>,
        conf_policy: String,
    ) -> zbus::fdo::Result<u32> {
        let caller = Self::caller(&header)
            .ok_or_else(|| zbus::fdo::Error::Failed("unknown caller".to_string()))?;
        Ok(txn::start_install(
            &self.manager,
            conn,
            path,
            apt_opts,
            conf_policy,
            caller,
        ))
    }

    async fn remove_package(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        name: String,
    ) -> zbus::fdo::Result<u32> {
        let caller = Self::caller(&header)
            .ok_or_else(|| zbus::fdo::Error::Failed("unknown caller".to_string()))?;
        Ok(txn::start_remove(&self.manager, conn, name, caller))
    }

    async fn cancel(&self, id: u32) -> zbus::fdo::Result<bool> {
        Ok(self.manager.cancel(id))
    }

    async fn get_transaction_status(&self, id: u32) -> zbus::fdo::Result<(String, String)> {
        self.manager
            .status(id)
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("unknown transaction {id}")))
    }
}
