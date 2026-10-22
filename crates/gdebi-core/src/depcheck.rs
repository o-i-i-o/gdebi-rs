//! 依赖检查。语义基准为旧版实际行为(python-apt `apt.debfile.DebPackage.check()`),
//! 系统状态经零 FFI 子进程获取:
//! - 已安装状态:`dpkg-query -W`(单次查询:名称/版本/状态/Depends/Conflicts/Provides)
//! - 依赖解析:`apt-get -s install --no-install-recommends`(与 apt CLI 同一解析器;
//!   失败输出同时解析 3.x 新求解器与 2.x 旧求解器两种格式)
//! - 源信任状态:`apt-get indextargets`(unauthenticated 判定)
//!
//! 与 python-apt 的已知语义边界(详见 diff-tests/README.md):
//! - or 组分支选择由 apt 解析器决定(python-apt 取首个可安装分支)
//! - 外部架构 deb 的 Multi-Arch foreign/same 细化规则未实现
//! - unauthenticated 按 Inst 行的源标记与 indextargets 尽力匹配(无未信任源的
//!   环境无法差分验证,取保守方向:匹配不到即视为未认证)
//!
//! 依赖 python-apt 的两个历史怪癖被有意保留(逐字节对齐 failure_string):
//! - `包名:all` 形式的降级检查照常生效(apt 缓存确有该键)
//! - 冲突/破坏类失败串无尾随换行,多条直接拼接;依赖类失败串自带 "\n"

use std::collections::BTreeMap;
use std::cmp::Ordering;
use std::path::Path;
use std::process::Command;

use crate::debfile::DebFile;
use crate::i18n::{gettext, tf};
use crate::version::cmp_version;
use crate::GdebiError;

/// apt-get 契约的变更集(对应旧版 required_changes 三元组,均按包名排序)
#[derive(Debug, Clone, Default)]
pub struct Changes {
    pub install: Vec<String>,
    pub remove: Vec<String>,
    pub unauthenticated: Vec<String>,
}

/// 依赖检查结论
#[derive(Debug)]
pub enum Verdict {
    Installable(Changes),
    /// 不可安装;failure_string 与旧版逐字节对齐(含内嵌换行怪癖)
    Uninstallable(String),
}

/// 依赖表达式的最小单元;name 可能带 :arch 后缀
struct Dep {
    name: String,
    ver: String,
    oper: String,
}

/// 已安装包(dpkg-query 单次查询的解析结果)
struct InstalledPkg {
    /// dpkg 的 binary:Package 键(原生/all 不带后缀,外部架构带 :arch),
    /// 与 apt 缓存键一致,排序后即 python-apt 的缓存迭代序
    key: String,
    /// 不带架构后缀的包名
    name: String,
    arch: String,
    version: String,
    depends_raw: String,
    conflicts_raw: String,
}

/// indextargets 条目(用于源信任判定)
#[derive(Default)]
struct IndexTarget {
    origin: String,
    label: String,
    suite: String,
    codename: String,
    trusted: bool,
}

pub struct DepChecker {
    native_arch: String,
    foreign_arches: Vec<String>,
    apt_opts: Vec<String>,
    /// 按 binary:Package 键排序的已安装包(与 python-apt 缓存迭代序一致)
    installed: Vec<InstalledPkg>,
    /// 包名 / 包名:架构 → installed 下标(仅收录已安装条目)
    index: BTreeMap<String, usize>,
    /// 已安装包的虚拟名 → 提供者包名
    providers: BTreeMap<String, Vec<String>>,
    trusted: Vec<IndexTarget>,
}

impl DepChecker {
    pub fn new(apt_opts: &[String]) -> Result<Self, GdebiError> {
        let native_arch = run_line(Command::new("dpkg").arg("--print-architecture"))?;
        let foreign_arches = run_line(Command::new("dpkg").arg("--print-foreign-architectures"))?
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        // 单次查询拿全已安装状态;db:Status-Abbrev 以 "ii" 开头 = 已安装
        let fmt = "${binary:Package}\t${Version}\t${db:Status-Abbrev}\t${Architecture}\t${Depends}\t${Conflicts}\t${Provides}\n";
        let out = run_line(
            Command::new("dpkg-query")
                .arg("-W")
                .arg(format!("-f={fmt}")),
        )?;

        let mut installed: Vec<InstalledPkg> = Vec::new();
        let mut providers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for line in out.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 7 || !f[2].starts_with("ii") {
                continue;
            }
            let key = f[0].to_string();
            let name = base_name(&key).to_string();
            for prov in provided_names(f[6]) {
                providers.entry(prov).or_default().push(name.clone());
            }
            installed.push(InstalledPkg {
                key,
                name,
                arch: f[3].to_string(),
                version: f[1].to_string(),
                depends_raw: f[4].to_string(),
                conflicts_raw: f[5].to_string(),
            });
        }
        installed.sort_by(|a, b| a.key.cmp(&b.key));

        let mut index = BTreeMap::new();
        for (i, pkg) in installed.iter().enumerate() {
            index.entry(pkg.key.clone()).or_insert(i);
            index
                .entry(format!("{}:{}", pkg.name, pkg.arch))
                .or_insert(i);
            index.entry(pkg.name.clone()).or_insert(i);
        }

        // 源信任状态只影响 unauthenticated 警告,拿不到时按"全部未认证"的
        // 保守方向降级,不阻塞可安装性判定
        let trusted = run_captured(Command::new("apt-get").arg("indextargets"))
            .map(|(out, _, code)| if code == Some(0) { parse_indextargets(&out) } else { Vec::new() })
            .unwrap_or_default();

        Ok(Self {
            native_arch,
            foreign_arches,
            apt_opts: apt_opts.to_vec(),
            installed,
            index,
            providers,
            trusted,
        })
    }

    /// 已安装包的当前版本(GUI 的同版本/重装/移除判定用)
    pub fn installed_version(&self, name: &str) -> Option<&str> {
        let &i = self.index.get(name)?;
        Some(self.installed[i].version.as_str())
    }

    /// 对标 python-apt `DebPackage.check()`:arch → 降级 → conflicts → breaks →
    /// 依赖解析(apt-get -s)→ 二次 conflicts → broken_count(由 apt-get 退出码承担)
    pub fn check(&self, deb: &DebFile, filename: &Path) -> Result<Verdict, GdebiError> {
        let pkgname_raw = field(deb, "Package");
        let version = field(deb, "Version");

        let arch = match deb.field_trimmed("Architecture") {
            Some(a) => a.to_string(),
            None => {
                return Ok(Verdict::Uninstallable(gettext(
                    "No Architecture field in the package",
                )))
            }
        };
        let mut pkgname = pkgname_raw.to_string();
        if arch != "all" && arch != self.native_arch {
            if self.foreign_arches.contains(&arch) {
                pkgname = format!("{}:{}", pkgname_raw, arch);
            } else {
                return Ok(Verdict::Uninstallable(tf(
                    "Wrong architecture '{0}' -- Run dpkg --add-architecture to \
                     add it and update afterwards",
                    &[&arch],
                )));
            }
        }

        // 降级检查:键为 包名:deb架构;与 apt 缓存一致,arch=all 同样命中
        if let Some(&i) = self.index.get(&format!("{}:{}", pkgname_raw, arch))
            && cmp_version(&self.installed[i].version, version) == Ordering::Greater
        {
            return Ok(Verdict::Uninstallable(gettext(
                "A later version is already installed",
            )));
        }

        let conflicts = parse_depends(field(deb, "Conflicts"));
        let replaces = parse_depends(field(deb, "Replaces"));
        // python-apt 只取每个 or 组的首名
        let provides: Vec<String> = parse_depends(field(deb, "Provides"))
            .iter()
            .filter_map(|g| g.first())
            .map(|d| d.name.clone())
            .collect();

        let mut failure = String::new();
        let no_marked: Vec<(String, String)> = Vec::new();
        let no_marked_provides: BTreeMap<String, Vec<String>> = BTreeMap::new();

        // 第一次冲突检查(尚无标记安装的包)
        if self.check_conflicts(
            &pkgname,
            &conflicts,
            &replaces,
            &no_marked,
            &no_marked_provides,
            &mut failure,
        ) {
            return Ok(Verdict::Uninstallable(failure));
        }
        // 安装本包是否破坏现有包
        if self.check_breaks(&pkgname, version, &arch, &provides, filename, &mut failure) {
            return Ok(Verdict::Uninstallable(failure));
        }

        // 依赖解析交给 apt-get -s(与 apt CLI 同一解析器);broken_count > 0
        // 的情形由其非零退出码承担
        let (stdout, stderr, code) = self.apt_get_simulate(filename)?;
        if code != Some(0) {
            // 仅取本包的不可满足依赖(python-apt 只报首个失败的 or 组);
            // 中间包破坏等其它失败对齐到 broken cache 文案
            let expr = parse_new_solver(&stderr, pkgname_raw).or_else(|| parse_old_solver(&stderr, pkgname_raw));
            match expr {
                Some(expr) => {
                    failure.push_str(&tf("Dependency is not satisfiable: {0}\n", &[expr.as_str()]))
                }
                None => failure.push_str(&gettext(
                    "Failed to satisfy all dependencies (broken cache)",
                )),
            }
            return Ok(Verdict::Uninstallable(failure));
        }

        let mut install: Vec<(String, String)> = Vec::new();
        let mut remove: Vec<String> = Vec::new();
        parse_simulate(&stdout, pkgname_raw, &mut install, &mut remove);

        // 第二次冲突检查:针对将被 apt 安装的依赖包(marked_install 语义)
        if !install.is_empty() && !conflicts.is_empty() {
            let marked_provides = self.marked_provides(&install)?;
            if self.check_conflicts(&pkgname, &conflicts, &replaces, &install, &marked_provides, &mut failure) {
                return Ok(Verdict::Uninstallable(failure));
            }
        }

        install.sort_by(|a, b| a.0.cmp(&b.0));
        remove.sort();
        let unauthenticated = install
            .iter()
            .filter(|(_, origin)| !self.trusted_origin(origin))
            .map(|(n, _)| n.clone())
            .collect();
        Ok(Verdict::Installable(Changes {
            install: install.into_iter().map(|(n, _)| n).collect(),
            remove,
            unauthenticated,
        }))
    }

    /// Conflicts 逐 or 组/逐项判定;命中时把失败串追加到 *failure(无尾随换行,
    /// 多条直接拼接,与 python-apt 一致),返回是否命中
    fn check_conflicts(
        &self,
        pkgname: &str,
        conflicts: &[Vec<Dep>],
        replaces: &[Vec<Dep>],
        marked: &[(String, String)],
        marked_provides: &BTreeMap<String, Vec<String>>,
        failure: &mut String,
    ) -> bool {
        let mut hit = false;
        for group in conflicts {
            for dep in group {
                let mut matched = false;
                // 真实包:已安装
                if let Some(&i) = self.index.get(dep.name.as_str()) {
                    let e = &self.installed[i];
                    hit |= self.conflict_hits(&e.name, &e.version, dep, replaces, failure);
                    matched = true;
                }
                // 真实包:本轮将被 apt 安装(marked_install 分支)
                if let Some((_, ver)) = marked.iter().find(|(n, _)| n == &dep.name) {
                    hit |= self.conflict_hits(&dep.name, ver, dep, replaces, failure);
                    matched = true;
                }
                if matched {
                    continue;
                }
                // 虚拟包:对每个已安装/将安装的提供者判定(自身除外)
                let prov_key = base_name(&dep.name);
                if let Some(ps) = self.providers.get(prov_key) {
                    for p in ps {
                        if p == pkgname {
                            continue;
                        }
                        if let Some(&i) = self.index.get(p.as_str()) {
                            let e = &self.installed[i];
                            hit |= self.conflict_hits(&e.name, &e.version, dep, replaces, failure);
                        }
                    }
                }
                if let Some(ps) = marked_provides.get(prov_key) {
                    for p in ps {
                        if p == pkgname {
                            continue;
                        }
                        if let Some((_, ver)) = marked.iter().find(|(n, _)| n == p) {
                            hit |= self.conflict_hits(p, ver, dep, replaces, failure);
                        }
                    }
                }
            }
        }
        hit
    }

    /// 单个冲突目标的判定:版本约束命中且未被 Replaces 豁免才算冲突
    fn conflict_hits(
        &self,
        name: &str,
        ver: &str,
        dep: &Dep,
        replaces: &[Vec<Dep>],
        failure: &mut String,
    ) -> bool {
        if check_dep(ver, &dep.oper, &dep.ver) && !replaces_real(replaces, name, ver) {
            failure.push_str(&tf("Conflicts with the installed package '{0}'", &[name]));
            true
        } else {
            false
        }
    }

    /// 对标 check_breaks_existing_packages():检出破坏返回 true
    fn check_breaks(
        &self,
        pkgname: &str,
        version: &str,
        arch: &str,
        provides: &[String],
        filename: &Path,
        failure: &mut String,
    ) -> bool {
        for pkg in &self.installed {
            // 1) 已安装包的 Depends 引用本包且本包版本不满足
            for group in parse_depends(&pkg.depends_raw) {
                for d in &group {
                    if d.name == pkgname && !check_dep(version, &d.oper, &d.ver) {
                        failure.push_str(&tf(
                            "Breaks existing package '{0}' dependency {1} ({2} {3})",
                            &[&pkg.name, &d.name, &d.oper, &d.ver],
                        ));
                        return true;
                    }
                }
            }
            // 2) 已安装包的 Conflicts 指向本包(apt 缓存条目架构须与 deb 架构
            //    一致:未限定名视为原生架构,因此对 arch=all 的 deb 永不命中)
            // 3) Conflicts 指向本包 Provides 的任一名字(任意版本约束都算)
            for group in parse_depends(&pkg.conflicts_raw) {
                for c in &group {
                    let target_arch = c
                        .name
                        .rsplit_once(':')
                        .map(|(_, a)| a)
                        .unwrap_or(&self.native_arch);
                    if c.name == pkgname
                        && target_arch == arch
                        && check_dep(version, &c.oper, &c.ver)
                    {
                        failure.push_str(&tf(
                            "Breaks existing package '{0}' conflict: {1} ({2} {3})",
                            &[&pkg.name, &c.name, &c.oper, &c.ver],
                        ));
                        return true;
                    }
                    if pkg.name != pkgname && provides.iter().any(|p| *p == base_name(&c.name)) {
                        failure.push_str(&tf(
                            "Breaks existing package '{0}' that conflict: '{1}'. \
                             But the '{2}' provides it via: '{3}'",
                            &[
                                &pkg.name,
                                &c.name,
                                &filename.display().to_string(),
                                &provides.join(","),
                            ],
                        ));
                        return true;
                    }
                }
            }
        }
        false
    }

    fn apt_get_simulate(&self, deb_path: &Path) -> Result<(String, String, Option<i32>), GdebiError> {
        let mut cmd = Command::new("apt-get");
        cmd.arg("-s").arg("--no-install-recommends");
        for o in &self.apt_opts {
            cmd.arg("-o").arg(o);
        }
        // apt 3.x 只把含 "/" 的参数按本地 deb 解析,相对路径会被当成包名
        cmd.arg("install").arg(std::path::absolute(deb_path)?);
        run_captured(&mut cmd)
    }

    /// 已标记安装包的 Provides(经 apt-cache show;供虚拟冲突目标的二次检查)
    fn marked_provides(
        &self,
        marked: &[(String, String)],
    ) -> Result<BTreeMap<String, Vec<String>>, GdebiError> {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, _) in marked {
            let (out, _, _) = run_captured(Command::new("apt-cache").arg("show").arg(name))?;
            let mut cur = String::new();
            for line in out.lines() {
                if let Some(rest) = line.strip_prefix("Provides:") {
                    if !cur.is_empty() {
                        for n in provided_names(&cur) {
                            map.entry(n).or_default().push(base_name(name).to_string());
                        }
                    }
                    cur = rest.trim().to_string();
                } else if !line.trim().is_empty() && line.starts_with(' ') && !cur.is_empty() {
                    // Provides 折行
                    cur.push(' ');
                    cur.push_str(line.trim());
                } else if !cur.is_empty() {
                    for n in provided_names(&cur) {
                        map.entry(n).or_default().push(base_name(name).to_string());
                    }
                    cur.clear();
                }
            }
            if !cur.is_empty() {
                for n in provided_names(&cur) {
                    map.entry(n).or_default().push(base_name(name).to_string());
                }
            }
        }
        Ok(map)
    }

    /// Inst 行源标记(如 "Ubuntu:26.10/stonking")与 indextargets 尽力匹配;
    /// 匹配不到视为未认证(保守方向)
    fn trusted_origin(&self, token: &str) -> bool {
        let Some((label, rest)) = token.split_once(':') else {
            return false;
        };
        let suite = rest.rsplit('/').next().unwrap_or(rest);
        self.trusted.iter().any(|t| {
            t.trusted
                && (t.origin == label || t.label == label)
                && (t.suite == suite || t.codename == suite)
        })
    }
}

/// 包名去掉 :arch 后缀
fn base_name(name: &str) -> &str {
    name.split(':').next().unwrap_or(name)
}

/// control 依赖字段 → or 组列表
fn parse_depends(s: &str) -> Vec<Vec<Dep>> {
    s.split(',')
        .filter(|g| !g.trim().is_empty())
        .map(|group| {
            group
                .split('|')
                .map(|d| {
                    let d = d.trim();
                    match d.find('(') {
                        Some(i) => {
                            let name = d[..i].trim().to_string();
                            let inner = d[i + 1..].trim_end_matches(')').trim();
                            let mut it = inner.splitn(2, char::is_whitespace);
                            let oper = it.next().unwrap_or("").to_string();
                            let ver = it.next().unwrap_or("").trim().to_string();
                            Dep { name, ver, oper }
                        }
                        None => Dep {
                            name: d.to_string(),
                            ver: String::new(),
                            oper: String::new(),
                        },
                    }
                })
                .collect()
        })
        .collect()
}

/// apt_pkg.check_dep 的直译(本机实测语义:'<'/'<<' 严格小于,'>'/'>>' 严格大于)
fn check_dep(ver: &str, oper: &str, ref_ver: &str) -> bool {
    let ord = cmp_version(ver, ref_ver);
    match oper {
        "" => true,
        "<" | "<<" => ord == Ordering::Less,
        "<=" => ord != Ordering::Greater,
        ">" | ">>" => ord == Ordering::Greater,
        ">=" => ord != Ordering::Less,
        "=" => ord == Ordering::Equal,
        _ => false,
    }
}

/// Replaces 是否豁免对 target 的冲突(python-apt replaces_real_pkg)
fn replaces_real(replaces: &[Vec<Dep>], target: &str, target_ver: &str) -> bool {
    replaces
        .iter()
        .flatten()
        .any(|r| r.name == target && check_dep(target_ver, &r.oper, &r.ver))
}

/// Provides 字段(可折行)→ 提供的虚拟名列表
fn provided_names(field: &str) -> Vec<String> {
    field
        .split(',')
        .filter_map(|e| {
            let e = e.trim();
            if e.is_empty() {
                return None;
            }
            let name = e.find('(').map(|i| &e[..i]).unwrap_or(e);
            let name = base_name(name.trim());
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// 解析 `apt-get -s` 的 Inst/Remv 行;deb 自身的 Inst 行被排除
/// (旧版 required_changes 只统计依赖标记,本包经 dpkg 安装)
fn parse_simulate(
    stdout: &str,
    skip: &str,
    install: &mut Vec<(String, String)>,
    remove: &mut Vec<String>,
) {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Inst ") {
            let mut it = rest.split_whitespace();
            let Some(name) = it.next() else { continue };
            if base_name(name) == base_name(skip) {
                continue;
            }
            // Inst <name> (<ver> [origin] [<arch>])
            let toks: Vec<&str> = it.collect();
            let n = toks.len();
            let origin = if n >= 3 && toks[n - 1].starts_with('[') {
                toks[n - 2]
            } else if n == 2 && !toks[1].starts_with('[') {
                toks[1]
            } else {
                ""
            };
            install.push((base_name(name).to_string(), origin.to_string()));
        } else if let Some(rest) = line.strip_prefix("Remv ")
            && let Some(name) = rest.split_whitespace().next()
        {
            remove.push(base_name(name).to_string());
        }
    }
}

/// apt 3.x 新求解器的失败输出:
///    2. <pkg>:<arch> Depends <expr>
///       but none of the choices are installable:
fn parse_new_solver(stderr: &str, pkgname: &str) -> Option<String> {
    let lines: Vec<&str> = stderr.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let t = t.split_once(". ").map(|(_, r)| r).unwrap_or(t);
        let Some((pkg_token, expr)) = t.split_once(" Depends ") else {
            continue;
        };
        if base_name(pkg_token) != pkgname {
            continue;
        }
        if lines.get(i + 1).is_some_and(|l| l.contains("but none of the choices")) {
            return Some(expr.trim().replace(" | ", "|"));
        }
    }
    None
}

/// apt 2.x 旧求解器的失败输出:
///  <pkg> : Depends: <expr> but it is not installable
///  <pkg> : Depends: a but it is not installable or
///                b but it is not installable
fn parse_old_solver(stderr: &str, pkgname: &str) -> Option<String> {
    let mut it = stderr.lines();
    for line in &mut it {
        if line.contains("unmet dependencies") {
            break;
        }
    }
    let mut pending: Vec<String> = Vec::new();
    let mut in_ours = false;
    for line in it {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with("E: ") {
            break;
        }
        if let Some((pkg_part, rest)) = t.split_once(" : ") {
            if base_name(pkg_part.trim()) == pkgname {
                in_ours = true;
                pending.clear();
                if let Some((frag, cont)) = old_fragment(rest) {
                    if cont {
                        pending.push(frag);
                    } else {
                        return Some(frag);
                    }
                }
            } else if in_ours {
                break;
            }
        } else if in_ours
            && let Some((frag, cont)) = old_fragment(t)
        {
            pending.push(frag);
            if !cont {
                return Some(pending.join("|"));
            }
        }
    }
    None
}

/// 提取旧求解器一行中的依赖表达式;返回 (表达式, 是否以 " or" 续接同组)
fn old_fragment(s: &str) -> Option<(String, bool)> {
    let cont = s.ends_with(" or");
    let body = if cont { &s[..s.len() - 3] } else { s };
    let idx = body.find(" but it is not installable")?;
    let mut frag = body[..idx].trim();
    if let Some(f) = frag.strip_prefix("Depends: ") {
        frag = f.trim();
    }
    Some((frag.trim().to_string(), cont))
}

fn parse_indextargets(out: &str) -> Vec<IndexTarget> {
    let mut targets = Vec::new();
    let mut cur = IndexTarget::default();
    let mut any = false;
    for line in out.lines() {
        if line.trim().is_empty() {
            if any {
                targets.push(std::mem::take(&mut cur));
                any = false;
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(": ") {
            any = true;
            match k {
                "Origin" => cur.origin = v.to_string(),
                "Label" => cur.label = v.to_string(),
                "Suite" => cur.suite = v.to_string(),
                "Codename" => cur.codename = v.to_string(),
                "Trusted" => cur.trusted = v.trim() == "yes",
                _ => {}
            }
        }
    }
    if any {
        targets.push(cur);
    }
    targets
}

fn field<'a>(deb: &'a DebFile, name: &str) -> &'a str {
    deb.field_trimmed(name).unwrap_or("")
}

/// 统一以 LC_ALL=C 运行子进程,保证输出可解析
fn run_captured(cmd: &mut Command) -> Result<(String, String, Option<i32>), GdebiError> {
    let out = cmd.env("LC_ALL", "C").output()?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

fn run_line(cmd: &mut Command) -> Result<String, GdebiError> {
    Ok(run_captured(cmd)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_depends_groups() {
        let gs = parse_depends("a (>= 1.0), b | c, d");
        assert_eq!(gs.len(), 3);
        assert_eq!(gs[0][0].name, "a");
        assert_eq!(gs[0][0].oper, ">=");
        assert_eq!(gs[0][0].ver, "1.0");
        assert_eq!(gs[1].len(), 2);
        assert_eq!(gs[1][1].name, "c");
        assert_eq!(gs[2][0].name, "d");
    }

    #[test]
    fn check_dep_relations() {
        // 本机 apt_pkg 实测语义
        assert!(check_dep("1.0", "", ""));
        assert!(check_dep("1.0", "=", "1.0"));
        assert!(!check_dep("1.0", "<", "1.0"));
        assert!(!check_dep("1.0", "<<", "1.0"));
        assert!(check_dep("1.0", "<=", "1.0"));
        assert!(check_dep("0.9", "<", "1.0"));
        assert!(!check_dep("1.0", ">", "1.0"));
        assert!(check_dep("1.1", ">", "1.0"));
        assert!(check_dep("1.0", ">=", "1.0"));
        assert!(!check_dep("1.0", "<=", "0.1"));
        assert!(check_dep("3.3.3", ">=", "0.1"));
    }

    #[test]
    fn simulate_inst_remv() {
        let stdout = "Inst sl (5.02-1build1 Ubuntu:26.10/stonking [amd64])\n\
                      Inst my-deb (1.0 local-deb [all])\n\
                      Remv jq [1.8.2-1ubuntu1]\n\
                      Conf sl (5.02-1build1 Ubuntu:26.10/stonking [amd64])\n";
        let mut install = Vec::new();
        let mut remove = Vec::new();
        parse_simulate(stdout, "my-deb", &mut install, &mut remove);
        assert_eq!(install, vec![("sl".to_string(), "Ubuntu:26.10/stonking".to_string())]);
        assert_eq!(remove, vec!["jq".to_string()]);
    }

    #[test]
    fn new_solver_failure() {
        let stderr = "E: Unable to satisfy dependencies. Reached two conflicting assignments:\n\
                      \x20  1. gdebi-test2:amd64=1.0 is selected for install\n\
                      \x20  2. gdebi-test2:amd64 Depends noexist-a | noexist-b\n\
                      \x20     but none of the choices are installable:\n\
                      \x20     [no choices]\n";
        assert_eq!(
            parse_new_solver(stderr, "gdebi-test2").as_deref(),
            Some("noexist-a|noexist-b")
        );
        assert_eq!(parse_new_solver(stderr, "other-pkg"), None);
    }

    #[test]
    fn old_solver_failure() {
        let stderr = "The following packages have unmet dependencies:\n\
                      \x20gdebi-test2 : Depends: pkg-x but it is not installable or\n\
                      \x20            pkg-y but it is not installable\n\
                      E: Unable to correct problems, you have held broken packages.\n";
        assert_eq!(
            parse_old_solver(stderr, "gdebi-test2").as_deref(),
            Some("pkg-x|pkg-y")
        );
    }

    #[test]
    fn indextargets_trust() {
        let out = "MetaKey: main/binary-amd64/Packages\n\
                   Origin: Ubuntu\n\
                   Suite: stonking\n\
                   Trusted: yes\n\
                   \n\
                   MetaKey: other/Packages\n\
                   Origin: Evil\n\
                   Suite: sid\n\
                   Trusted: no\n";
        let targets = parse_indextargets(out);
        assert_eq!(targets.len(), 2);
        let checker_trust = |token: &str, t: &[IndexTarget]| {
            let (label, rest) = match token.split_once(':') {
                Some(x) => x,
                None => return false,
            };
            let suite = rest.rsplit('/').next().unwrap_or(rest);
            t.iter().any(|tg| {
                tg.trusted
                    && (tg.origin == label || tg.label == label)
                    && (tg.suite == suite || tg.codename == suite)
            })
        };
        assert!(checker_trust("Ubuntu:26.10/stonking", &targets));
        assert!(!checker_trust("Evil:sid", &targets));
        assert!(!checker_trust("local-deb", &targets));
    }
}
