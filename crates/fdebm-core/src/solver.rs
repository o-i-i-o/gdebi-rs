//! varisat(CDCL SAT)编码的依赖求解器 —— 替代原 apt-get -s 文本编排。
//!
//! 求解域:已安装库(可卸载)∪ 仓库候选 ∪ 本地 deb(强制)。
//! 编码:
//! - 变量 = 选择单元:已安装包"保持安装"或某候选版本"被选中"
//! - 同名二进制包(键)单元两两互斥(AMO)
//! - Depends/Pre-Depends:选中 → 每个依赖组至少一个满足者(直接命中或虚拟提供)
//! - Conflicts/Breaks:互斥子句;Replaces 覆盖时豁免(dpkg 语义)
//! - Essential 包强制保持安装(比旧版 gdebi 多出的保护)
//!
//! 求解策略:CDCL 全解 + 假设式迭代精化(最小化卸载、优先新版本)。
//!
//! 与旧版 gdebi 的行为差异(有意):失败原因不再逐条比对 python-apt 的
//! failure_string,统一为"求解失败"类消息;or 组解析交给 SAT 而非首分支回溯。

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use varisat::{ExtendFormula, Lit, Solver};

use crate::debfile::DebFile;
use crate::error::FdebmError;
use crate::i18n::{gettext, tf};
use crate::sysstate::{Dep, Pkg, SysState, base_name};
use crate::version::cmp_version;

/// 下载坐标(仓库候选被选中时生成;本地 deb 直接走 dpkg -i)
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub name: String,
    pub version: String,
    pub url: String,
    pub dest: PathBuf,
    pub sha256: String,
    pub size: u64,
    pub trusted: bool,
}

/// 求解结果中的变更集(名称列表,GUI 呈现用)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub install: Vec<String>,
    pub remove: Vec<String>,
    pub unauthenticated: Vec<String>,
}

/// 完整求解结果
#[derive(Debug, Clone)]
pub struct Resolution {
    pub changes: Changes,
    pub downloads: Vec<DownloadSpec>,
}

/// 依赖检查结论
#[derive(Debug, Clone)]
pub enum Verdict {
    Installable(Resolution),
    Uninstallable(String),
}

/// 选择单元;编号空间:Installed 0..n,随后 Candidate
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Unit {
    /// installed 下标:该包保持安装
    Installed(usize),
    /// all_candidates 下标:该候选被选中
    Candidate(usize),
}

/// SAT 编码器:单元编号 ↔ varisat 变量(var 编号 = 单元编号 + 1)
struct Encoding<'a> {
    sys: &'a SysState,
    /// 本地 deb(独立单元,排在候选之后;不落入 sys 的缓存对象)
    local: Option<crate::sysstate::Candidate>,
    n_installed: usize,
    n_cands: usize,
    providers: BTreeMap<String, Vec<Unit>>,
    by_name: BTreeMap<String, Vec<Unit>>,
    /// 键 → 单元(AMO 分组)
    groups: BTreeMap<String, Vec<Unit>>,
}

impl<'a> Encoding<'a> {
    fn new(sys: &'a SysState, local: Option<crate::sysstate::Candidate>) -> Self {
        let n_installed = sys.installed.len();
        let n_cands = sys.all_candidates.len();
        let mut providers: BTreeMap<String, Vec<Unit>> = BTreeMap::new();
        let mut by_name: BTreeMap<String, Vec<Unit>> = BTreeMap::new();
        let mut groups: BTreeMap<String, Vec<Unit>> = BTreeMap::new();
        let mut push = |u: Unit, pkg: &Pkg| {
            by_name.entry(pkg.name.clone()).or_default().push(u);
            for p in &pkg.provides {
                providers.entry(p.clone()).or_default().push(u);
            }
            groups.entry(pkg.key.clone()).or_default().push(u);
        };
        for (i, pkg) in sys.installed.iter().enumerate() {
            push(Unit::Installed(i), pkg);
        }
        for (c, cand) in sys.all_candidates.iter().enumerate() {
            push(Unit::Candidate(c), &cand.pkg);
        }
        if let Some(local) = &local {
            push(Unit::Candidate(n_cands), &local.pkg);
        }
        Self {
            sys,
            local,
            n_installed,
            n_cands,
            providers,
            by_name,
            groups,
        }
    }

    fn unit(&self, idx: usize) -> Unit {
        if idx < self.n_installed {
            Unit::Installed(idx)
        } else {
            Unit::Candidate(idx - self.n_installed)
        }
    }

    fn plit(&self, idx: usize) -> Lit {
        Lit::from_dimacs((idx + 1) as isize)
    }

    fn nlit(&self, idx: usize) -> Lit {
        Lit::from_dimacs(-((idx + 1) as isize))
    }

    fn meta(&self, u: Unit) -> &Pkg {
        match u {
            Unit::Installed(i) => &self.sys.installed[i],
            Unit::Candidate(c) => {
                if c >= self.n_cands {
                    &self.local.as_ref().expect("local unit present").pkg
                } else {
                    &self.sys.all_candidates[c].pkg
                }
            }
        }
    }

    /// 依赖满足者:直接命中优先,无直接命中才看虚拟提供(与 python-apt 一致)
    fn satisfiers(&self, d: &Dep) -> Vec<Unit> {
        let direct: Vec<Unit> = self
            .by_name
            .get(base_name(&d.name))
            .map(|units| {
                units
                    .iter()
                    .copied()
                    .filter(|&u| {
                        let pkg = self.meta(u);
                        dep_covers(&d.name, pkg) && ver_matches(&pkg.version, &d.oper, &d.ver)
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !direct.is_empty() {
            return direct;
        }
        self.providers
            .get(base_name(&d.name))
            .cloned()
            .unwrap_or_default()
    }

    /// U 的 Conflicts/Breaks 条目 c 是否命中单元 v
    fn hits(&self, c: &Dep, v: Unit) -> bool {
        let pkg = self.meta(v);
        let name_hit = dep_covers(&c.name, pkg)
            || self
                .providers
                .get(base_name(&c.name))
                .is_some_and(|ps| ps.contains(&v));
        name_hit && (c.ver.is_empty() || ver_matches(&pkg.version, &c.oper, &c.ver))
    }

    /// U 的 Replaces 是否覆盖 v(冲突豁免)
    fn replaces_covers(&self, u: Unit, v: Unit) -> bool {
        let vpkg = self.meta(v);
        self.meta(u).replaces.iter().any(|g| {
            g.iter().any(|r| {
                dep_covers(&r.name, vpkg)
                    && (r.ver.is_empty() || ver_matches(&vpkg.version, &r.oper, &r.ver))
            })
        })
    }
}

/// 添加子句(排序去重,空子句即 UNSAT 由求解器报告)
fn add_clause(solver: &mut Solver, lits: Vec<Lit>) {
    let mut lits = lits;
    lits.sort();
    lits.dedup();
    solver.add_clause(&lits);
}

/// 依赖名是否覆盖给定包名(考虑 :arch 限定;无限定时任意架构均匹配)
fn dep_covers(dep_name: &str, pkg: &Pkg) -> bool {
    match dep_name.split_once(':') {
        Some((base, arch)) => pkg.name == base && pkg.arch == arch,
        None => pkg.name == dep_name,
    }
}

/// dpkg 关系操作符版本匹配("<"/">" 为 "<<"(">>") 的旧写法)
fn ver_matches(v: &str, oper: &str, w: &str) -> bool {
    if oper.is_empty() || w.is_empty() {
        return true;
    }
    let c = cmp_version(v, w);
    match oper {
        "<=" => c != std::cmp::Ordering::Greater,
        ">=" => c != std::cmp::Ordering::Less,
        "<<" | "<" => c == std::cmp::Ordering::Less,
        ">>" | ">" => c == std::cmp::Ordering::Greater,
        "=" => c == std::cmp::Ordering::Equal,
        _ => true,
    }
}

/// 主入口:在给定系统状态上求解安装 deb(不修改 sys)。
pub fn resolve(sys: &SysState, deb: &DebFile) -> Result<Verdict, FdebmError> {
    let name = deb.field_trimmed("Package").unwrap_or("").to_string();
    let version = deb.field_trimmed("Version").unwrap_or("").to_string();
    let arch = deb
        .field_trimmed("Architecture")
        .unwrap_or("all")
        .to_string();

    // 架构检查(与旧版 gdebi 同语义)
    let mut archs = vec![sys.native_arch.clone(), "all".to_string()];
    archs.extend(sys.foreign_arches.iter().cloned());
    if !archs.contains(&arch) {
        return Ok(Verdict::Uninstallable(tf(
            "Wrong architecture '{0}'",
            &[&arch],
        )));
    }

    // 降级拒绝(与旧版 gdebi 同语义;重装同版本放行)
    let key = if arch == "all" || arch == sys.native_arch {
        name.clone()
    } else {
        format!("{}:{}", name, arch)
    };
    if let Some(installed) = sys.installed_version(&key)
        && cmp_version(installed, &version) == std::cmp::Ordering::Greater
    {
        return Ok(Verdict::Uninstallable(gettext(
            "A later version is already installed",
        )));
    }

    Ok(encode_and_solve(
        sys,
        local_candidate(deb, &sys.native_arch),
    ))
}

/// 本地 deb → 候选元数据(键派生与 SysState::load 同规则)
fn local_candidate(deb: &DebFile, native_arch: &str) -> crate::sysstate::Candidate {
    let pkg = crate::sysstate::pkg_from_fields(|n| deb.field_trimmed(n), native_arch)
        .expect("DebFile::open 已校验 Package 字段");
    crate::sysstate::Candidate {
        pkg,
        repo: None,
        repo_filename: String::new(),
        sha256: String::new(),
        size: 0,
        local_path: Some(deb.path().to_path_buf()),
    }
}

fn encode_and_solve(sys: &SysState, local: crate::sysstate::Candidate) -> Verdict {
    let enc = Encoding::new(sys, Some(local));
    let n_units = enc.n_installed + enc.n_cands + usize::from(enc.local.is_some());
    let local_idx = enc
        .local
        .as_ref()
        .map(|_| enc.n_installed + enc.n_cands)
        .expect("resolve 已注入本地候选");

    let mut solver = Solver::new();

    // 目标:本地 deb 强制选中
    add_clause(&mut solver, vec![enc.plit(local_idx)]);

    // Essential 包强制保持安装(旧版 gdebi 缺失的保护)
    for (i, pkg) in sys.installed.iter().enumerate() {
        if pkg.essential {
            add_clause(&mut solver, vec![enc.plit(i)]);
        }
    }

    // 同键互斥(AMO)
    for group in enc.groups.values() {
        for a in 0..group.len() {
            for b in a + 1..group.len() {
                add_clause(
                    &mut solver,
                    vec![enc.nlit_of(group[a]), enc.nlit_of(group[b])],
                );
            }
        }
    }

    // 依赖子句(含 Pre-Depends,配置顺序由 dpkg 保证)
    for idx in 0..n_units {
        let u = enc.unit(idx);
        let meta = enc.meta(u);
        for group in meta.depends.iter().chain(meta.pre_depends.iter()) {
            let mut lits = vec![enc.nlit(idx)];
            for d in group {
                lits.extend(enc.satisfiers(d).into_iter().map(|s| enc.plit_of(s)));
            }
            add_clause(&mut solver, lits);
        }
    }

    // Conflicts(全单元互斥)与 Breaks(仅已安装)
    for idx in 0..n_units {
        let u = enc.unit(idx);
        let meta = enc.meta(u);
        if meta.conflicts.is_empty() && meta.breaks.is_empty() {
            continue;
        }
        for (deps, only_installed) in meta
            .conflicts
            .iter()
            .map(|g| (g, false))
            .chain(meta.breaks.iter().map(|g| (g, true)))
        {
            for c in deps {
                for v_idx in 0..n_units {
                    let v = enc.unit(v_idx);
                    if v == u || (only_installed && !matches!(v, Unit::Installed(_))) {
                        continue;
                    }
                    if enc.hits(c, v) && !enc.replaces_covers(u, v) {
                        add_clause(&mut solver, vec![enc.nlit(idx), enc.nlit(v_idx)]);
                    }
                }
            }
        }
    }

    // 求解 + 假设式精化(assume 每次整体替换假设集)
    let mut forced: Vec<Lit> = Vec::new();
    solver.assume(&forced);
    if !solver.solve().unwrap_or(false) {
        return Verdict::Uninstallable(gettext(
            "Failed to satisfy all dependencies (conflicting requests or missing packages)",
        ));
    }

    let extract = |solver: &Solver| -> HashSet<usize> {
        solver
            .model()
            .unwrap_or_default()
            .iter()
            .filter(|l| l.is_positive())
            .map(|l| l.index())
            .collect()
    };
    let mut selected = extract(&solver);

    // 精化 1:最小化卸载 —— 对每个被卸载的包尝试强制保持
    for i in 0..enc.n_installed {
        if selected.contains(&i) {
            continue;
        }
        let mut assumps = forced.clone();
        assumps.push(enc.plit(i));
        solver.assume(&assumps);
        if solver.solve().unwrap_or(false) {
            forced = assumps;
            selected = extract(&solver);
        }
    }

    // 精化 2:优先最新版本 —— 对每个键尝试强制最新非本地候选
    for key in enc.groups.keys() {
        let Some(cands) = sys.candidates.get(key) else {
            continue;
        };
        // add_candidates 已按版本降序排列;手工构造状态时按列表顺序
        let Some(&newest) = cands.iter().find(|&&c| !sys.all_candidates[c].is_local()) else {
            continue;
        };
        let newest_idx = newest + enc.n_installed;
        if selected.contains(&newest_idx) {
            continue;
        }
        let mut assumps = forced.clone();
        assumps.push(enc.plit(newest_idx));
        solver.assume(&assumps);
        if solver.solve().unwrap_or(false) {
            forced = assumps;
            selected = extract(&solver);
        }
    }

    // 结果组装
    let mut changes = Changes::default();
    let mut downloads = Vec::new();
    let mut install_keys: HashSet<String> = Default::default();
    for &idx in &selected {
        if idx < enc.n_installed {
            continue;
        }
        let cand_ref: &crate::sysstate::Candidate = if idx - enc.n_installed >= enc.n_cands {
            enc.local.as_ref().expect("local unit present")
        } else {
            &sys.all_candidates[idx - enc.n_installed]
        };
        if install_keys.insert(cand_ref.pkg.key.clone()) {
            changes.install.push(cand_ref.pkg.key.clone());
        }
        if let Some(repo) = &cand_ref.repo {
            let url = format!(
                "{}/{}",
                repo.base_uri.trim_end_matches('/'),
                cand_ref.repo_filename
            );
            if !repo.trusted {
                changes.unauthenticated.push(cand_ref.pkg.key.clone());
            }
            let fname = cand_ref
                .repo_filename
                .rsplit('/')
                .next()
                .unwrap_or("package.deb");
            downloads.push(DownloadSpec {
                name: cand_ref.pkg.key.clone(),
                version: cand_ref.pkg.version.clone(),
                url,
                dest: PathBuf::from(crate::download::CACHE_DIR).join(fname),
                sha256: cand_ref.sha256.clone(),
                size: cand_ref.size,
                trusted: repo.trusted,
            });
        }
    }
    for (i, pkg) in sys.installed.iter().enumerate() {
        // 被新候选替换(升级/重装)不算卸载
        if !selected.contains(&i) && !install_keys.contains(&pkg.key) {
            changes.remove.push(pkg.key.clone());
        }
    }
    changes.unauthenticated.sort();
    changes.unauthenticated.dedup();
    Verdict::Installable(Resolution { changes, downloads })
}

impl Encoding<'_> {
    fn plit_of(&self, u: Unit) -> Lit {
        let idx = match u {
            Unit::Installed(i) => i,
            Unit::Candidate(c) => self.n_installed + c,
        };
        self.plit(idx)
    }

    fn nlit_of(&self, u: Unit) -> Lit {
        let idx = match u {
            Unit::Installed(i) => i,
            Unit::Candidate(c) => self.n_installed + c,
        };
        self.nlit(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysstate::{Candidate, Pkg, Repo, SysState};

    fn pkg(key: &str, ver: &str) -> Pkg {
        let name = base_name(key).to_string();
        let arch = key
            .split_once(':')
            .map(|(_, a)| a.to_string())
            .unwrap_or_else(|| "amd64".into());
        Pkg {
            key: key.into(),
            name,
            arch,
            version: ver.into(),
            ..Default::default()
        }
    }

    fn cand(key: &str, ver: &str, trusted: bool) -> Candidate {
        Candidate {
            pkg: pkg(key, ver),
            repo: Some(Repo {
                trusted,
                base_uri: "http://repo".into(),
                ..Default::default()
            }),
            repo_filename: format!("{key}_{ver}.deb"),
            ..Default::default()
        }
    }

    fn sys_with(pkgs: Vec<Pkg>) -> SysState {
        let mut st = SysState {
            native_arch: "amd64".into(),
            ..Default::default()
        };
        for p in pkgs {
            let i = st.installed.len();
            st.index.entry(p.key.clone()).or_insert(i);
            st.index
                .entry(format!("{}:{}", p.name, p.arch))
                .or_insert(i);
            st.index.entry(p.name.clone()).or_insert(i);
            st.installed.push(p);
        }
        st
    }

    fn local(key: &str, ver: &str) -> Candidate {
        Candidate {
            pkg: pkg(key, ver),
            local_path: Some("/tmp/x.deb".into()),
            ..Default::default()
        }
    }

    fn dep(name: &str, oper: &str, ver: &str) -> Vec<Vec<Dep>> {
        vec![vec![Dep {
            name: name.into(),
            ver: ver.into(),
            oper: oper.into(),
        }]]
    }

    fn verdict_ok(v: Verdict) -> Resolution {
        match v {
            Verdict::Installable(r) => r,
            Verdict::Uninstallable(m) => panic!("expected installable, got: {m}"),
        }
    }

    fn assert_uninstallable(v: Verdict) -> String {
        match v {
            Verdict::Uninstallable(m) => m,
            Verdict::Installable(_) => panic!("expected uninstallable"),
        }
    }

    #[test]
    fn simple_dep_resolves() {
        let mut st = sys_with(vec![pkg("libc6", "2.39")]);
        st.add_candidates(vec![cand("libfoo", "1.0", true)]);
        let mut l = local("myapp", "2.0");
        l.pkg.depends = dep("libfoo", "", "");
        let r = verdict_ok(encode_and_solve(&st, l));
        assert!(r.changes.install.contains(&"libfoo".to_string()));
        assert!(r.changes.install.contains(&"myapp".to_string()));
        assert_eq!(r.downloads.len(), 1);
        assert_eq!(r.downloads[0].url, "http://repo/libfoo_1.0.deb");
        assert!(r.downloads[0].trusted);
    }

    #[test]
    fn or_group_picks_satisfiable_branch() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![cand("pkg-b", "3.0", true)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = vec![vec![
            Dep {
                name: "pkg-a".into(),
                ver: "2".into(),
                oper: ">=".into(),
            },
            Dep {
                name: "pkg-b".into(),
                ver: String::new(),
                oper: String::new(),
            },
        ]];
        let r = verdict_ok(encode_and_solve(&st, l));
        assert!(r.changes.install.contains(&"pkg-b".to_string()));
        assert!(!r.changes.install.contains(&"pkg-a".to_string()));
    }

    #[test]
    fn versioned_dep_prefers_compatible() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![cand("libx", "1.0", true), cand("libx", "2.0", true)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libx", "<<", "1.5");
        let r = verdict_ok(encode_and_solve(&st, l));
        assert_eq!(r.downloads[0].version, "1.0");
    }

    #[test]
    fn unsat_reports_uninstallable() {
        let st = sys_with(vec![]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("nonexistent", "", "");
        assert_uninstallable(encode_and_solve(&st, l));
    }

    #[test]
    fn conflict_forces_removal_of_non_essential() {
        let st = sys_with(vec![pkg("oldtool", "1.0")]);
        let mut l = local("newtool", "1.0");
        l.pkg.conflicts = dep("oldtool", "", "");
        let r = verdict_ok(encode_and_solve(&st, l));
        assert!(r.changes.remove.contains(&"oldtool".to_string()));
    }

    #[test]
    fn essential_never_removed() {
        let st = sys_with(vec![Pkg {
            essential: true,
            ..pkg("core", "1.0")
        }]);
        let mut l = local("rival", "1.0");
        l.pkg.conflicts = dep("core", "", "");
        assert_uninstallable(encode_and_solve(&st, l));
    }

    #[test]
    fn installed_package_satisfies_dep_without_download() {
        let st = sys_with(vec![pkg("libc6", "2.39")]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libc6", ">=", "2.30");
        let r = verdict_ok(encode_and_solve(&st, l));
        assert!(r.downloads.is_empty());
        // required_changes 仍含本地包自身(旧版 gdebi 语义)
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
    }

    #[test]
    fn provides_satisfies_dep_when_no_direct_hit() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![Candidate {
            pkg: Pkg {
                provides: vec!["www-browser".into()],
                ..pkg("w3m", "0.5.3")
            },
            ..cand("w3m", "0.5.3", true)
        }]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("www-browser", "", "");
        let r = verdict_ok(encode_and_solve(&st, l));
        assert!(r.changes.install.contains(&"w3m".to_string()));
    }

    #[test]
    fn unauth_repos_tracked() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![cand("libfoo", "1.0", false)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libfoo", "", "");
        let r = verdict_ok(encode_and_solve(&st, l));
        assert_eq!(r.changes.unauthenticated, vec!["libfoo".to_string()]);
        assert!(!r.downloads[0].trusted);
    }

    #[test]
    fn reinstall_same_version_not_reported_as_removal() {
        let st = sys_with(vec![pkg("myapp", "1.0")]);
        let r = verdict_ok(encode_and_solve(&st, local("myapp", "1.0")));
        assert!(r.changes.remove.is_empty());
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
    }

    #[test]
    fn ver_matches_operators() {
        assert!(ver_matches("2.39", ">=", "2.30"));
        assert!(!ver_matches("1.0", "<<", "1.0"));
        assert!(ver_matches("1.0", "=", "1.0"));
        assert!(ver_matches("1.0", ">", "0.9"));
    }
}
