use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use crate::svn::{LogEntry, StatusEntry, WcInfo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    DetectSvn,
    AuthUser,
    Refresh,
    Status,
    Log,
    /// 某次提交对单个文件 / 目录的逐行差异（双击历史页的涉及文件）
    FileDiff,
    /// 单个文件 / 目录自己的提交记录（点涉及文件行右侧的「查看提交记录」）
    FileLog,
    /// 检查版本更新（读服务端 latest.json）
    CheckUpdate,
    /// AI 生成工作日志（把勾选的提交记录发给 AI，整块结果一次性回传，dir 恒为 usize::MAX）
    AiLog,
    /// 个人提交统计：按目录各起一个读日志的任务，一次查询同时有几个在跑。
    /// 池里的 dir 恒为 usize::MAX：`is_busy` 只比目录编号不比种类，用真实编号会把那个
    /// 目录的「提交 / 更新 / relocate」按钮一起按住，而统计只是读数据，不该妨碍干活。
    /// 真实目录编号在 `Data::Stats.dir` 里带回来。
    Stats,
    /// 下载新版本 exe 到临时目录（下载与校验都在程序内完成）
    DownloadUpdate,
    Update,
    Commit,
    Maintain,
    Relocate,
    Diff,
}

/// 后台任务回传给界面的结果。
pub enum Data {
    Svn {
        exe: String,
        version: String,
        candidates: Vec<String>,
    },
    /// 本机 svn 登录人（用于提交记录默认只看自己的提交）
    User {
        user: String,
    },
    /// 版本更新检查结果（ok=false 时 info 为空且 message 是原因）
    UpdateCheck {
        ok: bool,
        message: String,
        info: Option<crate::update::UpdateManifest>,
    },
    /// 新版本下载结果（ok=false 时 message 是原因；bytes 是下载字节数）
    UpdateDownloaded {
        ok: bool,
        message: String,
        bytes: u64,
    },
    /// AI 生成工作日志结果（ok=false 时 message 是失败原因，content 为空）
    AiLog {
        ok: bool,
        content: String,
        message: String,
    },
    /// 个人提交统计：一个目录在指定区间内的本人提交记录。
    /// `epoch` 是发起这次查询时代页面上的代号，对不上说明用户中途换了条件，结果直接丢。
    Stats {
        dir: usize,
        epoch: u64,
        entries: Vec<LogEntry>,
        ok: bool,
        message: String,
    },
    Wc {
        dir: usize,
        info: Option<WcInfo>,
        remote: Option<bool>,
        remote_msg: String,
        remote_rev: String,
        last_rev: String,
        last_author: String,
        last_date: String,
        changed: Option<usize>,
        /// 会进「全部上传」的变动明细（含 ? 自动 add、! 自动 delete 的条目）
        changes: Vec<StatusEntry>,
        /// 需要人工处理的条目数（冲突 / 不完整），按**未过滤**的 status 结果统计：
        /// `changed` / `changes` 只留可提交的条目，冲突会被滤掉，不另外数就没人知道自己卡住了
        conflicts: Option<usize>,
        /// 服务器上已变、本地还没更新的文件数（`svn status -u`）
        out_of_date: Option<usize>,
    },
    Status {
        dir: usize,
        entries: Vec<StatusEntry>,
        ok: bool,
        message: String,
    },
    Log {
        dir: usize,
        entries: Vec<LogEntry>,
        ok: bool,
        message: String,
    },
    Run {
        dir: usize,
        ok: bool,
        message: String,
        reload: bool,
    },
}

enum Msg {
    Line(String),
    Done(Data),
}

/// 传给工作线程的输出通道，svn 的每行输出实时上报界面。
#[derive(Clone)]
pub struct Sink {
    tx: Sender<Msg>,
    quiet: bool,
}

impl Sink {
    fn new(tx: Sender<Msg>) -> Self {
        Self { tx, quiet: false }
    }

    pub fn line(&self, text: impl Into<String>) {
        if self.quiet {
            return;
        }
        let _ = self.tx.send(Msg::Line(text.into()));
    }

    /// 静音副本：之后的所有输出行都被丢弃。
    /// 用于程序自己补跑的读操作（提交成功后的自动刷新等）：`$ svn …` 的执行过程
    /// 刷进输出区只会淹没用户真正关心的提交结果，失败了也有 Done 消息兜底报错。
    pub fn muted(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            quiet: true,
        }
    }
}

struct Task {
    kind: Kind,
    label: String,
    dir: usize,
    rx: Receiver<Msg>,
    handle: Option<JoinHandle<()>>,
}

pub struct Polled {
    pub lines: Vec<String>,
    pub done: Vec<(Kind, usize, Data)>,
    /// 线程挂了但没回传结果的任务（svn 线程 panic / 起线程失败）：
    /// 不报出来的话界面只会安静地不动，看不出任何原因。
    pub stranded: Vec<(Kind, usize, String)>,
}

#[derive(Default)]
pub struct Pool {
    tasks: Vec<Task>,
}

impl Pool {
    pub fn spawn<F>(&mut self, kind: Kind, dir: usize, label: String, work: F)
    where
        F: FnOnce(Sink) -> Data + Send + 'static,
    {
        let (tx, rx) = channel();
        let sink = Sink::new(tx.clone());
        let handle = std::thread::spawn(move || {
            let data = work(sink);
            let _ = tx.send(Msg::Done(data));
        });
        self.tasks.push(Task {
            kind,
            label,
            dir,
            rx,
            handle: Some(handle),
        });
    }

    pub fn running(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_busy(&self, dir: usize) -> bool {
        self.tasks.iter().any(|task| task.dir == dir)
    }

    /// 该目录是否有「会改动工作副本」的任务在跑。
    /// 读类任务（status / log / diff / 检测）不算：它们只是慢一点，不应该让提交按钮失效。
    pub fn is_writing(&self, dir: usize) -> bool {
        self.tasks.iter().any(|task| {
            task.dir == dir
                && matches!(
                    task.kind,
                    Kind::Update | Kind::Commit | Kind::Maintain | Kind::Relocate
                )
        })
    }

    pub fn busy_label(&self, dir: usize) -> Option<String> {
        self.tasks
            .iter()
            .find(|task| task.dir == dir)
            .map(|task| task.label.clone())
    }

    /// 取出已产生的输出与已完成的任务。
    pub fn poll(&mut self) -> Polled {
        let mut lines = Vec::new();
        let mut done = Vec::new();
        let mut stranded = Vec::new();
        let mut finished: Vec<usize> = Vec::new();
        for (index, task) in self.tasks.iter_mut().enumerate() {
            loop {
                match task.rx.try_recv() {
                    Ok(Msg::Line(text)) => lines.push(text),
                    Ok(Msg::Done(data)) => {
                        done.push((task.kind, task.dir, data));
                        finished.push(index);
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        stranded.push((task.kind, task.dir, task.label.clone()));
                        finished.push(index);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
        }
        for index in finished.into_iter().rev() {
            if let Some(task) = self.tasks.get_mut(index) {
                if let Some(handle) = task.handle.take() {
                    std::mem::drop(handle);
                }
            }
            self.tasks.remove(index);
        }
        Polled {
            lines,
            done,
            stranded,
        }
    }

    pub fn has(&self, kind: Kind, dir: usize) -> bool {
        self.tasks.iter().any(|t| t.kind == kind && t.dir == dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Data {
        Data::Run {
            dir: 3,
            ok: true,
            message: String::new(),
            reload: false,
        }
    }

    /// 点文件自动看差异会起一个 Diff 任务，它绝不能让「提交」按钮失效。
    #[test]
    fn read_only_tasks_are_not_writing() {
        let mut pool = Pool::default();
        pool.spawn(Kind::Diff, 3, "查看差异".into(), |_| data());
        pool.spawn(Kind::Refresh, 3, "检测".into(), |_| data());
        assert!(pool.is_busy(3), "is_busy 仍包含读类任务");
        assert!(!pool.is_writing(3), "读类任务不算写操作");
        pool.spawn(Kind::Commit, 3, "提交".into(), |_| data());
        assert!(pool.is_writing(3));
        assert!(pool.is_writing(3), "重复调用结果应一致");
        assert!(!pool.is_writing(4), "别的目录不受影响");
    }

    /// 后台线程 panic 时通道会断开：必须回报，否则页面标志位再也清不掉（点了没反应）。
    #[test]
    fn dead_worker_is_reported_stranded() {
        let mut pool = Pool::default();
        pool.spawn(Kind::Status, 7, "读取修改".into(), |_| {
            panic!("模拟 svn 线程崩溃")
        });
        let mut found = Vec::new();
        for _ in 0..400 {
            let polled = pool.poll();
            if !polled.stranded.is_empty() {
                found = polled.stranded;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(found.len(), 1, "崩溃的任务没有被回报");
        assert_eq!(found[0].0, Kind::Status);
        assert_eq!(found[0].1, 7);
        assert_eq!(found[0].2, "读取修改");
        assert!(!pool.has(Kind::Status, 7), "任务应已从池中移除");
    }
}