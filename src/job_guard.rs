//! 把子进程绑定到一个 Windows Job Object,设置 `KILL_ON_JOB_CLOSE`:不管主程序是正常退出、
//! 崩溃还是被任务管理器强杀,Windows 内核都会自动把绑定在这个 Job 里的所有子进程一起终止,
//! 不再留下孤儿进程——实测遇到过一次:主程序 panic 退出后,whisper-cli 子进程自己继续跑了
//! 半个多小时(GPU 占用、tmp 目录里堆着没人清理的临时文件)。这是操作系统级别的保证,不依赖
//! 我们自己的清理代码有没有机会执行——崩溃时恰恰是自己的代码来不及执行的时候。
//!
//! 命名 `job_guard` 而不是 `job`,是为了和已有的 `job.rs`(任务队列)区分开,避免混淆。

use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::sync::OnceLock;
use win32job::Job;

static CHILD_JOB: OnceLock<Job> = OnceLock::new();

/// 程序启动时调用一次。创建/配置失败(比如某些受限的沙箱环境不允许建 Job Object)不阻断
/// 程序运行,只是退化成没有这层防护——总比直接启动不起来好,后续 `bind_child_to_job` 在
/// 没有可用 Job 时也是静默跳过,不会报错。
pub fn init_child_job_object() {
    let Ok(job) = Job::create() else {
        return;
    };
    let mut info = job.query_extended_limit_info().unwrap_or_default();
    info.limit_kill_on_job_close();
    if job.set_extended_limit_info(&info).is_err() {
        return;
    }
    let _ = CHILD_JOB.set(job);
}

/// 每次 `Command::spawn()` 之后调用,把这个子进程加入 Job。加入失败(没有可用的 Job、或者
/// 子进程已经退出导致句柄失效)不算错误,只是这一个子进程没有这层防护,不影响它正常工作,
/// 静默忽略即可——这层防护本身就是"锦上添花"的兜底,不该成为新的失败点。
pub fn bind_child_to_job(child: &Child) {
    if let Some(job) = CHILD_JOB.get() {
        let handle = child.as_raw_handle() as isize;
        let _ = job.assign_process(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use win32job::Job;

    /// 真实验证 KILL_ON_JOB_CLOSE 这个机制本身有没有生效——不通过 `init_child_job_object`/
    /// `CHILD_JOB`(那是进程级单例,测试里没法反复创建/销毁来观察),而是直接用
    /// `win32job::Job` 自己建一个、绑定一个真实拉起的长命令、然后 drop 掉 Job,确认子进程
    /// 真的被杀了——这正是 subgen.exe 崩溃/被关时依赖的同一套机制,只是测试里用主动 drop
    /// 模拟"最后一个 Job 句柄被关闭"这个触发条件,而不是真的让测试进程自己崩溃。
    #[test]
    fn dropping_the_job_kills_bound_child_process() {
        let job = Job::create().expect("创建 Job Object 失败");
        let mut info = job.query_extended_limit_info().unwrap_or_default();
        info.limit_kill_on_job_close();
        job.set_extended_limit_info(&info)
            .expect("设置 KILL_ON_JOB_CLOSE 失败");

        // 拉一个会跑几十秒的真实子进程(不是立刻退出的),绑定到 Job 上。用 ping 而不是
        // timeout——测试环境的 PATH 里可能有别的工具(比如 Git 自带的 coreutils)抢在
        // Windows 系统自带的 timeout.exe 前面、参数格式不兼容,ping.exe 没有这个歧义。
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 31 127.0.0.1 >nul"])
            .spawn()
            .expect("无法拉起测试用的长命令");
        let handle = child.as_raw_handle() as isize;
        job.assign_process(handle).expect("绑定子进程到 Job 失败");

        // 绑定生效后子进程应该还活着(还没被杀)
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            child.try_wait().unwrap().is_none(),
            "子进程绑定后应该还在正常运行,不该立刻被杀"
        );

        // 关键一步:drop 掉 Job(模拟主程序进程退出时"最后一个 Job 句柄被关闭"),
        // 应该触发 KILL_ON_JOB_CLOSE,子进程被 Windows 内核自动终止。
        drop(job);

        std::thread::sleep(std::time::Duration::from_millis(500));
        let status = child.try_wait().expect("查询子进程状态失败");
        assert!(
            status.is_some(),
            "Job 被 drop 后,绑定的子进程应该已经被系统终止,而不是继续跑着"
        );

        // 防止测试环境里万一没杀成功,收尾兜底清理,不让 30 秒的 timeout 进程真的挂 30 秒
        let _ = child.kill();
        let _ = child.wait();
    }
}
