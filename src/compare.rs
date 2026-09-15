//! Beyond Compare：定位程序、按本机对比记录挑出该开哪一组、把差异窗口拉起来。

use crate::{Level, SvnApp};
use crate::bcompare;
use std::path::{Path, PathBuf};

impl SvnApp {
    pub fn reset_bc(&mut self) {
        for note in bcompare::reset_cache() {
            self.push(Level::Info, note);
        }
        self.hint("已重置 Beyond Compare（删除注册表 CacheID），重启 Beyond Compare 后生效，详情见输出记录");
    }

    /// 启动 Beyond Compare；targets 为空时只打开主窗口，传两个路径即为两路对比。
    /// 返回是否真的启动成功，方便调用方接着写自己的提示语。
    /// switches 是额外要交给 BC 的命令行开关，比如记录里的名称筛选 `/filters=...`。
    pub fn open_bcompare(&mut self, targets: &[PathBuf], switches: &[String]) -> bool {
        if !self.bc.is_file() {
            match bcompare::detect() {
                Some(found) => {
                    self.bc = found.clone();
                    self.cfg.bc_exe = found.to_string_lossy().into_owned();
                    self.persist();
                }
                None => {
                    self.hint("未找到 Beyond Compare（BCompare.exe），可在「设置 → Beyond Compare」中指定路径");
                    self.show_settings = true;
                    return false;
                }
            }
        }
        // 提前检查目标路径是否存在，避免 BC 打开后报「加载失败」
        for path in targets {
            if !path.exists() {
                let msg = format!("对比路径不存在或无法访问：{}", path.display());
                self.push(Level::Warning, msg.clone());
                self.hint(msg);
                return false;
            }
        }
        let paths: Vec<&Path> = targets.iter().map(|path| path.as_path()).collect();
        match bcompare::launch(&self.bc, &paths, switches) {
            Ok(()) => {
                // 日志按「开关在前、路径在后」显示，与真正传给 BC 的参数顺序一致
                let mut shown: Vec<String> = switches
                    .iter()
                    .map(|s| if s.contains(' ') { format!("\"{}\"", s) } else { s.clone() })
                    .collect();
                shown.extend(targets.iter().map(|path| format!("\"{}\"", path.display())));
                self.push(
                    Level::Command,
                    format!("$ \"{}\" {}", self.bc.display(), shown.join(" ")),
                );
                self.hint("已启动 Beyond Compare");
                true
            }
            Err(e) => {
                self.hint(e);
                false
            }
        }
    }

    /// 顶部「Beyond Compare」按钮：先拿「更多 → 绑定对比对象」里绑定的目录去 BC 的对比记录
    /// （BCSessions.xml）里找对应的那条，没绑定就按当前目录的文件夹名猜；命中就把记录的
    /// 名称筛选交给 BC——BC 收到路径只会按程序默认设置新建一次比较，不会自己去读记录。
    /// 服务器端（所选工作副本目录）放哪一侧由设置的 bc_server_side 决定；没绑定时按路径名
    /// 里的 server 关键字认记录两侧，认不出就沿用记录原有的左右顺序。
    /// 绑定了但记录里没有这条时，就直接按绑定的两个目录开一次比较；两者都没有则打开主窗口。
    pub fn open_bcompare_matched(&mut self) {
        let Some(index) = self.selected else {
            self.hint("尚未选中目录，已直接打开 Beyond Compare 主窗口");
            self.open_bcompare(&[], &[]);
            return;
        };
        let Some(dir) = self.dir_path(index) else {
            self.hint("选中目录的路径已失效，已直接打开 Beyond Compare 主窗口");
            self.open_bcompare(&[], &[]);
            return;
        };
        let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bound = self.cfg.dirs[index].bc_target.trim_matches('"').to_owned();
        let here = dir.to_string_lossy().trim_end_matches(['\\', '/']).to_owned();
        let there = bound.trim_end_matches(['\\', '/']).to_owned();
        let sessions = bcompare::sessions();
        let session = if bound.is_empty() {
            bcompare::match_session(&sessions, &dir)
        } else {
            // 绑定过就只认「左右正好是这两个目录」的那条记录：既能拿到记录里的名称筛选，
            // 也好对号入座认出哪一侧是服务器端。同一对路径 BC 常常自动存出好几条，
            // 优先挑带名称筛选的那条，别退回到命令行开出来的那条空记录
            let mut hit: Option<&bcompare::BcSession> = None;
            for record in &sessions {
                let same = (record.left.eq_ignore_ascii_case(&there) && record.right.eq_ignore_ascii_case(&here))
                    || (record.left.eq_ignore_ascii_case(&here) && record.right.eq_ignore_ascii_case(&there));
                if same && hit.map_or(true, |saved| saved.filter.is_empty() && !record.filter.is_empty()) {
                    hit = Some(record);
                }
            }
            hit
        };
        if session.is_none() && bound.is_empty() {
            self.hint(format!(
                "Beyond Compare 的 {} 条对比记录里没有匹配「{folder}」的，且没绑定对比对象；\
                 请在「更多 → 绑定对比对象」里指定另一侧，之后就不依赖本机的 BC 记录了",
                sessions.len()
            ));
            self.open_bcompare(&[], &[]);
            return;
        }
        let label = self.dir_label(index);
        // 四个值：左侧、右侧、记录里的名称筛选、记录里认出的对比对象（另一侧）。
        // 最后一项只在「没绑定对比对象」时才有，用来回填进配置，之后不依赖本机 BC 记录。
        let (left, right, record_filter, record_target) = match session {
            Some(session) => {
                self.push(
                    Level::Info,
                    format!(
                        "目录「{label}」匹配到对比记录：{}（{}），名称筛选：{}",
                        session.name,
                        if session.folder { "文件夹对比" } else { "文件对比" },
                        if session.filter.is_empty() { "无" } else { session.filter.as_str() }
                    ),
                );
                // 服务器端 = 所选的工作副本目录（xxx_server），本地端 = 绑定的对比对象。
                // 先从记录两侧认出服务器端：绑定时按 here/there 对号；
                // 没绑定就按路径名里的 server 关键字认，两侧都认不出时沿用记录原顺序
                let server = if !bound.is_empty() {
                    if session.left.eq_ignore_ascii_case(&here) {
                        Some(session.left.as_str())
                    } else if session.right.eq_ignore_ascii_case(&here) {
                        Some(session.right.as_str())
                    } else {
                        None
                    }
                } else {
                    let (a, b) = (
                        bcompare::looks_like_server(&session.left),
                        bcompare::looks_like_server(&session.right),
                    );
                    if a != b {
                        Some(if a { session.left.as_str() } else { session.right.as_str() })
                    } else {
                        None
                    }
                };
                let (lr, rr) = match server {
                    Some(server) => {
                        let local = if server == session.left.as_str() {
                            session.right.as_str()
                        } else {
                            session.left.as_str()
                        };
                        bcompare::order_sides(
                            Path::new(server),
                            Path::new(local),
                            &self.cfg.bc_server_side,
                        )
                    }
                    None => (
                        PathBuf::from(&session.left),
                        PathBuf::from(&session.right),
                    ),
                };
                // 只有当记录某一侧「正好就是」当前工作副本目录时，另一侧才是可靠的对比对象。
                // 记录可能是按文件夹名蒙中的（路径其实不一样），这时别回填，免得把目录绑成它自己
                let target = if session.left.eq_ignore_ascii_case(&here) {
                    Some(session.right.clone())
                } else if session.right.eq_ignore_ascii_case(&here) {
                    Some(session.left.clone())
                } else {
                    None
                };
                (
                    lr.display().to_string(),
                    rr.display().to_string(),
                    session.filter.clone(),
                    target,
                )
            }
            None => {
                // 所选工作副本目录是服务器端，绑定的对比对象是本地端，按设置排左右
                let (l, r) = bcompare::order_sides(&dir, Path::new(&bound), &self.cfg.bc_server_side);
                let (left, right) = (l.display().to_string(), r.display().to_string());
                self.push(
                    Level::Info,
                    format!("目录「{label}」按绑定的对比对象打开：{left} <--> {right}（BC 记录里没有这条，名称筛选走本程序配置）"),
                );
                (left, right, String::new(), None)
            }
        };
        // 把 BC 记录里认出来的东西回填进我们自己的配置：本机用户在 BC 里存过这条会话，
        // 记录里才有；其他用户机器上没有这条记录就读不到，于是既带不出筛选也认不出对比对象。
        // 回填后不再依赖本机 BC 记录，换机器、换人都能一致（配置可随项目分发）。
        let mut dirty = false;
        if self.cfg.dirs[index].bc_filter.is_empty() && !record_filter.is_empty() {
            self.cfg.dirs[index].bc_filter = record_filter.clone();
            dirty = true;
        }
        // 只在没手动绑定过时才回填对比对象，免得悄悄覆盖用户自己的选择
        if self.cfg.dirs[index].bc_target.is_empty() {
            if let Some(target) = record_target {
                if !target.is_empty() {
                    self.cfg.dirs[index].bc_target = target;
                    dirty = true;
                }
            }
        }
        if dirty {
            self.persist();
        }
        // 我们自己的配置优先（用户可在「更多」里直接填），没有时才退回 BC 记录的值
        let filter = if self.cfg.dirs[index].bc_filter.is_empty() {
            record_filter
        } else {
            self.cfg.dirs[index].bc_filter.clone()
        };
        let mut switches: Vec<String> = Vec::new();
        if !filter.is_empty() {
            switches.push(format!("/filters={filter}"));
        }
        self.open_bcompare(&[PathBuf::from(&left), PathBuf::from(&right)], &switches);
    }

    /// 用 Beyond Compare 对比单个文件的「BASE 版本 ↔ 本地版本」。
    pub fn open_bcompare_file(&mut self, index: usize, path: String) {
        let (Some(svn), Some(root)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        let target = PathBuf::from(&path);
        // 临时目录里保留工作副本内的相对路径，BC 两侧的标题更好对应；
        // 拿不到相对路径时直接放弃，避免误把 BASE 写回工作副本
        let Ok(relative) = target.strip_prefix(&root) else {
            self.hint("无法确定该文件在工作副本中的相对路径，已放弃对比");
            return;
        };
        let mirror = std::env::temp_dir()
            .join("SVNManager")
            .join("BASE")
            .join(relative);
        if let Some(parent) = mirror.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match svn.cat_base(&target, &mirror) {
            Ok(()) => {
                // 服务器端（BASE 版本）放哪一侧由设置决定，默认在左
                let (left, right) = bcompare::order_sides(&mirror, &target, &self.cfg.bc_server_side);
                self.open_bcompare(&[left, right], &[]);
            }
            Err(e) => {
                // 新增 / 未版本化的条目没有 BASE 版本，直接在 BC 里打开本地文件
                self.push(Level::Warning, format!("$ svn cat -r BASE \"{path}\" 失败：{e}"));
                self.open_bcompare(&[target], &[]);
            }
        }
    }
}
