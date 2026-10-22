//! 事务管理器:状态机 + 工作线程 + 信号发射。
//!
//! 状态机(对应 readme:分析中→等待授权→下载中→安装中→完成/失败):
//! analyzing → waiting-auth → downloading → installing → done | failed | cancelled
//! 信号契约(org.gdebi.Rs):
//! - TransactionEvent(u32 id, String state)
//! - OutputLine(u32 id, String line)
//! - ProgressChanged(u32 id, f64 percent /*-1=pulse*/, String description)
//! - Finished(u32 id, bool ok, String detail)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use gdebi_core::download::{self, DownloadProgress};
use gdebi_core::install::{self, CancelHandle, InstallBatch};
use gdebi_core::i18n::tf;
use gdebi_core::solver::{self, Verdict};
use gdebi_core::sysstate::SysState;
use gdebi_core::ui::{AlertKind, InstallProgress, Ui};
use gdebi_core::{DebFile, GdebiError};

use crate::polkit;

pub const ACTIONS: &str = "org.gdebi.Rs";

pub const ANALYZING: &str = "analyzing";
pub const WAITING_AUTH: &str = "waiting-auth";
pub const DOWNLOADING: &str = "downloading";
pub const INSTALLING: &str = "installing";
pub const DONE: &str = "done";
pub const FAILED: &str = "failed";
pub const CANCELLED: &str = "cancelled";

struct Txn {
    state: Mutex<(String, String)>,
    cancel: Arc<CancelHandle>,
}

struct Inner {
    next_id: AtomicU32,
    txns: Mutex<HashMap<u32, Arc<Txn>>>,
}

pub struct Manager {
    inner: Inner,
    /// SysState 缓存:(status mtime, lists mtime, state);apt_opts 非空时绕过
    sys_cache: Mutex<Option<(SystemTime, SystemTime, Arc<SysState>)>>,
}

impl Manager {
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self {
            inner: Inner {
                next_id: AtomicU32::new(1),
                txns: Mutex::new(HashMap::new()),
            },
            sys_cache: Mutex::new(None),
        })
    }

    fn alloc(&self, cancel: Arc<CancelHandle>) -> u32 {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        self.inner.txns.lock().unwrap().insert(
            id,
            Arc::new(Txn {
                state: Mutex::new((ANALYZING.to_string(), String::new())),
                cancel,
            }),
        );
        id
    }

    pub fn status(&self, id: u32) -> Option<(String, String)> {
        let txns = self.inner.txns.lock().unwrap();
        let txn = txns.get(&id)?;
        Some(txn.state.lock().unwrap().clone())
    }

    pub fn cancel(&self, id: u32) -> bool {
        let txns = self.inner.txns.lock().unwrap();
        match txns.get(&id) {
            Some(txn) => {
                txn.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// 系统状态缓存:dpkg/status 与 lists 目录 mtime 不变时复用
    fn sys_state(&self, apt_opts: &[String]) -> Result<Arc<SysState>, GdebiError> {
        if apt_opts.is_empty() {
            let st_mtime = std::fs::metadata(gdebi_core::sysstate::DPKG_STATUS)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let li_mtime = std::fs::metadata(gdebi_core::sysstate::LISTS_DIR)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let mut cache = self.sys_cache.lock().unwrap();
            if let Some((st, li, state)) = cache.as_ref() {
                if *st == st_mtime && *li == li_mtime {
                    return Ok(state.clone());
                }
            }
            let state = Arc::new(SysState::load(apt_opts)?);
            *cache = Some((st_mtime, li_mtime, state.clone()));
            Ok(state)
        } else {
            Ok(Arc::new(SysState::load(apt_opts)?))
        }
    }
}

// ---------------------------------------------------------------------------
// 信号发射
// ---------------------------------------------------------------------------

/// 统一信号发射:路径/接口固定为 /org/gdebi/Rs @ org.gdebi.Rs
fn emit<T>(conn: &zbus::Connection, signal: &str, body: &T)
where
    T: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    let path = zbus::zvariant::ObjectPath::try_from("/org/gdebi/Rs").unwrap();
    let iface = zbus::names::InterfaceName::try_from(ACTIONS).unwrap();
    let _ = async_io::block_on(conn.emit_signal(None::<&str>, &path, &iface, signal, body));
}

fn emit_event(conn: &zbus::Connection, id: u32, state: &str) {
    emit(conn, "TransactionEvent", &(id, state.to_string()));
}

fn emit_finished(conn: &zbus::Connection, id: u32, ok: bool, detail: &str) {
    emit(conn, "Finished", &(id, ok, detail.to_string()));
}

fn emit_progress(conn: &zbus::Connection, id: u32, percent: f64, desc: &str) {
    emit(conn, "ProgressChanged", &(id, percent, desc.to_string()));
}

/// 终态落账 + Finished 信号
fn finalize(conn: &zbus::Connection, txn: &Txn, state: &str, detail: &str, id: u32, ok: bool) {
    *txn.state.lock().unwrap() = (state.to_string(), detail.to_string());
    emit_event(conn, id, state);
    emit_finished(conn, id, ok, detail);
}

// ---------------------------------------------------------------------------
// 事务启动(D-Bus 方法调用入口)
// ---------------------------------------------------------------------------

pub fn start_install(
    manager: &Arc<Manager>,
    conn: &zbus::Connection,
    path: String,
    apt_opts: Vec<String>,
    caller: String,
) -> u32 {
    let cancel = Arc::new(CancelHandle::new());
    let id = manager.alloc(cancel.clone());
    let m = manager.clone();
    let conn = conn.clone();
    std::thread::spawn(move || {
        let txn = m.inner.txns.lock().unwrap().get(&id).cloned();
        let Some(txn) = txn else { return };
        run_install(&m, &conn, &txn, id, &path, &apt_opts, &caller);
    });
    id
}

pub fn start_remove(
    manager: &Arc<Manager>,
    conn: &zbus::Connection,
    name: String,
    caller: String,
) -> u32 {
    let cancel = Arc::new(CancelHandle::new());
    let id = manager.alloc(cancel.clone());
    let m = manager.clone();
    let conn = conn.clone();
    std::thread::spawn(move || {
        let txn = m.inner.txns.lock().unwrap().get(&id).cloned();
        let Some(txn) = txn else { return };
        run_remove(&m, &conn, &txn, id, &name, &caller);
    });
    id
}

/// 只读分析(D-Bus 方法内同步执行,重活放独立线程避免阻塞总线任务)
pub fn analyze(
    manager: &Arc<Manager>,
    path: &str,
    apt_opts: &[String],
) -> Result<(bool, String, Vec<String>, Vec<String>, Vec<String>, String), String> {
    let run = || -> Result<_, GdebiError> {
        let deb = DebFile::open(path)?;
        let sys = manager.sys_state(apt_opts)?;
        let name = deb.field_trimmed("Package").unwrap_or("").to_string();
        let arch = deb
            .field_trimmed("Architecture")
            .unwrap_or("all")
            .to_string();
        let key = if arch == "all" || arch == sys.native_arch {
            name.clone()
        } else {
            format!("{}:{}", name, arch)
        };
        let installed_version = sys
            .installed_version(&key)
            .unwrap_or_default()
            .to_string();
        Ok(match solver::resolve(&sys, &deb)? {
            Verdict::Installable(res) => (
                true,
                String::new(),
                res.changes.install,
                res.changes.remove,
                res.changes.unauthenticated,
                installed_version,
            ),
            Verdict::Uninstallable(failure) => {
                (false, failure, Vec::new(), Vec::new(), Vec::new(), installed_version)
            }
        })
    };
    run().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 工作线程流程
// ---------------------------------------------------------------------------

fn run_install(
    manager: &Arc<Manager>,
    conn: &zbus::Connection,
    txn: &Arc<Txn>,
    id: u32,
    path: &str,
    apt_opts: &[String],
    caller: &str,
) {
    // 1. 解析 + 求解
    let deb = match DebFile::open(path) {
        Ok(d) => d,
        Err(e) => {
            finalize(conn, txn, FAILED, &e.to_string(), id, false);
            return;
        }
    };
    emit(conn, "TransactionEvent", &(id, ANALYZING.to_string()));
    let sys = match manager.sys_state(apt_opts) {
        Ok(s) => s,
        Err(e) => {
            finalize(conn, txn, FAILED, &e.to_string(), id, false);
            return;
        }
    };
    let resolution = match solver::resolve(&sys, &deb) {
        Ok(Verdict::Installable(r)) => r,
        Ok(Verdict::Uninstallable(failure)) => {
            finalize(conn, txn, FAILED, &failure, id, false);
            return;
        }
        Err(e) => {
            finalize(conn, txn, FAILED, &e.to_string(), id, false);
            return;
        }
    };

    // 2. Polkit 授权(GUI 瘦客户端,认证由守护进程发起)
    *txn.state.lock().unwrap() = (WAITING_AUTH.to_string(), String::new());
    emit(conn, "TransactionEvent", &(id, WAITING_AUTH.to_string()));
    let auth = async_io::block_on(polkit::check_authorized(
        conn,
        caller,
        "org.gdebi.rs.install-local-package",
    ));
    match auth {
        Ok(true) => {}
        Ok(false) => {
            finalize(conn, txn, FAILED, "Not authorized", id, false);
            return;
        }
        Err(e) => {
            finalize(conn, txn, FAILED, &format!("polkit: {e}"), id, false);
            return;
        }
    }

    // 3. 下载依赖(求解器给出坐标;无依赖则跳过)
    if !resolution.downloads.is_empty() {
        *txn.state.lock().unwrap() = (DOWNLOADING.to_string(), String::new());
        emit(conn, "TransactionEvent", &(id, DOWNLOADING.to_string()));
        let dp = TxnProgress {
            id,
            conn: conn.clone(),
        };
        if let Err(e) = download::download_all(&resolution.downloads, &dp, &txn.cancel) {
            let (state, ok) = match e {
                GdebiError::Cancelled => (CANCELLED, false),
                _ => (FAILED, false),
            };
            finalize(conn, txn, state, &e.to_string(), id, ok);
            return;
        }
    }

    // 4. dpkg 执行层
    *txn.state.lock().unwrap() = (INSTALLING.to_string(), String::new());
    emit(conn, "TransactionEvent", &(id, INSTALLING.to_string()));
    let batch = InstallBatch {
        local: PathBuf::from(path),
        dep_debs: resolution
            .downloads
            .iter()
            .map(|d| d.dest.clone())
            .collect(),
        remove: resolution.changes.remove.clone(),
    };
    let ui = TxnUi {
        id,
        conn: conn.clone(),
    };
    let progress = TxnProgress {
        id,
        conn: conn.clone(),
    };
    match install::install(&ui, &progress, &txn.cancel, &batch) {
        Ok(0) => finalize(conn, txn, DONE, "", id, true),
        Ok(code) => finalize(
            conn,
            txn,
            FAILED,
            &tf("dpkg exited with status {0}", &[&code.to_string()]),
            id,
            false,
        ),
        Err(e) => {
            let (state, ok) = match e {
                GdebiError::Cancelled => (CANCELLED, false),
                _ => (FAILED, false),
            };
            finalize(conn, txn, state, &e.to_string(), id, ok);
        }
    }
}

fn run_remove(
    manager: &Arc<Manager>,
    conn: &zbus::Connection,
    txn: &Arc<Txn>,
    id: u32,
    name: &str,
    caller: &str,
) {
    let _ = manager;
    *txn.state.lock().unwrap() = (WAITING_AUTH.to_string(), String::new());
    emit(conn, "TransactionEvent", &(id, WAITING_AUTH.to_string()));
    let auth = async_io::block_on(polkit::check_authorized(
        conn,
        caller,
        "org.gdebi.rs.remove-package",
    ));
    match auth {
        Ok(true) => {}
        Ok(false) => {
            finalize(conn, txn, FAILED, "Not authorized", id, false);
            return;
        }
        Err(e) => {
            finalize(conn, txn, FAILED, &format!("polkit: {e}"), id, false);
            return;
        }
    }
    *txn.state.lock().unwrap() = (INSTALLING.to_string(), String::new());
    emit(conn, "TransactionEvent", &(id, INSTALLING.to_string()));
    let ui = TxnUi {
        id,
        conn: conn.clone(),
    };
    let progress = TxnProgress {
        id,
        conn: conn.clone(),
    };
    match install::remove(&ui, &progress, &txn.cancel, name) {
        Ok(0) => finalize(conn, txn, DONE, "", id, true),
        Ok(code) => finalize(
            conn,
            txn,
            FAILED,
            &tf("dpkg exited with status {0}", &[&code.to_string()]),
            id,
            false,
        ),
        Err(e) => {
            let (state, ok) = match e {
                GdebiError::Cancelled => (CANCELLED, false),
                _ => (FAILED, false),
            };
            finalize(conn, txn, state, &e.to_string(), id, ok);
        }
    }
}

// ---------------------------------------------------------------------------
// 进度/UI 适配器:core 回调 → D-Bus 信号
// ---------------------------------------------------------------------------

struct TxnProgress {
    id: u32,
    conn: zbus::Connection,
}

impl InstallProgress for TxnProgress {
    fn output_line(&self, line: &str) {
        emit(&self.conn, "OutputLine", &(self.id, line.to_string()));
    }
}

impl DownloadProgress for TxnProgress {
    fn item_start(&self, name: &str, idx: usize, total: usize) {
        emit_progress(
            &self.conn,
            self.id,
            -1.0,
            &tf("Fetching file {0} of {1}", &[name, &total.to_string()]),
        );
        let _ = idx;
    }

    fn item_progress(&self, name: &str, received: u64, size: u64) {
        let percent = if size > 0 {
            received as f64 * 100.0 / size as f64
        } else {
            -1.0
        };
        emit_progress(&self.conn, self.id, percent, name);
    }

    fn item_done(&self, _name: &str) {}
}

/// 守护进程无交互界面:告警转为输出行;确认类回调一律拒绝
struct TxnUi {
    id: u32,
    conn: zbus::Connection,
}

impl Ui for TxnUi {
    fn alert(&self, kind: AlertKind, header: &str, body: Option<&str>) -> Result<(), GdebiError> {
        let kind = match kind {
            AlertKind::Error => "error",
            AlertKind::Warning => "warning",
            AlertKind::Info => "info",
        };
        emit(
            &self.conn,
            "OutputLine",
            &(
                self.id,
                match body {
                    Some(b) => format!("{kind}: {header}: {b}"),
                    None => format!("{kind}: {header}"),
                },
            ),
        );
        Ok(())
    }

    fn confirm(&self, _header: &str, _body: Option<&str>) -> Result<bool, GdebiError> {
        Ok(false)
    }
}
