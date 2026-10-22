//! GTK4 前端(gtk4 为硬依赖)。
//! 布局对齐旧版 gdebi-gtk 的 data/gdebi.ui(window_main / dialog_deb_install):
//! 菜单栏(File/Help)+ 左侧信息表(Package/Status)+
//! 右侧动作按钮列 + Notebook(Description/Details)+ 底部缓存进度条与状态栏 +
//! 安装/移除进度对话框(Terminal expander + autoclose)。
//! 文案沿用旧版原文并经 gettext 国际化(域 gdebi,复用旧版 po,见 core::i18n)。
//! 安装/移除在后台线程执行,事件经 async-channel 回主线程呈现
//! (gtk 对象不跨线程,ThreadUi 只负责把事件投入 channel)。
//!
//! 未还原部分(不留占位):button_download(远程 deb 下载)。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;

use clap::{CommandFactory, FromArgMatches, Parser};
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{
    gio, glib, Application, ApplicationWindow, ButtonsType, FileChooserNative, MessageDialog,
    MessageType, ResponseType,
};

use gdebi_core::depcheck::{Changes, DepChecker, Verdict};
use gdebi_core::flow::{self, PackageInfo};
use gdebi_core::i18n;
use gdebi_core::i18n::{gettext, ngettext, tf};
use gdebi_core::install;
use gdebi_core::ui::{AlertKind, InstallProgress, Ui};
use gdebi_core::{DebFile, GdebiError};

#[derive(Parser)]
#[command(name = "gdebi-rs-gtk", version)]
pub struct Args {
    /// Set an APT configuration option
    #[arg(short = 'o', long = "option")]
    pub apt_opts: Vec<String>,

    /// 非 root 时经 pkexec 提权重启后自动执行动作(旧版 gksu 重启契约)
    #[arg(short = 'n', long = "non-interactive", hide = true)]
    pub non_interactive: bool,

    /// 与 --non-interactive 同用:执行移除而非安装(旧版 --remove)
    #[arg(long = "remove", hide = true)]
    pub remove: bool,

    /// 要打开的 .deb 文件
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
}

/// 构建带翻译文案的命令。derive 属性内的字符串不被 xgettext 提取,
/// about/help 统一在普通调用点经 gettext 注入(调用点即提取点);
/// 字段 doc 注释仅供开发者,用户可见 help 全部由此覆盖
fn build_command() -> clap::Command {
    Args::command()
        .about(gettext(
            "simple tool to view and install deb files - GTK frontend",
        ))
        .mut_arg("apt_opts", |a| {
            a.help(gettext("Set an APT configuration option"))
        })
        .mut_arg("file", |a| a.help(gettext("Debian package to open")))
}

// ---------------------------------------------------------------------------
// Ui trait 对话框实现(主线程)
// ---------------------------------------------------------------------------

/// Ui 回调 → GTK 模态对话框
struct GtkUi {
    parent: gtk::Window,
}

impl Ui for GtkUi {
    fn alert(&self, kind: AlertKind, header: &str, body: Option<&str>) -> Result<(), GdebiError> {
        show_alert(&self.parent, kind, header, body);
        Ok(())
    }

    /// 模态同步语义:嵌套迭代主循环直至用户响应
    fn confirm(&self, header: &str, body: Option<&str>) -> Result<bool, GdebiError> {
        let (tx, rx) = std_mpsc::channel::<bool>();
        let dlg = MessageDialog::builder()
            .transient_for(&self.parent)
            .modal(true)
            .message_type(MessageType::Question)
            .buttons(ButtonsType::OkCancel)
            .text(header)
            .build();
        if let Some(b) = body {
            dlg.set_secondary_text(Some(b));
        }
        dlg.connect_response(move |d, r| {
            let _ = tx.send(r == ResponseType::Ok);
            d.destroy();
        });
        dlg.present();
        loop {
            glib::MainContext::default().iteration(true);
            if let Ok(v) = rx.try_recv() {
                return Ok(v);
            }
        }
    }
}

fn show_alert<W: IsA<gtk::Window>>(parent: &W, kind: AlertKind, header: &str, body: Option<&str>) {
    let ty = match kind {
        AlertKind::Error => MessageType::Error,
        AlertKind::Warning => MessageType::Warning,
        AlertKind::Info => MessageType::Info,
    };
    let text = match body {
        Some(b) => format!("{header}\n\n{b}"),
        None => header.to_string(),
    };
    let dlg = MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .message_type(ty)
        .buttons(ButtonsType::Close)
        .text(text)
        .build();
    dlg.connect_response(|d, _| d.destroy());
    dlg.present();
}

// ---------------------------------------------------------------------------
// 后台动作线程 → 主线程事件流
// ---------------------------------------------------------------------------

enum ActionEvent {
    Alert(AlertKind, String, Option<String>),
    OutputLine(String),
    Finished(Result<i32, String>),
}

/// 线程侧 Ui/InstallProgress:仅投递事件(confirm 在线程内不可用)
struct ThreadUi {
    tx: async_channel::Sender<ActionEvent>,
}

impl Ui for ThreadUi {
    fn alert(&self, kind: AlertKind, header: &str, body: Option<&str>) -> Result<(), GdebiError> {
        // 线程上下文:send() 返回 future,必须 send_blocking 才真正入队
        let _ = self
            .tx
            .send_blocking(ActionEvent::Alert(
                kind,
                header.to_string(),
                body.map(String::from),
            ));
        Ok(())
    }

    fn confirm(&self, _header: &str, _body: Option<&str>) -> Result<bool, GdebiError> {
        Ok(false) // 动作流程不调用 confirm;trait 契约占位
    }
}

impl InstallProgress for ThreadUi {
    fn output_line(&self, line: &str) {
        let _ = self
            .tx
            .send_blocking(ActionEvent::OutputLine(line.to_string()));
    }
}

// ---------------------------------------------------------------------------
// 主窗口(布局对齐 data/gdebi.ui 的 window_main)
// ---------------------------------------------------------------------------

/// window_main 的控件集合(Rc 共享给菜单动作)
struct MainWindow {
    window: gtk::ApplicationWindow,
    label_name: gtk::Label,
    label_status: gtk::Label,
    button_details: gtk::Button,
    button_install: gtk::Button,
    button_remove: gtk::Button,
    textview_description: gtk::TextView,
    details_version: gtk::Label,
    details_maintainer: gtk::Label,
    details_priority: gtk::Label,
    details_section: gtk::Label,
    details_size: gtk::Label,
    tree_files: gtk::TreeView,
    file_content: gtk::TextView,
    lintian_output: gtk::TextView,
    statusbar: gtk::Statusbar,
    status_ctx: u32,
    apt_opts: Vec<String>,
    non_interactive: bool,
    current_file: Rc<RefCell<Option<PathBuf>>>,
    changes: Rc<RefCell<Option<Changes>>>,
}

pub fn run() {
    // 首次取串(含 clap about)前完成 locale 与消息目录绑定
    i18n::init();
    let cmd = build_command();
    let matches = cmd.get_matches();
    let args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let app = Application::builder()
        .application_id("org.debian.gdebi-rs-gtk")
        .build();
    let action = args.into_action();
    app.connect_activate(move |app| activate(app, action.clone()));
    // 参数已由 clap 解析,不交给 GApplication(位置参数会触发 GFile open 路径)
    app.run_with_args(&["gdebi-rs-gtk"]);
}

/// 启动动作上下文:文件 + 提权重启模式(旧版 gksu 重启后的 --non-interactive)
#[derive(Debug, Clone)]
struct LaunchAction {
    file: Option<PathBuf>,
    apt_opts: Vec<String>,
    non_interactive: bool,
    remove: bool,
}

impl Args {
    fn into_action(self) -> LaunchAction {
        LaunchAction {
            file: self.file,
            apt_opts: self.apt_opts,
            non_interactive: self.non_interactive,
            remove: self.remove,
        }
    }
}

fn activate(app: &Application, action: LaunchAction) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title(gettext("Package Installer").as_str())
        .default_width(550)
        .default_height(400)
        // 旧版 gdebi.ui 顶部有 menubar1;GTK4 默认不渲染 app menubar
        .show_menubar(true)
        .build();

    let gui = build_window(&window, action.apt_opts, action.non_interactive);
    setup_actions(app, &gui);

    if let Some(f) = action.file {
        load_deb(&gui, &f);
        // 提权重启后(root):加载完成即执行动作,不再重复确认
        if action.non_interactive {
            if action.remove {
                on_remove_clicked(&gui);
            } else {
                on_install_clicked(&gui);
            }
        }
    }
    window.present();
}

fn build_window(
    window: &gtk::ApplicationWindow,
    apt_opts: Vec<String>,
    non_interactive: bool,
) -> Rc<MainWindow> {
    // 左侧信息表(旧版 table1):Package / Status
    let label_name = gtk::Label::default();
    label_name.set_markup("<b><big>   </big></b>");
    label_name.set_halign(gtk::Align::Start);
    label_name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let label_status = gtk::Label::default();
    label_status.set_halign(gtk::Align::Start);
    label_status.set_valign(gtk::Align::Start);
    label_status.set_wrap(true);
    label_status.set_use_markup(true);
    let button_details = gtk::Button::with_mnemonic(&gettext("_Details"));
    // 旧版 __init__ 初始隐藏,加载 deb 后按 verdict 再决定显隐
    button_details.hide();

    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .vexpand(true)
        .valign(gtk::Align::Start)
        .build();
    let labels = [(0, gettext("Package:")), (1, gettext("Status:"))];
    for (row, text) in labels {
        let l = gtk::Label::new(Some(text.as_str()));
        l.set_halign(gtk::Align::Start);
        l.set_valign(gtk::Align::Start);
        grid.attach(&l, 0, row, 1, 1);
    }
    grid.attach(&label_name, 1, 0, 1, 1);
    // Status 行:label_status + button_details 同一行(旧版 hbox2)
    let status_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    status_row.append(&label_status);
    status_row.append(&button_details);
    grid.attach(&status_row, 1, 1, 1, 1);

    // 右侧动作按钮列(旧版 vbox5):Install / Remove
    // (button_download 待远程 deb 下载接入)
    let button_install = gtk::Button::with_mnemonic(&gettext("_Install Package"));
    let button_remove = gtk::Button::with_mnemonic(&gettext("_Remove Package"));
    for b in [&button_install, &button_remove] {
        b.set_hexpand(true);
        b.set_sensitive(false);
    }
    button_remove.set_visible(false);
    let btn_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .valign(gtk::Align::Start)
        .build();
    btn_col.append(&button_install);
    btn_col.append(&button_remove);

    let hbox_main = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .vexpand(true)
        .build();
    hbox_main.append(&grid);
    hbox_main.append(&btn_col);

    // Notebook(旧版 notebook_details):Description 全文 + Details 五项
    let textview_description = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::Word)
        .build();
    let tab_desc = gtk::Label::new(Some(gettext("Description").as_str()));
    let details_version = gtk::Label::default();
    let details_maintainer = gtk::Label::default();
    let details_priority = gtk::Label::default();
    let details_section = gtk::Label::default();
    let details_size = gtk::Label::default();
    let details_grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .margin_top(12)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
        .valign(gtk::Align::Start)
        .build();
    let details = [
        (0, gettext("Version:"), &details_version),
        (1, gettext("Maintainer:"), &details_maintainer),
        (2, gettext("Priority:"), &details_priority),
        (3, gettext("Section:"), &details_section),
        (4, gettext("Size:"), &details_size),
    ];
    for (row, text, val) in details {
        let l = gtk::Label::new(Some(text.as_str()));
        l.set_markup(&format!("<b>{text}</b>"));
        l.set_halign(gtk::Align::Start);
        details_grid.attach(&l, 0, row, 1, 1);
        val.set_halign(gtk::Align::Start);
        val.set_selectable(true);
        details_grid.attach(val, 1, row, 1, 1);
    }
    let tab_details = gtk::Label::new(Some(gettext("Details").as_str()));
    let notebook = gtk::Notebook::new();
    notebook.set_vexpand(true);
    notebook.append_page(&scroll(&textview_description), Some(&tab_desc));
    notebook.append_page(&details_grid, Some(&tab_details));

    // Included files(旧版 tab3/hpaned2):左文件树,右内容文本(等宽)
    let tree_files = gtk::TreeView::new();
    tree_files.set_headers_visible(false);
    let cell = gtk::CellRendererText::new();
    let col = gtk::TreeViewColumn::new();
    col.pack_start(&cell, true);
    col.add_attribute(&cell, "text", 0);
    tree_files.append_column(&col);
    let file_content = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .build();
    let files_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .position(220)
        .start_child(&scroll(&tree_files))
        .end_child(&scroll(&file_content))
        .hexpand(true)
        .vexpand(true)
        .build();
    let tab_files = gtk::Label::new(Some(gettext("Included files").as_str()));
    notebook.append_page(&files_paned, Some(&tab_files));

    // Lintian output(旧版 tab4):等宽文本,后台线程填充
    let lintian_output = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .build();
    let tab_lintian = gtk::Label::new(Some(gettext("Lintian output").as_str()));
    notebook.append_page(&scroll(&lintian_output), Some(&tab_lintian));

    // 底部(旧版 hbox3):progressbar_cache + statusbar_main
    let progressbar_cache = gtk::ProgressBar::new();
    progressbar_cache.set_hexpand(true);
    progressbar_cache.set_pulse_step(0.05);
    let statusbar = gtk::Statusbar::new();
    let status_ctx = statusbar.context_id("context_main_window");
    let hbox3 = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    hbox3.append(&progressbar_cache);
    hbox3.append(&statusbar);

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    vbox.append(&hbox_main);
    vbox.append(&notebook);
    vbox.append(&hbox3);
    window.set_child(Some(&vbox));

    let gui = Rc::new(MainWindow {
        window: window.clone(),
        label_name,
        label_status,
        button_details,
        button_install,
        button_remove,
        textview_description,
        details_version,
        details_maintainer,
        details_priority,
        details_section,
        details_size,
        tree_files,
        file_content,
        lintian_output,
        statusbar,
        status_ctx,
        apt_opts,
        non_interactive,
        current_file: Rc::new(RefCell::new(None)),
        changes: Rc::new(RefCell::new(None)),
    });

    // Install / Remove / Details 接线
    let gui_btn = gui.clone();
    gui.button_install.connect_clicked(move |_| {
        on_install_clicked(&gui_btn);
    });
    let gui_rm = gui.clone();
    gui.button_remove.connect_clicked(move |_| {
        on_remove_clicked(&gui_rm);
    });
    let gui_det = gui.clone();
    gui.button_details.connect_clicked(move |_| {
        if let Some(changes) = gui_det.changes.borrow().as_ref() {
            show_details(&gui_det.window, &changes.install, &changes.remove);
        }
    });
    // Included files:选中成员 → 右侧内容视图(旧版 on_treeview_files_cursor_changed)
    let gui_sel = gui.clone();
    gui.tree_files
        .selection()
        .connect_changed(move |sel| on_file_selected(&gui_sel, sel));

    gui
}

/// 文件树选中回调:组(0=control,1=data)与成员名决定内容来源
fn on_file_selected(gui: &MainWindow, sel: &gtk::TreeSelection) {
    let Some((model, iter)) = sel.selected() else {
        return;
    };
    let store = model
        .downcast::<gtk::TreeStore>()
        .expect("files tree store");
    let path = store.path(&iter);
    // 顶层为组标题(旧版:depth < 2 不处理内容)
    if path.depth() < 2 {
        return;
    }
    // 组号取路径首段(gtk4 的 TreePath 无 indices 访问器,经字符串解析)
    let group: u32 = path
        .to_str()
        .unwrap_or_default()
        .split(':')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(u32::MAX);
    let name: String = store.get_value(&iter, 0).get().unwrap_or_default();
    if name.ends_with('/') {
        gui.file_content
            .buffer()
            .set_text(&gettext("Selection is a directory"));
        return;
    }
    let content = match group {
        0 => DebFile::open(
            gui.current_file.borrow().as_deref().unwrap_or(Path::new("")),
        )
        .and_then(|deb| deb.control_content(&name)),
        1 => DebFile::open(
            gui.current_file.borrow().as_deref().unwrap_or(Path::new("")),
        )
        .and_then(|deb| deb.data_content(&name)),
        _ => return,
    };
    // 文本原样显示;二进制显示文件类型(core 统一判定,规避内嵌 NUL)
    let text = match content {
        Ok(bytes) if !bytes.is_empty() => flow::content_preview(&bytes),
        Ok(_) => gettext("File content can not be extracted"),
        Err(e) => tf("Error reading file content '{0}'", &[&e.to_string()]),
    };
    gui.file_content.buffer().set_text(&text);
}

/// Included files 页签填充(旧版 set_deb 的文件树部分)
fn fill_files(gui: &MainWindow, deb: &DebFile) {
    let store = gtk::TreeStore::new(&[glib::Type::STRING]);
    let res = (|| -> Result<(), GdebiError> {
        // 组 0:Package control data;组 1:Upstream data(旧版原文)
        let control_header = store.append(None);
        store.set(&control_header, &[(0, &gettext("Package control data"))]);
        for n in deb.control_filelist()? {
            let child = store.append(Some(&control_header));
            store.set(&child, &[(0, &n)]);
        }
        let data_header = store.append(None);
        store.set(&data_header, &[(0, &gettext("Upstream data"))]);
        for n in deb.data_filelist()? {
            let child = store.append(Some(&data_header));
            store.set(&child, &[(0, &n)]);
        }
        Ok(())
    })();
    if let Err(e) = res {
        // 旧版:异常详情进日志,树内仅显示固定提示
        eprintln!("Exception while reading the filelist: '{e}'");
        store.clear();
        let iter = store.append(None);
        store.set(&iter, &[(0, &gettext("Error reading filelist"))]);
    }
    gui.tree_files.set_model(Some(&store));
    gui.tree_files.expand_all();
}

/// 后台运行 lintian 并流式回填(旧版 _run_lintian;非交互模式跳过)
fn run_lintian(gui: &MainWindow, file: &Path) {
    let buf = gui.lintian_output.buffer();
    if !Path::new("/usr/bin/lintian").exists() {
        buf.set_text(&gettext(
            "No lintian available.\nPlease install using sudo apt-get install lintian",
        ));
        return;
    }
    buf.set_text(&gettext("Running lintian..."));

    enum LintianEvent {
        Line(String),
        Finished(i32),
    }
    let (tx, rx) = async_channel::unbounded::<LintianEvent>();
    let path = file.to_path_buf();
    std::thread::spawn(move || {
        let child = Command::new("/usr/bin/lintian")
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = tx
                    .send_blocking(LintianEvent::Line(tf(
                        "Failed to run lintian: {0}",
                        &[&e.to_string()],
                    )));
                let _ = tx.send_blocking(LintianEvent::Finished(-1));
                return;
            }
        };
        let out = child.stdout.take().expect("piped stdout");
        let err = child.stderr.take().expect("piped stderr");
        let tx2 = tx.clone();
        let t = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx2.send_blocking(LintianEvent::Line(line));
            }
        });
        use std::io::BufRead;
        for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
            let _ = tx.send_blocking(LintianEvent::Line(line));
        }
        let _ = t.join();
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = tx.send_blocking(LintianEvent::Finished(code));
    });

    // 回填:文件已切换则丢弃过期输出(旧版无此防护,但多次 Open 会串页)
    let watch = gui.current_file.clone();
    let watched = file.to_path_buf();
    let view = gui.lintian_output.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            if watch.borrow().as_deref() != Some(watched.as_path()) {
                break;
            }
            match ev {
                LintianEvent::Line(line) => {
                    let buf = view.buffer();
                    let mut end = buf.end_iter();
                    buf.insert(&mut end, &format!("{line}\n"));
                }
                LintianEvent::Finished(code) => {
                    let buf = view.buffer();
                    let mut end = buf.end_iter();
                    buf.insert(
                        &mut end,
                        &format!("\n{}", tf("Lintian finished with exit status {0}", &[&code.to_string()])),
                    );
                    break;
                }
            }
        }
    });
}

fn setup_actions(app: &Application, gui: &Rc<MainWindow>) {
    // 菜单栏(旧版 menubar1:File: Open…/Refresh/Quit;Help: About)
    let file_menu = gio::Menu::new();
    file_menu.append(Some(gettext("_Open…").as_str()), Some("app.open"));
    file_menu.append(Some(gettext("_Refresh").as_str()), Some("app.refresh"));
    file_menu.append(Some(gettext("_Quit").as_str()), Some("app.quit"));
    let help_menu = gio::Menu::new();
    help_menu.append(Some(gettext("_About").as_str()), Some("app.about"));
    let menubar = gio::Menu::new();
    menubar.append_submenu(Some(gettext("_File").as_str()), &file_menu);
    menubar.append_submenu(Some(gettext("_Help").as_str()), &help_menu);
    app.set_menubar(Some(&menubar));

    let open = gio::SimpleAction::new("open", None);
    let gui_open = gui.clone();
    open.connect_activate(move |_, _| {
        let chooser = FileChooserNative::builder()
            .action(gtk::FileChooserAction::Open)
            .transient_for(&gui_open.window)
            .title(gettext("Open Debian package").as_str())
            .build();
        let gui_fc = gui_open.clone();
        chooser.connect_response(move |c, resp| {
            if resp == ResponseType::Accept
                && let Some(f) = c.file().and_then(|f| f.path())
            {
                load_deb(&gui_fc, &f);
            }
        });
        chooser.show();
    });
    app.add_action(&open);

    let refresh = gio::SimpleAction::new("refresh", None);
    let gui_ref = gui.clone();
    refresh.connect_activate(move |_, _| {
        let f = gui_ref.current_file.borrow().clone();
        if let Some(f) = f {
            load_deb(&gui_ref, &f);
        }
    });
    app.add_action(&refresh);

    let quit = gio::SimpleAction::new("quit", None);
    let win = gui.window.clone();
    quit.connect_activate(move |_, _| win.close());
    app.add_action(&quit);

    let about = gio::SimpleAction::new("about", None);
    let win_about = gui.window.clone();
    about.connect_activate(move |_, _| {
        // GTK4 的 AboutDialog 直接继承 Window(非 Dialog),自带关闭按钮
        let dlg = gtk::AboutDialog::builder()
            .transient_for(&win_about)
            .modal(true)
            .program_name("gdebi-rs")
            .version(env!("CARGO_PKG_VERSION"))
            .comments(gettext("simple tool to view and install deb files").as_str())
            .build();
        dlg.present();
    });
    app.add_action(&about);

    // 快捷键(Ctrl+O / Ctrl+Q),对齐旧版 accelgroup
    app.set_accels_for_action("app.open", &["<Control>O"]);
    app.set_accels_for_action("app.quit", &["<Control>Q"]);
}

fn scroll(w: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(w)
        .hexpand(true)
        .vexpand(true)
        .build()
}

/// 打开并填充窗口(旧版 set_deb;同步执行,与旧版相同的短暂阻塞)
fn load_deb(gui: &Rc<MainWindow>, file: &Path) {
    gui.statusbar.push(gui.status_ctx, gettext("Loading...").as_str());
    let ui = GtkUi {
        parent: gui.window.clone().upcast(),
    };
    *gui.changes.borrow_mut() = None;
    *gui.current_file.borrow_mut() = Some(file.to_path_buf());

    // 旧版入口:文件不存在直接提示
    if !file.exists() {
        show_alert(
            &gui.window,
            AlertKind::Error,
            &tf(
                "gdebi error, file not found: {0}",
                &[&file.display().to_string()],
            ),
            None,
        );
        gui.statusbar.pop(gui.status_ctx);
        return;
    }

    let deb = match DebFile::open(file) {
        Ok(d) => d,
        Err(e) => {
            let _ = ui.alert(
                AlertKind::Error,
                &gettext("Could not open the software package"),
                Some(&e.to_string()),
            );
            gui.statusbar.pop(gui.status_ctx);
            return;
        }
    };
    let info = flow::describe_deb(&deb);
    fill_static(gui, &info, &deb);

    // 文件树与 lintian(旧版 set_deb 顺序:文件树 → lintian → 依赖检查;
    // lintian 仅非交互模式运行,旧版 L325)
    fill_files(gui, &deb);
    if !gui.non_interactive {
        run_lintian(gui, file);
    }

    // 依赖检查;错误经 ui.alert 呈现(顺序与 CLI open_and_check 一致)
    let checker = match DepChecker::new(&gui.apt_opts) {
        Ok(c) => c,
        Err(e) => {
            let _ = ui.alert(
                AlertKind::Error,
                &gettext("Error opening the cache"),
                Some(&e.to_string()),
            );
            gui.statusbar.pop(gui.status_ctx);
            return;
        }
    };
    let verdict = match checker.check(&deb, file) {
        Ok(v) => v,
        Err(e) => {
            let _ = ui.alert(
                AlertKind::Error,
                &gettext("Dependency check failed"),
                Some(&e.to_string()),
            );
            gui.statusbar.pop(gui.status_ctx);
            return;
        }
    };

    match verdict {
        Verdict::Uninstallable(failure) => {
            // 旧版 check 失败分支:红色错误 + install 禁用 + details 隐藏
            gui.label_status.set_markup(&tf(
                "<span foreground=\"red\" weight=\"bold\">Error: {0}</span>",
                &[&glib::markup_escape_text(&failure)],
            ));
            gui.button_install.set_sensitive(false);
            gui.button_details.hide();
            gui.button_remove.hide();
        }
        Verdict::Installable(changes) => {
            let (inst, rm) = (&changes.install, &changes.remove);
            // 旧版 get_changes 的 deps markup(ngettext 处理复数)
            let mut deps = String::new();
            if !rm.is_empty() {
                deps.push_str(&tf(
                    &ngettext(
                        "Requires the <b>removal</b> of {0} package",
                        "Requires the <b>removal</b> of {0} packages",
                        rm.len() as u32,
                    ),
                    &[&rm.len().to_string()],
                ));
                deps.push('\n');
            }
            if !inst.is_empty() {
                deps.push_str(&tf(
                    &ngettext(
                        "Requires the installation of {0} package",
                        "Requires the installation of {0} packages",
                        inst.len() as u32,
                    ),
                    &[&inst.len().to_string()],
                ));
                deps.push('\n');
            }

            let same_installed =
                checker.installed_version(&info.package) == Some(info.version.as_str());

            if same_installed {
                // 旧版同版本分支:状态文案 + Reinstall + Remove 显示
                gui.label_status
                    .set_text(&gettext("Same version is already installed"));
                gui.button_install
                    .set_label(&gettext("_Reinstall Package"));
                gui.button_remove.set_visible(true);
                gui.button_remove.set_sensitive(true);
                gui.button_details.hide();
            } else {
                gui.button_install.set_label(&gettext("_Install Package"));
                gui.button_remove.hide();
                if !rm.is_empty() || !inst.is_empty() {
                    gui.label_status.set_markup(&deps);
                    gui.button_details.show();
                } else {
                    gui.label_status
                        .set_text(&gettext("All dependencies are satisfied"));
                    gui.button_details.hide();
                }
            }
            gui.button_install.set_sensitive(true);
            *gui.changes.borrow_mut() = Some(changes);
        }
    }
    gui.statusbar.pop(gui.status_ctx);
}

/// Details 页签与描述的静态填充(旧版 label_name/textview_description)
fn fill_static(gui: &MainWindow, info: &PackageInfo, deb: &DebFile) {
    gui.label_name
        .set_markup(&format!("<b><big>{}</big></b>", info.package));
    gui.textview_description
        .buffer()
        .set_text(&info.description);

    let set = |l: &gtk::Label, v: Option<String>| l.set_text(&v.unwrap_or_default());
    set(&gui.details_version, Some(info.version.clone()));
    set(&gui.details_maintainer, deb.field_trimmed("Maintainer").map(String::from));
    set(&gui.details_priority, deb.field_trimmed("Priority").map(String::from));
    set(&gui.details_section, deb.field_trimmed("Section").map(String::from));
    set(
        &gui.details_size,
        deb.field_trimmed("Installed-Size").map(|s| format!("{s} KiB")),
    );
}

// ---------------------------------------------------------------------------
// 安装 / 移除动作(对话框对齐 dialog_deb_install)
// ---------------------------------------------------------------------------

type ActionWork =
    Box<dyn FnOnce(&ThreadUi, &Changes, &Path, &[String]) -> Result<i32, GdebiError> + Send>;

/// 动作执行对话框:label_action + progressbar(pulse)+ label_install_status +
/// Terminal expander + autoclose 勾选;Close 按钮运行中禁用;
/// 期间主窗口禁用(旧版 set_sensitive),完成后恢复
fn run_action_dialog(
    gui: &MainWindow,
    action: &str,
    complete: String,
    failed: String,
    work: ActionWork,
) {
    let dialog = gtk::Dialog::builder()
        .transient_for(&gui.window)
        .modal(true)
        .title(gettext("Applying changes").as_str())
        .default_width(480)
        .build();
    dialog.add_button(&gettext("Close"), ResponseType::Close);
    dialog.set_default_response(ResponseType::Close);

    let label_action = gtk::Label::new(Some(action));
    label_action.set_halign(gtk::Align::Start);
    let bar = gtk::ProgressBar::new();
    let label_status = gtk::Label::new(None);
    label_status.set_halign(gtk::Align::Start);
    label_status.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let textview = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .build();
    let exp = gtk::Expander::new(Some(gettext("Terminal").as_str()));
    exp.set_child(Some(&scroll(&textview)));
    let autoclose = gtk::CheckButton::with_label(&gettext(
        "Automatically close after the changes have been successfully applied",
    ));
    autoclose.set_active(true);
    autoclose.set_halign(gtk::Align::Start);

    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    box_.append(&label_action);
    box_.append(&bar);
    box_.append(&label_status);
    box_.append(&exp);
    box_.append(&autoclose);
    dialog.content_area().append(&box_);

    let close_btn = dialog
        .widget_for_response(ResponseType::Close)
        .and_downcast::<gtk::Button>();
    if let Some(b) = &close_btn {
        b.set_sensitive(false);
    }
    gui.window.set_sensitive(false);

    // pulse 定时器(子进程行输出无精确百分比;完成后置 1.0)
    let bar_pulse = bar.clone();
    let pulse_id = glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        bar_pulse.pulse();
        glib::ControlFlow::Continue
    });

    dialog.connect_response(|d, r| {
        if r == ResponseType::Close {
            d.destroy();
        }
    });
    dialog.present();

    let (tx, rx) = async_channel::unbounded::<ActionEvent>();
    let changes = gui.changes.borrow().clone().unwrap_or_default();
    let file = gui.current_file.borrow().clone().unwrap_or_default();
    let apt_opts = gui.apt_opts.clone();
    std::thread::spawn(move || {
        let th_ui = ThreadUi { tx: tx.clone() };
        let res = work(&th_ui, &changes, &file, &apt_opts);
        let _ = tx.send_blocking(ActionEvent::Finished(res.map_err(|e| e.to_string())));
    });

    let statusbar = gui.statusbar.clone();
    let ctx = gui.status_ctx;
    let window = gui.window.clone();
    let bar_done = bar.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            match ev {
                ActionEvent::Alert(kind, header, body) => {
                    show_alert(&window, kind, &header, body.as_deref())
                }
                ActionEvent::OutputLine(line) => {
                    label_status.set_text(&line);
                    let buf = textview.buffer();
                    let mut end = buf.end_iter();
                    buf.insert(&mut end, &format!("{line}\n"));
                    textview.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
                }
                ActionEvent::Finished(res) => {
                    pulse_id.remove();
                    bar_done.set_fraction(1.0);
                    if let Some(b) = &close_btn {
                        b.set_sensitive(true);
                    }
                    match res {
                        Ok(0) => {
                            statusbar.push(ctx, &complete);
                            if autoclose.is_active() {
                                dialog.destroy();
                            }
                        }
                        Ok(_) => {
                            statusbar.push(ctx, &failed);
                        }
                        Err(e) => {
                            statusbar.push(ctx, &failed);
                            show_alert(
                                &window,
                                AlertKind::Error,
                                &gettext("gdebi error"),
                                Some(&e),
                            );
                        }
                    }
                    window.set_sensitive(true);
                    break;
                }
            }
        }
    });
}

fn on_install_clicked(gui: &Rc<MainWindow>) {
    if !is_root()
        // 旧版:提权提示对话框(execv gksu --message 的现代等价)
        && !request_privileges(gui, false)
    {
        return;
    }
    if let Some(c) = gui.changes.borrow().as_ref()
        && !c.unauthenticated.is_empty()
    {
        // 旧版 dpkg_action 的 unauthenticated 确认
        let ui = GtkUi {
            parent: gui.window.clone().upcast(),
        };
        let ok = ui.confirm(
            &gettext("Install unauthenticated software?"),
            Some(&tf(
                "The following packages are UNAUTHENTICATED: {0}",
                &[&c.unauthenticated.join(" ")],
            )),
        );
        match ok {
            Ok(true) => {}
            _ => return,
        }
    }
    gui.statusbar
        .push(gui.status_ctx, gettext("Installing package file...").as_str());
    run_action_dialog(
        gui,
        &gettext("Installing package file..."),
        gettext("Installation complete").to_string(),
        gettext("Failed to install package file").to_string(),
        Box::new(move |ui, changes, file, opts| {
            install::install(ui, ui, changes, file, opts)
        }),
    );
}

fn on_remove_clicked(gui: &Rc<MainWindow>) {
    if !is_root() && !request_privileges(gui, true) {
        return;
    }
    let Some(pkgname) = gui
        .current_file
        .borrow()
        .clone()
        .and_then(|f| DebFile::open(&f).ok())
        .and_then(|d| d.field_trimmed("Package").map(String::from))
        .filter(|n| !n.is_empty())
    else {
        return;
    };
    gui.statusbar
        .push(gui.status_ctx, gettext("Removing package...").as_str());
    // 旧版 dpkg_action(install=False) 经 apt 提交(级联处理依赖);
    // 骨架期直接 dpkg -r(不级联,最小影响)
    run_action_dialog(
        gui,
        &gettext("Removing package..."),
        gettext("Removal complete").to_string(),
        gettext("Failed to remove package").to_string(),
        Box::new(move |ui, _changes, _file, _opts| {
            use std::io::BufRead;
            use std::process::{Command, Stdio};
            let mut child = Command::new("dpkg")
                .arg("-r")
                .arg(&pkgname)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let out = child.stdout.take().expect("piped stdout");
            let err = child.stderr.take().expect("piped stderr");
            let ui_out = ThreadUi {
                tx: ui.tx.clone(),
            };
            let t = std::thread::spawn(move || {
                for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                    ui_out.output_line(&line);
                }
            });
            for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                ui.output_line(&line);
            }
            let _ = t.join();
            Ok(child.wait()?.code().unwrap_or(-1))
        }),
    );
}

/// 非 root 判定
fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// 非 root 时请求提权:提示对话框(旧版 gksu --message 的文案与职责)确认后
/// 经 pkexec 重启自身执行动作(polkit agent 负责认证)。
/// exec 成功则进程映像被替换不返回(窗口随进程消失,与旧版 execv 行为一致);
/// 返回 false 表示用户取消或启动失败,界面保持可用。
fn request_privileges(gui: &Rc<MainWindow>, remove: bool) -> bool {
    let (header, body) = if remove {
        (
            gettext("You need to grant administrative rights to remove software"),
            gettext("It is a possible risk to remove packages."),
        )
    } else {
        (
            gettext("You need to grant administrative rights to install software"),
            gettext(
                "It is a possible security risk to install packages files manually.\nInstall software from trustworthy software distributors only.",
            ),
        )
    };
    let ui = GtkUi {
        parent: gui.window.clone().upcast(),
    };
    match ui.confirm(&header, Some(&body)) {
        Ok(true) => {}
        _ => return false,
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            show_alert(
                &gui.window,
                AlertKind::Error,
                &gettext("Failed to locate the program executable"),
                Some(&e.to_string()),
            );
            return false;
        }
    };
    let Some(deb) = gui.current_file.borrow().clone() else {
        return false;
    };
    // 重启后的 cwd 不保证继承,deb 必须绝对路径
    let deb = std::path::absolute(&deb).unwrap_or(deb);

    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("pkexec");
    cmd.arg(&exe).arg("--non-interactive");
    if remove {
        cmd.arg("--remove");
    }
    cmd.arg(&deb);
    let e = cmd.exec();
    // 仅 exec 失败时到达(pkexec 缺失、路径不可执行等)
    show_alert(
        &gui.window,
        AlertKind::Error,
        &gettext("Failed to start pkexec"),
        Some(&e.to_string()),
    );
    false
}

fn show_details<W: IsA<gtk::Window>>(
    parent: &W,
    install: &[String],
    remove: &[String],
) {
    let mut list = String::new();
    for r in remove {
        list.push_str(&tf("<b>To be removed: {0}</b>", &[r]));
        list.push('\n');
    }
    for i in install {
        list.push_str(&tf("To be installed: {0}", &[i]));
        list.push('\n');
    }
    let dlg = MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .message_type(MessageType::Info)
        .buttons(ButtonsType::Close)
        .text(gettext("Changes").as_str())
        .build();
    dlg.set_secondary_text(Some(&list));
    dlg.set_secondary_use_markup(true);
    dlg.connect_response(|d, _| d.destroy());
    dlg.present();
}
